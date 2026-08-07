use serde::{Deserialize, Serialize};
use std::fmt;

use crate::error::{Error, Result};

/// The V1 column types from the spec, including the native vector type and
/// the V1.1 date/time additions. Vector columns carry their dimension in
/// [`crate::Column::dim`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ColumnType {
    Bool,
    Int64,
    Float64,
    Text,
    Blob,
    Timestamp,
    Json,
    Vector,
    Date,
    Time,
}

impl fmt::Display for ColumnType {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let s = match self {
            ColumnType::Bool => "bool",
            ColumnType::Int64 => "int64",
            ColumnType::Float64 => "float64",
            ColumnType::Text => "text",
            ColumnType::Blob => "blob",
            ColumnType::Timestamp => "timestamp",
            ColumnType::Json => "json",
            ColumnType::Vector => "vector",
            ColumnType::Date => "date",
            ColumnType::Time => "time",
        };
        f.write_str(s)
    }
}

/// A single field value. `Timestamp` is microseconds since the Unix epoch;
/// `Date` is days since the Unix epoch; `Time` is microseconds since
/// midnight; `Vector` is an embedding of f32 components (dimension fixed
/// per column).
#[derive(Debug, Clone, PartialEq)]
pub enum Value {
    Null,
    Bool(bool),
    Int64(i64),
    Float64(f64),
    Text(String),
    Blob(Vec<u8>),
    Timestamp(i64),
    Json(serde_json::Value),
    Vector(Vec<f32>),
    Date(i32),
    Time(i64),
}

impl Value {
    pub fn is_null(&self) -> bool {
        matches!(self, Value::Null)
    }

    pub fn matches(&self, ty: ColumnType) -> bool {
        matches!(
            (self, ty),
            (Value::Bool(_), ColumnType::Bool)
                | (Value::Int64(_), ColumnType::Int64)
                | (Value::Float64(_), ColumnType::Float64)
                | (Value::Text(_), ColumnType::Text)
                | (Value::Blob(_), ColumnType::Blob)
                | (Value::Timestamp(_), ColumnType::Timestamp)
                | (Value::Json(_), ColumnType::Json)
                | (Value::Vector(_), ColumnType::Vector)
                | (Value::Date(_), ColumnType::Date)
                | (Value::Time(_), ColumnType::Time)
        )
    }

    pub fn as_text(&self) -> Option<&str> {
        match self {
            Value::Text(s) => Some(s),
            _ => None,
        }
    }

    /// Build a `Date` from a calendar day; None if the date is not real
    /// (bad month, bad day-of-month) or the year is outside 1..=9999.
    pub fn date_from_ymd(year: i32, month: u32, day: u32) -> Option<Value> {
        days_from_ymd(year, month, day).map(Value::Date)
    }

    /// Parse `'YYYY-MM-DD'` into a `Date`.
    pub fn parse_date(s: &str) -> Option<Value> {
        parse_date_str(s).map(Value::Date)
    }

    /// Build a `Time` from hour/minute/second/microsecond; None out of range.
    pub fn time_from_hms_micro(hour: u32, minute: u32, second: u32, micro: u32) -> Option<Value> {
        if hour > 23 || minute > 59 || second > 59 || micro > 999_999 {
            return None;
        }
        let micros =
            ((hour as i64 * 3600 + minute as i64 * 60 + second as i64) * 1_000_000) + micro as i64;
        Some(Value::Time(micros))
    }

    /// Parse `'HH:MM:SS[.ffffff]'` into a `Time`.
    pub fn parse_time(s: &str) -> Option<Value> {
        parse_time_str(s).map(Value::Time)
    }

    /// Parse a naive-UTC datetime string into a `Timestamp` (microseconds
    /// since the Unix epoch). Accepts `'YYYY-MM-DD HH:MM:SS[.ffffff]'`, the
    /// ISO `T` separator, an optional trailing `Z`, and a date-only string
    /// (midnight UTC). Timezone offsets are not supported: ClawDB stores
    /// instants; timezone presentation belongs to the application.
    pub fn parse_timestamp(s: &str) -> Option<Value> {
        parse_timestamp_str(s).map(Value::Timestamp)
    }
}

