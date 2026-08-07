use std::fs::{File, OpenOptions};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::time::Instant;

use crate::error::{Error, Result};
use crate::segment::{KIND_PUT, KIND_TOMBSTONE};
use crate::value::{read_u16, read_u32, read_u64, read_u8};

pub(crate) const WAL_DIR: &str = "wal";

/// How aggressively commits are forced to stable storage.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Durability {
    /// fsync the WAL on every commit. Slowest, survives OS crash.
    Safe,
    /// fsync at most every `balanced_sync_interval_ms`. An OS crash can lose
    /// the last few commits; a process crash loses nothing.
    Balanced,
    /// Never fsync explicitly outside checkpoints. An OS crash can lose
    /// recent commits; a process crash loses nothing.
    Fast,
}

pub(crate) fn wal_file_name(id: u32) -> String {
    format!("{id:06}.wal")
}

pub(crate) fn wal_path(dir: &Path, id: u32) -> PathBuf {
    dir.join(WAL_DIR).join(wal_file_name(id))
}

// Commit record layout (all integers little-endian):
//
//   u64  commit_version
//   u32  change_count
//   per change:
//     u8   kind          KIND_PUT | KIND_TOMBSTONE
//     u16  table_len     + table name bytes
//     u16  id_len        + id bytes
//     u32  payload_len   + payload bytes (empty for tombstones)
//   u32  crc32           over every preceding byte of the record
//
// A record is only applied on replay if it parses completely and its CRC
// matches; a torn tail is truncated. Replay is idempotent because records
// at or below the manifest's committed_version are skipped.

pub(crate) struct WalChange {
    pub table: String,
    pub id: String,
    /// `None` is a tombstone.
    pub payload: Option<Vec<u8>>,
}

pub(crate) struct WalRecord {
    pub version: u64,
    pub changes: Vec<WalChange>,
}

pub(crate) fn encode_commit(version: u64, changes: &[(&str, &str, Option<&[u8]>)]) -> Vec<u8> {
    let mut buf = Vec::with_capacity(128);
    buf.extend_from_slice(&version.to_le_bytes());
    buf.extend_from_slice(&(changes.len() as u32).to_le_bytes());
    for (table, id, payload) in changes {
        buf.push(if payload.is_some() { KIND_PUT } else { KIND_TOMBSTONE });
        buf.extend_from_slice(&(table.len() as u16).to_le_bytes());
        buf.extend_from_slice(table.as_bytes());
        buf.extend_from_slice(&(id.len() as u16).to_le_bytes());
        buf.extend_from_slice(id.as_bytes());
        let p = payload.unwrap_or(&[]);
        buf.extend_from_slice(&(p.len() as u32).to_le_bytes());
        buf.extend_from_slice(p);
    }
    let crc = crc32fast::hash(&buf);
    buf.extend_from_slice(&crc.to_le_bytes());
    buf
}

pub(crate) struct WalScan {
    pub records: Vec<WalRecord>,
    pub valid_len: u64,
    pub clean: bool,
}

/// Scan a WAL buffer, stopping at the first torn or corrupt record.
pub(crate) fn scan_wal(data: &[u8]) -> WalScan {
    let mut records = Vec::new();
    let mut pos = 0usize;
    loop {
        let start = pos;
        if pos >= data.len() {
            return WalScan {
                records,
                valid_len: start as u64,
                clean: true,
            };
        }
        match parse_record(data, &mut pos, start) {
            Ok(r) => records.push(r),
            Err(_) => {
                return WalScan {
                    records,
                    valid_len: start as u64,
                    clean: false,
                }
            }
        }
    }
}

fn parse_record(data: &[u8], pos: &mut usize, start: usize) -> Result<WalRecord> {
    let version = read_u64(data, pos)?;
    let count = read_u32(data, pos)? as usize;
    // Bounds-checked reads keep a garbage count from allocating; this cap is
    // an extra sanity guard against absurd-but-parseable values.
    if count as u64 > data.len() as u64 {
        return Err(Error::Corrupt("wal: implausible change count".into()));
    }
    let mut changes = Vec::with_capacity(count.min(1024));
    for _ in 0..count {
        let kind = read_u8(data, pos)?;
        if kind != KIND_PUT && kind != KIND_TOMBSTONE {
            return Err(Error::Corrupt(format!("wal: unknown change kind {kind}")));
        }
        let table = read_short_str(data, pos)?;
        let id = read_short_str(data, pos)?;
        let payload_len = read_u32(data, pos)? as usize;
        let end = pos
            .checked_add(payload_len)
            .ok_or_else(|| Error::Corrupt("wal: length overflow".into()))?;
        let payload_bytes = data
            .get(*pos..end)
            .ok_or_else(|| Error::Corrupt("wal: truncated payload".into()))?;
        *pos = end;
        if kind == KIND_TOMBSTONE && payload_len != 0 {
            return Err(Error::Corrupt("wal: tombstone with payload".into()));
        }
        changes.push(WalChange {
            table,
            id,
            payload: (kind == KIND_PUT).then(|| payload_bytes.to_vec()),
        });
    }
    let crc_pos = *pos;
    let stored_crc = read_u32(data, pos)?;
    if crc32fast::hash(&data[start..crc_pos]) != stored_crc {
        return Err(Error::Corrupt("wal: record crc mismatch".into()));
    }
    Ok(WalRecord { version, changes })
}

fn read_short_str(data: &[u8], pos: &mut usize) -> Result<String> {
    let len = read_u16(data, pos)? as usize;
    let end = pos
        .checked_add(len)
        .ok_or_else(|| Error::Corrupt("wal: length overflow".into()))?;
    let slice = data
        .get(*pos..end)
        .ok_or_else(|| Error::Corrupt("wal: unexpected end".into()))?;
    *pos = end;
    std::str::from_utf8(slice)
        .map(|s| s.to_owned())
        .map_err(|_| Error::Corrupt("wal: invalid utf8".into()))
}

pub(crate) struct WalWriter {
    pub id: u32,
    file: File,
    pub len: u64,
    last_sync: Instant,
}

impl WalWriter {
    /// Open (creating if missing) the WAL file with the given id for appends.
    pub fn open(dir: &Path, id: u32) -> Result<WalWriter> {
        let path = wal_path(dir, id);
        let file = OpenOptions::new().create(true).append(true).open(&path)?;
        let len = file.metadata()?.len();
        Ok(WalWriter {
            id,
            file,
            len,
            last_sync: Instant::now(),
        })
    }

    pub fn append_commit(
        &mut self,
        bytes: &[u8],
        durability: Durability,
        balanced_interval_ms: u64,
    ) -> Result<()> {
        self.file.write_all(bytes)?;
        self.len += bytes.len() as u64;
        match durability {
            Durability::Safe => {
                self.file.sync_data()?;
            }
            Durability::Balanced => {
                if self.last_sync.elapsed().as_millis() as u64 >= balanced_interval_ms {
                    self.file.sync_data()?;
                    self.last_sync = Instant::now();
                }
            }
            Durability::Fast => {}
        }
        Ok(())
    }
}
