use crate::error::{Error, Result};
use crate::value::{read_u16, read_u32, read_u64, read_u8};

pub(crate) const KIND_PUT: u8 = 1;
pub(crate) const KIND_TOMBSTONE: u8 = 2;

// Entry layout (all integers little-endian):
//
//   u8   kind            KIND_PUT | KIND_TOMBSTONE
//   u64  version         global monotonic sequence
//   u16  table_len       + table name bytes (utf8)
//   u16  id_len          + id bytes (utf8)
//   u32  payload_len     + payload bytes (encoded record; empty on tombstone)
//   u32  crc32           over every preceding byte of the entry
//
// A torn tail (partial last entry after a kill) fails either framing or CRC
// and is dropped on open. Phase 0 gives no durability guarantee beyond that.

/// Location of one record version inside a segment, as discovered on open
/// or recorded at write time. This is what the in-memory index stores.
#[derive(Debug, Clone)]
pub(crate) struct EntryRef {
    pub version: u64,
    pub table: String,
    pub id: String,
    pub tombstone: bool,
    pub payload_offset: u64,
    pub payload_len: u32,
}

pub(crate) fn segment_file_name(id: u32) -> String {
    format!("{id:06}.seg")
}

/// Encode an entry into reusable storage. Checkpoint and compaction call this
/// to avoid one heap allocation for every persisted record. Returns the
/// offset of the payload relative to the start of the entry.
pub(crate) fn encode_entry_into(
    buf: &mut Vec<u8>,
    version: u64,
    table: &str,
    id: &str,
    payload: Option<&[u8]>,
) -> Result<u64> {
    let kind = if payload.is_some() {
        KIND_PUT
    } else {
        KIND_TOMBSTONE
    };
    let payload = payload.unwrap_or(&[]);
    buf.clear();
    buf.reserve(1 + 8 + 2 + table.len() + 2 + id.len() + 4 + payload.len() + 4);
    buf.push(kind);
    buf.extend_from_slice(&version.to_le_bytes());
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
    let payload_len = u32::try_from(payload.len()).map_err(|_| {
        Error::InvalidArgument("segment payload exceeds the 4-GiB storage limit".into())
    })?;
    buf.extend_from_slice(&payload_len.to_le_bytes());
    let payload_offset = buf.len() as u64;
    buf.extend_from_slice(payload);
    let crc = crc32fast::hash(buf);
    buf.extend_from_slice(&crc.to_le_bytes());
    Ok(payload_offset)
}

pub(crate) struct ScanOutcome {
    /// Byte length of the valid prefix of the segment.
    pub valid_len: u64,
    /// False when the segment ended in a torn or corrupt entry.
    pub clean: bool,
}

/// Scan a whole segment buffer, appending every valid entry to `out`.
/// Stops at the first entry that fails framing or CRC validation.
pub(crate) fn scan_segment(data: &[u8], out: &mut Vec<EntryRef>) -> ScanOutcome {
    let mut pos = 0usize;
    loop {
        let entry_start = pos;
        if pos >= data.len() {
            return ScanOutcome {
                valid_len: entry_start as u64,
                clean: true,
            };
        }
        match parse_entry(data, &mut pos, entry_start as u64) {
            Ok(entry) => out.push(entry),
            Err(_) => {
                return ScanOutcome {
                    valid_len: entry_start as u64,
                    clean: false,
                }
            }
        }
    }
}

/// Scan a segment while handing each entry to a visitor immediately. Unlike
/// [`scan_segment`], this keeps memory independent of the number of records in
/// the segment and is used by recovery/index rebuild paths.
pub(crate) fn visit_segment(
    data: &[u8],
    mut visit: impl FnMut(EntryRef) -> Result<()>,
) -> Result<ScanOutcome> {
    let mut pos = 0usize;
    loop {
        let entry_start = pos;
        if pos >= data.len() {
            return Ok(ScanOutcome {
                valid_len: entry_start as u64,
                clean: true,
            });
        }
        match parse_entry(data, &mut pos, entry_start as u64) {
            Ok(entry) => visit(entry)?,
            Err(_) => {
                return Ok(ScanOutcome {
                    valid_len: entry_start as u64,
                    clean: false,
                })
            }
        }
    }
}

