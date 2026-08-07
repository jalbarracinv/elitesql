use serde::{Deserialize, Serialize};
use std::fmt;

use crate::error::{Error, Result};

/// The V1 column types from the spec. `vector<float32, N>` arrives in Phase 3.
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
        };
        f.write_str(s)
    }
}

/// A single field value. `Timestamp` is microseconds since the Unix epoch.
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
        )
    }

    pub fn as_text(&self) -> Option<&str> {
        match self {
            Value::Text(s) => Some(s),
            _ => None,
        }
    }
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
    }
}

pub(crate) fn decode_value(buf: &[u8], pos: &mut usize) -> Result<Value> {
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
        other => Err(Error::Corrupt(format!("unknown value tag {other}"))),
    }
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
