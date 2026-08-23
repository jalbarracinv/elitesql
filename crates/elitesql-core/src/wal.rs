use std::fs::{File, OpenOptions};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::time::Instant;

use crate::error::{Error, Result};
use crate::segment::{KIND_PUT, KIND_TOMBSTONE};
use crate::value::{read_u16, read_u32, read_u64, read_u8};

pub(crate) const WAL_DIR: &str = "wal";
const IDENTITY_META_TABLE: &str = "\0elitesql_identity";

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
    pub identity_high_water: Vec<(String, i64)>,
}

pub(crate) fn encode_commit(
    version: u64,
    changes: &[(&str, &str, Option<&[u8]>)],
    identity_high_water: &[(&str, i64)],
) -> Result<Vec<u8>> {
    let mut buf = Vec::with_capacity(128);
    buf.extend_from_slice(&version.to_le_bytes());
    let count = changes
        .len()
        .checked_add(identity_high_water.len())
        .ok_or_else(|| Error::InvalidArgument("WAL change count overflow".into()))?;
    let count = u32::try_from(count)
        .map_err(|_| Error::InvalidArgument("WAL has more than u32::MAX changes".into()))?;
    buf.extend_from_slice(&count.to_le_bytes());
    for (table, id, payload) in changes {
        buf.push(if payload.is_some() {
            KIND_PUT
        } else {
            KIND_TOMBSTONE
        });
        let table_len = u16::try_from(table.len()).map_err(|_| {
            Error::InvalidArgument("table name exceeds the 65535-byte storage limit".into())
        })?;
        buf.extend_from_slice(&table_len.to_le_bytes());
        buf.extend_from_slice(table.as_bytes());
        let id_len = u16::try_from(id.len()).map_err(|_| {
            Error::InvalidArgument("record id exceeds the 65535-byte storage limit".into())
        })?;
        buf.extend_from_slice(&id_len.to_le_bytes());
        buf.extend_from_slice(id.as_bytes());
        let p = payload.unwrap_or(&[]);
        let payload_len = u32::try_from(p.len()).map_err(|_| {
            Error::InvalidArgument("WAL payload exceeds the 4-GiB storage limit".into())
        })?;
        buf.extend_from_slice(&payload_len.to_le_bytes());
        buf.extend_from_slice(p);
    }
    for (table, value) in identity_high_water {
        buf.push(KIND_PUT);
        buf.extend_from_slice(&(IDENTITY_META_TABLE.len() as u16).to_le_bytes());
        buf.extend_from_slice(IDENTITY_META_TABLE.as_bytes());
        let table_len = u16::try_from(table.len()).map_err(|_| {
            Error::InvalidArgument("table name exceeds the 65535-byte storage limit".into())
        })?;
        buf.extend_from_slice(&table_len.to_le_bytes());
        buf.extend_from_slice(table.as_bytes());
        buf.extend_from_slice(&(8u32).to_le_bytes());
        buf.extend_from_slice(&value.to_le_bytes());
    }
    let crc = crc32fast::hash(&buf);
    buf.extend_from_slice(&crc.to_le_bytes());
    Ok(buf)
}

/// Assign the serialized version after a WAL record was prepared. Updating
/// the short header and CRC is cheaper than re-encoding every payload while
/// holding the global commit mutex.
pub(crate) fn set_encoded_commit_version(record: &mut [u8], version: u64) {
    assert!(
        record.len() >= 12,
        "an encoded WAL commit includes a header and CRC"
    );
    record[..8].copy_from_slice(&version.to_le_bytes());
    let crc_offset = record.len() - 4;
    let crc = crc32fast::hash(&record[..crc_offset]);
    record[crc_offset..].copy_from_slice(&crc.to_le_bytes());
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
    let mut previous_version: Option<u64> = None;
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
            Ok(r) if previous_version.is_none_or(|previous| r.version > previous) => {
                previous_version = Some(r.version);
                records.push(r)
            }
            Ok(_) => {
                return WalScan {
                    records,
                    valid_len: start as u64,
                    clean: false,
                }
            }
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
    let mut identity_high_water = Vec::new();
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
        if table == IDENTITY_META_TABLE {
            if kind != KIND_PUT || payload_len != 8 {
                return Err(Error::Corrupt("wal: invalid identity metadata".into()));
            }
            let value = i64::from_le_bytes(
                payload_bytes
                    .try_into()
                    .expect("identity payload length checked"),
            );
            if value < 1 {
                return Err(Error::Corrupt("wal: invalid identity high-water".into()));
            }
            identity_high_water.push((id, value));
        } else {
            changes.push(WalChange {
                table,
                id,
                payload: (kind == KIND_PUT).then(|| payload_bytes.to_vec()),
            });
        }
    }
    let crc_pos = *pos;
    let stored_crc = read_u32(data, pos)?;
    if crc32fast::hash(&data[start..crc_pos]) != stored_crc {
        return Err(Error::Corrupt("wal: record crc mismatch".into()));
    }
    Ok(WalRecord {
        version,
        changes,
        identity_high_water,
    })
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
    poisoned: Option<String>,
    #[cfg(test)]
    fail_next_sync: bool,
}