/// Validate framing and CRCs without allocating table/id strings. Used when
/// a current mmap primary directory already supplies every entry location.
pub(crate) fn validate_segment(data: &[u8]) -> ScanOutcome {
    let mut pos = 0usize;
    loop {
        let entry_start = pos;
        if pos >= data.len() {
            return ScanOutcome {
                valid_len: entry_start as u64,
                clean: true,
            };
        }
        if validate_entry(data, &mut pos, entry_start).is_err() {
            return ScanOutcome {
                valid_len: entry_start as u64,
                clean: false,
            };
        }
    }
}

fn validate_entry(data: &[u8], pos: &mut usize, entry_start: usize) -> Result<()> {
    let kind = read_u8(data, pos)?;
    if kind != KIND_PUT && kind != KIND_TOMBSTONE {
        return Err(Error::Corrupt(format!("unknown entry kind {kind}")));
    }
    read_u64(data, pos)?;
    validate_short_str(data, pos)?;
    validate_short_str(data, pos)?;
    let payload_len = read_u32(data, pos)? as usize;
    let payload_end = pos
        .checked_add(payload_len)
        .filter(|end| *end <= data.len())
        .ok_or_else(|| Error::Corrupt("truncated payload".into()))?;
    *pos = payload_end;
    let stored_crc = read_u32(data, pos)?;
    if stored_crc != crc32fast::hash(&data[entry_start..payload_end]) {
        return Err(Error::Corrupt("entry crc mismatch".into()));
    }
    if kind == KIND_TOMBSTONE && payload_len != 0 {
        return Err(Error::Corrupt("tombstone with payload".into()));
    }
    Ok(())
}

fn validate_short_str(data: &[u8], pos: &mut usize) -> Result<()> {
    let len = read_u16(data, pos)? as usize;
    let end = pos
        .checked_add(len)
        .filter(|end| *end <= data.len())
        .ok_or_else(|| Error::Corrupt("unexpected end of data".into()))?;
    std::str::from_utf8(&data[*pos..end]).map_err(|_| Error::Corrupt("invalid utf8".into()))?;
    *pos = end;
    Ok(())
}

fn parse_entry(data: &[u8], pos: &mut usize, entry_start: u64) -> Result<EntryRef> {
    let kind = read_u8(data, pos)?;
    if kind != KIND_PUT && kind != KIND_TOMBSTONE {
        return Err(Error::Corrupt(format!("unknown entry kind {kind}")));
    }
    let version = read_u64(data, pos)?;
    let table = read_short_str(data, pos)?;
    let id = read_short_str(data, pos)?;
    let payload_len = read_u32(data, pos)?;
    let payload_start = *pos;
    let payload_end = payload_start
        .checked_add(payload_len as usize)
        .ok_or_else(|| Error::Corrupt("payload length overflow".into()))?;
    if payload_end > data.len() {
        return Err(Error::Corrupt("truncated payload".into()));
    }
    *pos = payload_end;
    let stored_crc = read_u32(data, pos)?;
    let actual_crc = crc32fast::hash(&data[entry_start as usize..payload_end]);
    if stored_crc != actual_crc {
        return Err(Error::Corrupt("entry crc mismatch".into()));
    }
    if kind == KIND_TOMBSTONE && payload_len != 0 {
        return Err(Error::Corrupt("tombstone with payload".into()));
    }
    Ok(EntryRef {
        version,
        table,
        id,
        tombstone: kind == KIND_TOMBSTONE,
        payload_offset: payload_start as u64,
        payload_len,
    })
}

fn read_short_str(data: &[u8], pos: &mut usize) -> Result<String> {
    let len = read_u16(data, pos)? as usize;
    let end = pos
        .checked_add(len)
        .ok_or_else(|| Error::Corrupt("length overflow".into()))?;
    let slice = data
        .get(*pos..end)
        .ok_or_else(|| Error::Corrupt("unexpected end of data".into()))?;
    *pos = end;
    std::str::from_utf8(slice)
        .map(|s| s.to_owned())
        .map_err(|_| Error::Corrupt("invalid utf8".into()))
}