// --- calendar helpers ------------------------------------------------------

fn is_leap(y: i32) -> bool {
    (y % 4 == 0 && y % 100 != 0) || y % 400 == 0
}

fn days_in_month(y: i32, m: u32) -> u32 {
    match m {
        1 | 3 | 5 | 7 | 8 | 10 | 12 => 31,
        4 | 6 | 9 | 11 => 30,
        2 => {
            if is_leap(y) {
                29
            } else {
                28
            }
        }
        _ => 0,
    }
}

/// Days since the Unix epoch for a civil date (Howard Hinnant's algorithm),
/// validating that the date actually exists. Years limited to 1..=9999.
pub(crate) fn days_from_ymd(year: i32, month: u32, day: u32) -> Option<i32> {
    if !(1..=9999).contains(&year) || !(1..=12).contains(&month) {
        return None;
    }
    if day < 1 || day > days_in_month(year, month) {
        return None;
    }
    let y = year as i64 - if month <= 2 { 1 } else { 0 };
    let m = month as i64;
    let d = day as i64;
    let era = if y >= 0 { y } else { y - 399 } / 400;
    let yoe = y - era * 400;
    let doy = (153 * (if m > 2 { m - 3 } else { m + 9 }) + 2) / 5 + d - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    Some((era * 146_097 + doe - 719_468) as i32)
}

pub(crate) fn parse_date_str(s: &str) -> Option<i32> {
    let mut parts = s.split('-');
    let year: i32 = parts.next()?.parse().ok()?;
    let month: u32 = parts.next()?.parse().ok()?;
    let day: u32 = parts.next()?.parse().ok()?;
    if parts.next().is_some() {
        return None;
    }
    days_from_ymd(year, month, day)
}

/// Inverse of `days_from_ymd` (Howard Hinnant's civil_from_days).
pub(crate) fn ymd_from_days(days: i32) -> (i32, u32, u32) {
    let z = days as i64 + 719_468;
    let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = (doy - (153 * mp + 2) / 5 + 1) as u32;
    let m = if mp < 10 { mp + 3 } else { mp - 9 } as u32;
    let y = if m <= 2 { y + 1 } else { y };
    (y as i32, m, d)
}

pub(crate) fn parse_timestamp_str(s: &str) -> Option<i64> {
    let s = s.strip_suffix('Z').unwrap_or(s);
    let (date_part, time_part) = match s.split_once(' ').or_else(|| s.split_once('T')) {
        Some((d, t)) => (d, Some(t)),
        None => (s, None),
    };
    let days = parse_date_str(date_part)? as i64;
    let micros = match time_part {
        Some(t) => parse_time_str(t)?,
        None => 0,
    };
    Some(days * 86_400_000_000 + micros)
}

pub(crate) fn parse_time_str(s: &str) -> Option<i64> {
    let (hms, frac) = match s.split_once('.') {
        Some((a, b)) => (a, Some(b)),
        None => (s, None),
    };
    let mut parts = hms.split(':');
    let h: u32 = parts.next()?.parse().ok()?;
    let m: u32 = parts.next()?.parse().ok()?;
    let sec: u32 = parts.next()?.parse().ok()?;
    if parts.next().is_some() || h > 23 || m > 59 || sec > 59 {
        return None;
    }
    let micro: i64 = match frac {
        None => 0,
        Some(f) => {
            if f.is_empty() || f.len() > 6 || !f.bytes().all(|b| b.is_ascii_digit()) {
                return None;
            }
            let padded = format!("{f:0<6}");
            padded.parse().ok()?
        }
    };
    Some((h as i64 * 3600 + m as i64 * 60 + sec as i64) * 1_000_000 + micro)
}

