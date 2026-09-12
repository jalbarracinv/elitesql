use std::fs::{File, OpenOptions};
use std::io::{IoSlice, Write};
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

/// Preflight the entire recoverable chain before normal open modifies any
/// canonical file. Only an incomplete final record may be truncated.
pub(crate) fn validate_wal_chain(
    dir: &Path,
    anchor: u32,
    required: u32,
    watermark: u64,
) -> Result<Vec<u32>> {
    let mut ids = Vec::new();
    for entry in std::fs::read_dir(dir.join(WAL_DIR))? {
        let entry = entry?;
        if let Some(id) = entry
            .file_name()
            .to_str()
            .and_then(|name| name.strip_suffix(".wal"))
            .and_then(|name| name.parse::<u32>().ok())
        {
            if id >= anchor {
                ids.push(id);
            }
        }
    }
    ids.sort_unstable();
    if ids.first() != Some(&anchor) || ids.last().is_none_or(|id| *id < required.max(anchor)) {
        return Err(Error::Corrupt(format!(
            "required wal chain {anchor}..={} is incomplete",
            required.max(anchor)
        )));
    }
    let mut expected_id = anchor;
    let mut version = watermark;
    let mut scans = Vec::with_capacity(ids.len());
    for id in ids.iter().copied() {
        if id != expected_id {
            return Err(Error::Corrupt(format!(
                "required wal {expected_id} is missing before {id}"
            )));
        }
        expected_id = id.saturating_add(1);
        let data = std::fs::read(wal_path(dir, id))?;
        let scan = scan_wal(&data);
        if let Some(message) = scan.corruption {
            return Err(Error::Corrupt(format!(
                "wal {id} at offset {}: {message}",
                scan.valid_len
            )));
        }
        scans.push((id, scan));
    }
    for (position, (id, scan)) in scans.iter().enumerate() {
        // A writer switch reserves empty successors before the previous WAL
        // is synced, so a power loss can leave a torn tail followed only by
        // empty files. A successor that already holds commits, however, means
        // the torn record was complete when they were written: corruption.
        if !scan.clean
            && scans[position + 1..]
                .iter()
                .any(|(_, later)| !later.records.is_empty())
        {
            return Err(Error::Corrupt(format!(
                "wal {id}: incomplete record before a successor WAL with commits"
            )));
        }
    }
    for (id, scan) in scans {
        for record in scan.records {
            if record.version <= version {
                continue;
            }
            if version.checked_add(1) != Some(record.version) {
                return Err(Error::Corrupt(format!(
                    "wal {id}: commit version gap after {version}: {}",
                    record.version
                )));
            }
            version = record.version;
        }
    }
    Ok(ids)
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
    pub corruption: Option<String>,
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
                corruption: None,
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
                    corruption: Some("WAL versions are not strictly increasing".into()),
                }
            }
            Err(error) => {
                // Only a complete, checksummed record *after* the failure
                // proves interior corruption. Without one, the damaged bytes
                // are the tail a crash left behind: a strict prefix after a
                // process kill, or zero-filled/partially written blocks after
                // a power loss. Both are truncated as an incomplete final
                // record; nothing acknowledged as durable can live there.
                let later_valid_record = (start.saturating_add(1)..data.len().saturating_sub(15))
                    .any(|offset| {
                        let mut candidate = offset;
                        parse_record(data, &mut candidate, offset)
                            .is_ok_and(|record| record.version > previous_version.unwrap_or(0))
                    });
                let corruption = match error {
                    WalParseError::Incomplete => later_valid_record
                        .then(|| "incomplete record precedes a valid WAL record".into()),
                    WalParseError::Corrupt(message) => later_valid_record.then_some(message),
                };
                return WalScan {
                    records,
                    valid_len: start as u64,
                    clean: false,
                    corruption,
                };
            }
        }
    }
}

enum WalParseError {
    Incomplete,
    Corrupt(String),
}