pub(crate) enum WalAppendOutcome {
    Complete,
    SyncFailed(std::io::Error),
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
            poisoned: None,
            #[cfg(test)]
            fail_next_sync: false,
        })
    }

    fn rollback_partial_append(&mut self, start: u64, write_error: std::io::Error) -> Error {
        if let Err(rollback_error) = self.file.set_len(start) {
            self.poisoned = Some(format!(
                "write failed ({write_error}); truncating to {start} also failed ({rollback_error})"
            ));
            return Error::Io(std::io::Error::other(
                self.poisoned.as_ref().expect("poison just stored").clone(),
            ));
        }
        Error::Io(write_error)
    }

    #[cfg(test)]
    pub(crate) fn fail_next_sync_for_test(&mut self) {
        self.fail_next_sync = true;
    }

    #[cfg(test)]
    pub fn append_commit(
        &mut self,
        bytes: &[u8],
        durability: Durability,
        balanced_interval_ms: u64,
    ) -> Result<WalAppendOutcome> {
        self.append_commit_unflushed(bytes)?;
        if self.sync_due(durability, balanced_interval_ms) {
            Ok(self.sync_data())
        } else {
            Ok(WalAppendOutcome::Complete)
        }
    }

    /// Append one complete framed record without forcing it to storage. The
    /// caller may subsequently group several appended records behind one
    /// `sync_data` call.
    pub fn append_commit_unflushed(&mut self, bytes: &[u8]) -> Result<u64> {
        if let Some(message) = &self.poisoned {
            return Err(Error::Io(std::io::Error::other(format!(
                "WAL writer is unusable after an earlier append failure: {message}"
            ))));
        }
        let start = self.len;
        if let Err(write_error) = self.file.write_all(bytes) {
            // `write_all` may have emitted a prefix. Never append another
            // record behind that torn tail. If rollback itself fails, poison
            // the writer so this handle cannot acknowledge later commits.
            return Err(self.rollback_partial_append(start, write_error));
        }
        self.len = self.len.saturating_add(bytes.len() as u64);
        Ok(self.len)
    }

    pub fn sync_due(&self, durability: Durability, balanced_interval_ms: u64) -> bool {
        match durability {
            Durability::Safe => true,
            Durability::Balanced => {
                self.last_sync.elapsed().as_millis() as u64 >= balanced_interval_ms
            }
            Durability::Fast => false,
        }
    }

    /// Synchronize every record appended so far. A sync failure leaves the
    /// fully framed records in place and therefore has the same ambiguous
    /// outcome as the legacy per-commit path.
    pub fn sync_data(&mut self) -> WalAppendOutcome {
        let sync_result = self.file.sync_data();
        #[cfg(test)]
        let sync_result = if std::mem::take(&mut self.fail_next_sync) {
            Err(std::io::Error::other("injected WAL sync failure"))
        } else {
            sync_result
        };
        match sync_result {
            Ok(()) => {
                self.last_sync = Instant::now();
                WalAppendOutcome::Complete
            }
            // The full framed record is already part of this process's WAL.
            // Rolling it back would make its outcome even less knowable and
            // permit version reuse. Publish it logically and tell the caller
            // that crash durability is unknown.
            Err(error) => WalAppendOutcome::SyncFailed(error),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn scan_stops_before_duplicate_or_regressing_versions() {
        let first = encode_commit(7, &[("t", "a", Some(b"one"))], &[]).unwrap();
        let duplicate = encode_commit(7, &[("t", "b", Some(b"two"))], &[]).unwrap();
        let later = encode_commit(8, &[("t", "c", Some(b"three"))], &[]).unwrap();
        let mut bytes = first.clone();
        bytes.extend_from_slice(&duplicate);
        bytes.extend_from_slice(&later);

        let scan = scan_wal(&bytes);
        assert!(!scan.clean);
        assert_eq!(scan.valid_len, first.len() as u64);
        assert_eq!(scan.records.len(), 1);
        assert_eq!(scan.records[0].version, 7);
    }

    #[test]
    fn partial_append_is_rolled_back_before_another_record_is_written() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir(dir.path().join(WAL_DIR)).unwrap();
        let mut writer = WalWriter::open(dir.path(), 1).unwrap();
        let torn = encode_commit(1, &[("t", "a", Some(b"one"))], &[]).unwrap();
        writer.file.write_all(&torn[..torn.len() / 2]).unwrap();
        let _ = writer.rollback_partial_append(0, std::io::Error::other("injected partial write"));
        assert_eq!(std::fs::metadata(wal_path(dir.path(), 1)).unwrap().len(), 0);

        let complete = encode_commit(1, &[("t", "b", Some(b"two"))], &[]).unwrap();
        assert!(matches!(
            writer
                .append_commit(&complete, Durability::Fast, 0)
                .unwrap(),
            WalAppendOutcome::Complete
        ));
        drop(writer);
        let bytes = std::fs::read(wal_path(dir.path(), 1)).unwrap();
        let scan = scan_wal(&bytes);
        assert!(scan.clean);
        assert_eq!(scan.records.len(), 1);
        assert_eq!(scan.records[0].version, 1);
        assert_eq!(scan.records[0].changes[0].id, "b");
    }

    #[test]
    fn encoder_rejects_strings_that_do_not_fit_its_frame() {
        let too_long = "x".repeat(u16::MAX as usize + 1);
        assert!(encode_commit(1, &[("t", &too_long, None)], &[]).is_err());
        assert!(encode_commit(1, &[(&too_long, "id", None)], &[]).is_err());
        assert!(encode_commit(1, &[], &[(&too_long, 1)]).is_err());
    }

    #[test]
    fn an_encoded_commit_can_be_assigned_its_final_version() {
        let mut encoded = encode_commit(0, &[("t", "a", Some(b"one"))], &[]).unwrap();
        set_encoded_commit_version(&mut encoded, 42);

        let scanned = scan_wal(&encoded);
        assert!(scanned.clean);
        assert_eq!(scanned.records.len(), 1);
        assert_eq!(scanned.records[0].version, 42);
        assert_eq!(
            scanned.records[0].changes[0].payload.as_deref(),
            Some(b"one".as_slice())
        );
    }
}