// --- Binary encoding -------------------------------------------------------
//
// Values are encoded as a 1-byte tag followed by a type-specific payload.
// Variable-length payloads carry a u32 little-endian length prefix.

const TAG_NULL: u8 = 0;
const TAG_BOOL: u8 = 1;
const TAG_INT64: u8 = 2;
const TAG_FLOAT64: u8 = 3;
const TAG_TEXT: u8 = 4;
const TAG_BLOB: u8 = 5;
const TAG_TIMESTAMP: u8 = 6;
const TAG_JSON: u8 = 7;
const TAG_VECTOR: u8 = 8;
const TAG_DATE: u8 = 9;
const TAG_TIME: u8 = 10;
/// Internal only: a reference to an out-of-line blob chunk in `blobs/`.
/// Resolved to `Value::Blob` during record decoding; never user-visible.
const TAG_BLOBREF: u8 = 11;

const BLOB_MAGIC: &[u8; 8] = b"CLAWBLOB";

#[derive(Debug, Clone)]
pub(crate) struct BlobRef {
    pub name: String,
    pub size: u64,
    pub crc: u32,
}

pub(crate) fn encode_blob_ref(buf: &mut Vec<u8>, name: &str, size: u64, crc: u32) {
    buf.push(TAG_BLOBREF);
    buf.extend_from_slice(&(name.len() as u16).to_le_bytes());
    buf.extend_from_slice(name.as_bytes());
    buf.extend_from_slice(&size.to_le_bytes());
    buf.extend_from_slice(&crc.to_le_bytes());
}

fn read_blob_ref(buf: &[u8], pos: &mut usize) -> Result<BlobRef> {
    let name_len = read_u16(buf, pos)? as usize;
    let end = pos
        .checked_add(name_len)
        .ok_or_else(|| Error::Corrupt("blobref: length overflow".into()))?;
    let name_bytes = buf
        .get(*pos..end)
        .ok_or_else(|| Error::Corrupt("blobref: unexpected end".into()))?;
    *pos = end;
    let name = std::str::from_utf8(name_bytes)
        .map_err(|_| Error::Corrupt("blobref: invalid utf8".into()))?
        .to_owned();
    if name.contains('/') || name.contains("..") {
        return Err(Error::Corrupt("blobref: invalid name".into()));
    }
    let size = read_u64(buf, pos)?;
    let crc = read_u32(buf, pos)?;
    Ok(BlobRef { name, size, crc })
}

/// Blob chunk file layout: 8-byte magic, u32 crc of the content, u64 length,
/// content. Fully validated on read.
pub(crate) fn write_blob_file_bytes(content: &[u8]) -> (Vec<u8>, u32) {
    let crc = crc32fast::hash(content);
    let mut out = Vec::with_capacity(content.len() + 20);
    out.extend_from_slice(BLOB_MAGIC);
    out.extend_from_slice(&crc.to_le_bytes());
    out.extend_from_slice(&(content.len() as u64).to_le_bytes());
    out.extend_from_slice(content);
    (out, crc)
}

pub(crate) fn read_blob_file(dir: &std::path::Path, r: &BlobRef) -> Result<Vec<u8>> {
    let bytes = std::fs::read(dir.join(format!("{}.blob", r.name)))
        .map_err(|e| Error::Corrupt(format!("blob chunk {} unreadable: {e}", r.name)))?;
    if bytes.len() < 20 || &bytes[..8] != BLOB_MAGIC {
        return Err(Error::Corrupt(format!("blob chunk {}: bad header", r.name)));
    }
    let crc = u32::from_le_bytes(bytes[8..12].try_into().unwrap());
    let len = u64::from_le_bytes(bytes[12..20].try_into().unwrap());
    let content = &bytes[20..];
    if crc != r.crc || len != r.size || content.len() as u64 != len {
        return Err(Error::Corrupt(format!(
            "blob chunk {}: checksum or size mismatch",
            r.name
        )));
    }
    if crc32fast::hash(content) != crc {
        return Err(Error::Corrupt(format!("blob chunk {}: content corrupt", r.name)));
    }
    Ok(content.to_vec())
}