fn parse_record(
    data: &[u8],
    pos: &mut usize,
    start: usize,
) -> std::result::Result<WalRecord, WalParseError> {
    let version = read_u64(data, pos).map_err(|_| WalParseError::Incomplete)?;
    let count = read_u32(data, pos).map_err(|_| WalParseError::Incomplete)? as usize;
    // Bounds-checked reads keep a garbage count from allocating; this cap is
    // an extra sanity guard against absurd-but-parseable values.
    if count as u64 > data.len() as u64 {
        return Err(WalParseError::Incomplete);
    }
    let mut changes = Vec::with_capacity(count.min(1024));
    let mut identity_high_water = Vec::new();
    for _ in 0..count {
        let kind = read_u8(data, pos).map_err(|_| WalParseError::Incomplete)?;
        if kind != KIND_PUT && kind != KIND_TOMBSTONE {
            return Err(WalParseError::Corrupt(format!(
                "wal: unknown change kind {kind}"
            )));
        }
        let table = read_short_str(data, pos)?;
        let id = read_short_str(data, pos)?;
        let payload_len = read_u32(data, pos).map_err(|_| WalParseError::Incomplete)? as usize;
        let end = pos
            .checked_add(payload_len)
            .ok_or_else(|| WalParseError::Corrupt("wal: length overflow".into()))?;
        let payload_bytes = data.get(*pos..end).ok_or(WalParseError::Incomplete)?;
        *pos = end;
        if kind == KIND_TOMBSTONE && payload_len != 0 {
            return Err(WalParseError::Corrupt("wal: tombstone with payload".into()));
        }
        if table == IDENTITY_META_TABLE {
            if kind != KIND_PUT || payload_len != 8 {
                return Err(WalParseError::Corrupt(
                    "wal: invalid identity metadata".into(),
                ));
            }
            let value = i64::from_le_bytes(
                payload_bytes
                    .try_into()
                    .expect("identity payload length checked"),
            );
            if value < 1 {
                return Err(WalParseError::Corrupt(
                    "wal: invalid identity high-water".into(),
                ));
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
    let stored_crc = read_u32(data, pos).map_err(|_| WalParseError::Incomplete)?;
    if crc32fast::hash(&data[start..crc_pos]) != stored_crc {
        return Err(WalParseError::Corrupt("wal: record crc mismatch".into()));
    }
    Ok(WalRecord {
        version,
        changes,
        identity_high_water,
    })
}

fn read_short_str(data: &[u8], pos: &mut usize) -> std::result::Result<String, WalParseError> {
    let len = read_u16(data, pos).map_err(|_| WalParseError::Incomplete)? as usize;
    let end = pos
        .checked_add(len)
        .ok_or_else(|| WalParseError::Corrupt("wal: length overflow".into()))?;
    let slice = data.get(*pos..end).ok_or(WalParseError::Incomplete)?;
    *pos = end;
    std::str::from_utf8(slice)
        .map(|s| s.to_owned())
        .map_err(|_| WalParseError::Corrupt("wal: invalid utf8".into()))
}

pub(crate) struct WalWriter {
    pub id: u32,
    file: File,
    pub len: u64,
    last_sync: Instant,
    /// Bytes appended since the last successful `sync_data`.
    unsynced: bool,
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
            unsynced: false,
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
        self.unsynced = true;
        Ok(self.len)
    }

    /// Append several already-framed commits with vectored writes. All
    /// records retain independent CRCs and recovery boundaries; the only
    /// shared work is entering the kernel. A partial/error outcome rolls the
    /// complete batch back to its original length just like a single append.
    pub fn append_commits_unflushed(&mut self, records: &[&[u8]]) -> Result<u64> {
        if let Some(message) = &self.poisoned {
            return Err(Error::Io(std::io::Error::other(format!(
                "WAL writer is unusable after an earlier append failure: {message}"
            ))));
        }
        let total = records.iter().try_fold(0usize, |total, record| {
            total
                .checked_add(record.len())
                .ok_or_else(|| Error::InvalidArgument("WAL batch length overflow".into()))
        })?;
        if total == 0 {
            return Ok(self.len);
        }

        let start = self.len;
        let mut index = 0usize;
        let mut offset = 0usize;
        while index < records.len() {
            let mut slices = Vec::with_capacity((records.len() - index).min(64));
            slices.push(IoSlice::new(&records[index][offset..]));
            for record in records.iter().skip(index + 1).take(63) {
                slices.push(IoSlice::new(record));
            }
            let written = match self.file.write_vectored(&slices) {
                Ok(0) => {
                    return Err(self.rollback_partial_append(
                        start,
                        std::io::Error::new(
                            std::io::ErrorKind::WriteZero,
                            "failed to append coordinated WAL batch",
                        ),
                    ))
                }
                Ok(written) => written,
                Err(error) if error.kind() == std::io::ErrorKind::Interrupted => continue,
                Err(error) => return Err(self.rollback_partial_append(start, error)),
            };
            let mut remaining = written;
            while index < records.len() {
                let available = records[index].len() - offset;
                if remaining < available {
                    offset += remaining;
                    break;
                }
                remaining -= available;
                index += 1;
                offset = 0;
                if remaining == 0 {
                    break;
                }
            }
        }
        self.len = self.len.saturating_add(total as u64);
        self.unsynced = true;
        Ok(self.len)
    }

    /// Whether appended records still await a durability barrier.
    pub fn has_unsynced_appends(&self) -> bool {
        self.unsynced
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
        let sync_result = crate::durable::sync_data(&self.file);
        #[cfg(test)]
        let sync_result = if std::mem::take(&mut self.fail_next_sync) {
            Err(std::io::Error::other("injected WAL sync failure"))
        } else {
            sync_result
        };
        match sync_result {
            Ok(()) => {
                self.last_sync = Instant::now();
                self.unsynced = false;
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

    #[test]
    fn vectored_batch_retains_independent_recovery_frames() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir(dir.path().join(WAL_DIR)).unwrap();
        let mut writer = WalWriter::open(dir.path(), 1).unwrap();
        let first = encode_commit(1, &[("t", "a", Some(b"one"))], &[]).unwrap();
        let second = encode_commit(2, &[("t", "b", Some(b"two"))], &[]).unwrap();

        writer.append_commits_unflushed(&[&first, &second]).unwrap();
        drop(writer);

        let bytes = std::fs::read(wal_path(dir.path(), 1)).unwrap();
        let scan = scan_wal(&bytes);
        assert!(scan.clean);
        assert_eq!(scan.records.len(), 2);
        assert_eq!(scan.records[0].version, 1);
        assert_eq!(scan.records[0].changes[0].id, "a");
        assert_eq!(scan.records[1].version, 2);
        assert_eq!(scan.records[1].changes[0].id, "b");
    }
}