pub(crate) fn encode_value(buf: &mut Vec<u8>, v: &Value) {
    match v {
        Value::Null => buf.push(TAG_NULL),
        Value::Bool(b) => {
            buf.push(TAG_BOOL);
            buf.push(*b as u8);
        }
        Value::Int64(n) => {
            buf.push(TAG_INT64);
            buf.extend_from_slice(&n.to_le_bytes());
        }
        Value::Float64(x) => {
            buf.push(TAG_FLOAT64);
            buf.extend_from_slice(&x.to_le_bytes());
        }
        Value::Text(s) => {
            buf.push(TAG_TEXT);
            buf.extend_from_slice(&(s.len() as u32).to_le_bytes());
            buf.extend_from_slice(s.as_bytes());
        }
        Value::Blob(b) => {
            buf.push(TAG_BLOB);
            buf.extend_from_slice(&(b.len() as u32).to_le_bytes());
            buf.extend_from_slice(b);
        }
        Value::Timestamp(n) => {
            buf.push(TAG_TIMESTAMP);
            buf.extend_from_slice(&n.to_le_bytes());
        }
        Value::Json(j) => {
            let bytes = serde_json::to_vec(j).expect("serde_json::Value always serializes");
            buf.push(TAG_JSON);
            buf.extend_from_slice(&(bytes.len() as u32).to_le_bytes());
            buf.extend_from_slice(&bytes);
        }
        Value::Vector(v) => {
            buf.push(TAG_VECTOR);
            buf.extend_from_slice(&(v.len() as u32).to_le_bytes());
            for x in v {
                buf.extend_from_slice(&x.to_le_bytes());
            }
        }
        Value::Date(d) => {
            buf.push(TAG_DATE);
            buf.extend_from_slice(&d.to_le_bytes());
        }
        Value::Time(t) => {
            buf.push(TAG_TIME);
            buf.extend_from_slice(&t.to_le_bytes());
        }
    }
}

/// Decode one tagged value. `blobs` is the directory used to resolve
/// out-of-line blob references; contexts without one fail on such refs.
pub(crate) fn decode_value(buf: &[u8], pos: &mut usize, blobs: Option<&std::path::Path>) -> Result<Value> {
    let tag = read_u8(buf, pos)?;
    match tag {
        TAG_NULL => Ok(Value::Null),
        TAG_BOOL => Ok(Value::Bool(read_u8(buf, pos)? != 0)),
        TAG_INT64 => Ok(Value::Int64(i64::from_le_bytes(read_array(buf, pos)?))),
        TAG_FLOAT64 => Ok(Value::Float64(f64::from_le_bytes(read_array(buf, pos)?))),
        TAG_TEXT => {
            let bytes = read_len_prefixed(buf, pos)?;
            let s = std::str::from_utf8(bytes)
                .map_err(|_| Error::Corrupt("invalid utf8 in text value".into()))?;
            Ok(Value::Text(s.to_owned()))
        }
        TAG_BLOB => Ok(Value::Blob(read_len_prefixed(buf, pos)?.to_vec())),
        TAG_TIMESTAMP => Ok(Value::Timestamp(i64::from_le_bytes(read_array(buf, pos)?))),
        TAG_JSON => {
            let bytes = read_len_prefixed(buf, pos)?;
            let j = serde_json::from_slice(bytes)
                .map_err(|_| Error::Corrupt("invalid json value".into()))?;
            Ok(Value::Json(j))
        }
        TAG_VECTOR => {
            let count = read_u32(buf, pos)? as usize;
            if count.checked_mul(4).is_none_or(|bytes| *pos + bytes > buf.len()) {
                return Err(Error::Corrupt("truncated vector value".into()));
            }
            let mut v = Vec::with_capacity(count);
            for _ in 0..count {
                v.push(f32::from_le_bytes(read_array(buf, pos)?));
            }
            Ok(Value::Vector(v))
        }
        TAG_DATE => Ok(Value::Date(i32::from_le_bytes(read_array(buf, pos)?))),
        TAG_TIME => Ok(Value::Time(i64::from_le_bytes(read_array(buf, pos)?))),
        TAG_BLOBREF => {
            let r = read_blob_ref(buf, pos)?;
            match blobs {
                Some(dir) => Ok(Value::Blob(read_blob_file(dir, &r)?)),
                None => Err(Error::Corrupt(
                    "blob reference in a context without a blobs directory".into(),
                )),
            }
        }
        other => Err(Error::Corrupt(format!("unknown value tag {other}"))),
    }
}

/// Skip one tagged value without materializing it; collects blob references
/// into `refs` when provided (used by compaction GC and check()).
pub(crate) fn skip_value(
    buf: &[u8],
    pos: &mut usize,
    refs: Option<&mut Vec<BlobRef>>,
) -> Result<()> {
    let tag = read_u8(buf, pos)?;
    match tag {
        TAG_NULL => {}
        TAG_BOOL => {
            read_u8(buf, pos)?;
        }
        TAG_INT64 | TAG_FLOAT64 | TAG_TIMESTAMP | TAG_TIME => {
            read_array::<8>(buf, pos)?;
        }
        TAG_DATE => {
            read_array::<4>(buf, pos)?;
        }
        TAG_TEXT | TAG_BLOB | TAG_JSON => {
            read_len_prefixed(buf, pos)?;
        }
        TAG_VECTOR => {
            let count = read_u32(buf, pos)? as usize;
            let bytes = count
                .checked_mul(4)
                .ok_or_else(|| Error::Corrupt("length overflow".into()))?;
            if *pos + bytes > buf.len() {
                return Err(Error::Corrupt("truncated vector value".into()));
            }
            *pos += bytes;
        }
        TAG_BLOBREF => {
            let r = read_blob_ref(buf, pos)?;
            if let Some(refs) = refs {
                refs.push(r);
            }
        }
        other => return Err(Error::Corrupt(format!("unknown value tag {other}"))),
    }
    Ok(())
}

// --- Cursor helpers shared by the record and segment codecs ----------------

pub(crate) fn read_u8(buf: &[u8], pos: &mut usize) -> Result<u8> {
    let b = *buf
        .get(*pos)
        .ok_or_else(|| Error::Corrupt("unexpected end of data".into()))?;
    *pos += 1;
    Ok(b)
}

pub(crate) fn read_array<const N: usize>(buf: &[u8], pos: &mut usize) -> Result<[u8; N]> {
    let end = pos
        .checked_add(N)
        .ok_or_else(|| Error::Corrupt("length overflow".into()))?;
    let slice = buf
        .get(*pos..end)
        .ok_or_else(|| Error::Corrupt("unexpected end of data".into()))?;
    *pos = end;
    Ok(slice.try_into().expect("slice length checked"))
}

pub(crate) fn read_u16(buf: &[u8], pos: &mut usize) -> Result<u16> {
    Ok(u16::from_le_bytes(read_array(buf, pos)?))
}

pub(crate) fn read_u32(buf: &[u8], pos: &mut usize) -> Result<u32> {
    Ok(u32::from_le_bytes(read_array(buf, pos)?))
}

pub(crate) fn read_u64(buf: &[u8], pos: &mut usize) -> Result<u64> {
    Ok(u64::from_le_bytes(read_array(buf, pos)?))
}

pub(crate) fn read_len_prefixed<'a>(buf: &'a [u8], pos: &mut usize) -> Result<&'a [u8]> {
    let len = read_u32(buf, pos)? as usize;
    let end = pos
        .checked_add(len)
        .ok_or_else(|| Error::Corrupt("length overflow".into()))?;
    let slice = buf
        .get(*pos..end)
        .ok_or_else(|| Error::Corrupt("unexpected end of data".into()))?;
    *pos = end;
    Ok(slice)
}
