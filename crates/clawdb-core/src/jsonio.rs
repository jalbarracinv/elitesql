//! JSON marshaling used by the C ABI, the CLI, the sidecar protocol and
//! import/export. Scalar values map to native JSON; types JSON cannot carry
//! natively use a tagged object `{"$t": "...", ...}` that round-trips.

use serde_json::{json, Map, Value as J};

use crate::error::{Error, Result};
use crate::value::{parse_date_str, parse_time_str, parse_timestamp_str, ymd_from_days};
use crate::{ColumnType, QueryOutput, Record, Value};

pub fn format_date(days: i32) -> String {
    let (y, m, d) = ymd_from_days(days);
    format!("{y:04}-{m:02}-{d:02}")
}

pub fn format_time(micros: i64) -> String {
    let s = micros.div_euclid(1_000_000);
    let frac = micros.rem_euclid(1_000_000);
    let (h, m, sec) = (s / 3600, (s / 60) % 60, s % 60);
    if frac == 0 {
        format!("{h:02}:{m:02}:{sec:02}")
    } else {
        format!("{h:02}:{m:02}:{sec:02}.{frac:06}")
    }
}

pub fn format_timestamp(micros: i64) -> String {
    let days = micros.div_euclid(86_400_000_000);
    let tod = micros.rem_euclid(86_400_000_000);
    format!("{} {}Z", format_date(days as i32), format_time(tod))
}

pub fn value_to_json(v: &Value) -> J {
    match v {
        Value::Null => J::Null,
        Value::Bool(b) => json!(b),
        Value::Int64(n) => json!(n),
        Value::Float64(f) => match serde_json::Number::from_f64(*f) {
            Some(n) => J::Number(n),
            None => json!({"$t": "float64", "repr": f.to_string()}),
        },
        Value::Text(s) => json!(s),
        Value::Blob(b) => json!({"$t": "blob", "hex": hex_encode(b)}),
        Value::Timestamp(us) => json!({"$t": "timestamp", "us": us, "iso": format_timestamp(*us)}),
        Value::Json(j) => json!({"$t": "json", "v": j}),
        Value::Vector(v) => {
            json!({"$t": "vector", "v": v.iter().map(|x| *x as f64).collect::<Vec<_>>()})
        }
        Value::Date(days) => json!({"$t": "date", "days": days, "iso": format_date(*days)}),
        Value::Time(us) => json!({"$t": "time", "us": us, "iso": format_time(*us)}),
    }
}

/// Generic JSON -> Value: native scalars plus the tagged forms produced by
/// `value_to_json`. Untagged arrays/objects become `Value::Json`.
pub fn json_to_value(j: &J) -> Result<Value> {
    Ok(match j {
        J::Null => Value::Null,
        J::Bool(b) => Value::Bool(*b),
        J::Number(n) => {
            if let Some(i) = n.as_i64() {
                Value::Int64(i)
            } else {
                Value::Float64(n.as_f64().unwrap_or(f64::NAN))
            }
        }
        J::String(s) => Value::Text(s.clone()),
        J::Object(map) if map.contains_key("$t") => tagged_to_value(map)?,
        other => Value::Json(other.clone()),
    })
}

fn tagged_to_value(map: &Map<String, J>) -> Result<Value> {
    let tag = map.get("$t").and_then(|t| t.as_str()).unwrap_or_default();
    let bad = |msg: &str| Error::InvalidArgument(format!("invalid tagged value ({tag}): {msg}"));
    Ok(match tag {
        "blob" => {
            let hex = map.get("hex").and_then(|h| h.as_str()).ok_or_else(|| bad("missing hex"))?;
            Value::Blob(hex_decode(hex).ok_or_else(|| bad("bad hex"))?)
        }
        "timestamp" => match (map.get("us").and_then(|n| n.as_i64()), map.get("iso").and_then(|s| s.as_str())) {
            (Some(us), _) => Value::Timestamp(us),
            (None, Some(iso)) => Value::Timestamp(
                parse_timestamp_str(iso.trim_end_matches('Z').trim())
                    .ok_or_else(|| bad("bad iso"))?,
            ),
            _ => return Err(bad("missing us/iso")),
        },
        "date" => match (map.get("days").and_then(|n| n.as_i64()), map.get("iso").and_then(|s| s.as_str())) {
            (Some(days), _) => Value::Date(days as i32),
            (None, Some(iso)) => Value::Date(parse_date_str(iso).ok_or_else(|| bad("bad iso"))?),
            _ => return Err(bad("missing days/iso")),
        },
        "time" => match (map.get("us").and_then(|n| n.as_i64()), map.get("iso").and_then(|s| s.as_str())) {
            (Some(us), _) => Value::Time(us),
            (None, Some(iso)) => Value::Time(parse_time_str(iso).ok_or_else(|| bad("bad iso"))?),
            _ => return Err(bad("missing us/iso")),
        },
        "json" => Value::Json(map.get("v").cloned().ok_or_else(|| bad("missing v"))?),
        "vector" => {
            let arr = map.get("v").and_then(|v| v.as_array()).ok_or_else(|| bad("missing v"))?;
            let mut out = Vec::with_capacity(arr.len());
            for x in arr {
                out.push(x.as_f64().ok_or_else(|| bad("non-numeric component"))? as f32);
            }
            Value::Vector(out)
        }
        "float64" => {
            let repr = map.get("repr").and_then(|r| r.as_str()).ok_or_else(|| bad("missing repr"))?;
            Value::Float64(repr.parse().map_err(|_| bad("bad repr"))?)
        }
        other => return Err(Error::InvalidArgument(format!("unknown value tag '{other}'"))),
    })
}

/// Schema-aware JSON -> Value for imports: the column type disambiguates
/// natural encodings ("2026-08-07" for a date column, a number for a
/// timestamp, a bare array for a vector).
pub fn json_to_value_for_type(j: &J, ty: ColumnType) -> Result<Value> {
    let mismatch = |j: &J| {
        Error::InvalidArgument(format!("value {j} is not valid for a {ty} column"))
    };
    if j.is_null() {
        return Ok(Value::Null);
    }
    let v = match (ty, j) {
        (ColumnType::Bool, J::Bool(b)) => Value::Bool(*b),
        (ColumnType::Int64, J::Number(n)) => Value::Int64(n.as_i64().ok_or_else(|| mismatch(j))?),
        (ColumnType::Float64, J::Number(n)) => Value::Float64(n.as_f64().ok_or_else(|| mismatch(j))?),
        (ColumnType::Text, J::String(s)) => Value::Text(s.clone()),
        (ColumnType::Timestamp, J::Number(n)) => Value::Timestamp(n.as_i64().ok_or_else(|| mismatch(j))?),
        (ColumnType::Timestamp, J::String(s)) => {
            Value::parse_timestamp(s.trim_end_matches('Z')).ok_or_else(|| mismatch(j))?
        }
        (ColumnType::Date, J::Number(n)) => Value::Date(n.as_i64().ok_or_else(|| mismatch(j))? as i32),
        (ColumnType::Date, J::String(s)) => Value::parse_date(s).ok_or_else(|| mismatch(j))?,
        (ColumnType::Time, J::Number(n)) => Value::Time(n.as_i64().ok_or_else(|| mismatch(j))?),
        (ColumnType::Time, J::String(s)) => Value::parse_time(s).ok_or_else(|| mismatch(j))?,
        (ColumnType::Json, other) if !matches!(other, J::Object(m) if m.contains_key("$t")) => {
            Value::Json(other.clone())
        }
        (ColumnType::Vector, J::Array(arr)) => {
            let mut out = Vec::with_capacity(arr.len());
            for x in arr {
                out.push(x.as_f64().ok_or_else(|| mismatch(j))? as f32);
            }
            Value::Vector(out)
        }
        (ColumnType::Blob, J::String(s)) => Value::Blob(hex_decode(s).ok_or_else(|| mismatch(j))?),
        _ => {
            // Fall back to the tagged forms, then verify the type matches.
            let v = json_to_value(j)?;
            if !v.matches(ty) {
                return Err(mismatch(j));
            }
            v
        }
    };
    Ok(v)
}

/// A full record (including "id") as a plain JSON object.
pub fn record_to_json(record: &Record) -> J {
    let mut map = Map::new();
    for (k, v) in record {
        map.insert(k.clone(), value_to_json(v));
    }
    J::Object(map)
}

/// The result of `Db::query` as JSON — the shape used by the C ABI and the
/// sidecar protocol.
pub fn output_to_json(out: &QueryOutput) -> J {
    match out {
        QueryOutput::Rows { columns, rows } => json!({
            "columns": columns,
            "rows": rows
                .iter()
                .map(|r| r.iter().map(value_to_json).collect::<Vec<_>>())
                .collect::<Vec<_>>(),
        }),
        QueryOutput::Inserted { ids } => json!({"inserted": ids}),
        QueryOutput::Affected(n) => json!({"affected": n}),
        QueryOutput::None => json!({"ok": true}),
    }
}

/// Create a vector index from a JSON params object (shared by the C ABI and
/// the sidecar): {"table","column","metric"?: "cosine"|"dot"|"l2",
/// "mode"?: "sync"|"async", "m"?, "ef_construction"?}.
pub fn create_vector_index_json(db: &crate::Db, params: &J) -> Result<J> {
    let table = params
        .get("table")
        .and_then(|v| v.as_str())
        .ok_or_else(|| Error::InvalidArgument("missing 'table'".into()))?;
    let column = params
        .get("column")
        .and_then(|v| v.as_str())
        .ok_or_else(|| Error::InvalidArgument("missing 'column'".into()))?;
    let mut opts = crate::VectorIndexOptions::default();
    match params.get("metric").and_then(|m| m.as_str()) {
        None => {}
        Some("cosine") => opts.metric = crate::VectorMetric::Cosine,
        Some("dot") => opts.metric = crate::VectorMetric::Dot,
        Some("l2") => opts.metric = crate::VectorMetric::L2,
        Some(other) => {
            return Err(Error::InvalidArgument(format!("unknown metric '{other}'")))
        }
    }
    match params.get("mode").and_then(|m| m.as_str()) {
        None => {}
        Some("sync") => opts.mode = crate::IndexingMode::Sync,
        Some("async") => opts.mode = crate::IndexingMode::Async,
        Some(other) => return Err(Error::InvalidArgument(format!("unknown mode '{other}'"))),
    }
    if let Some(m) = params.get("m").and_then(|v| v.as_u64()) {
        opts.m = m as usize;
    }
    if let Some(efc) = params.get("ef_construction").and_then(|v| v.as_u64()) {
        opts.ef_construction = efc as usize;
    }
    if let Some(q) = params.get("quantized").and_then(|v| v.as_bool()) {
        opts.quantized = q;
    }
    db.create_vector_index(table, column, opts)?;
    Ok(json!(true))
}

/// Create a full-text index from a JSON params object: {"table","column"}.
pub fn create_text_index_json(db: &crate::Db, params: &J) -> Result<J> {
    let table = params
        .get("table")
        .and_then(|v| v.as_str())
        .ok_or_else(|| Error::InvalidArgument("missing 'table'".into()))?;
    let column = params
        .get("column")
        .and_then(|v| v.as_str())
        .ok_or_else(|| Error::InvalidArgument("missing 'column'".into()))?;
    db.create_text_index(table, column)?;
    Ok(json!(true))
}

fn filter_from_json(params: &J) -> Result<Option<Record>> {
    match params.get("filter").and_then(|f| f.as_object()) {
        None => Ok(None),
        Some(filter) => {
            let mut rec = Record::new();
            for (k, v) in filter {
                rec.insert(k.clone(), json_to_value(v)?);
            }
            Ok(Some(rec))
        }
    }
}

/// BM25 full-text search: {"table","column","query","top_k"?,
/// "filter"?: {col: value}} -> {"hits":[{"id","score","record"}]}.
pub fn search_text_json(db: &crate::Db, params: &J) -> Result<J> {
    let table = params
        .get("table")
        .and_then(|v| v.as_str())
        .ok_or_else(|| Error::InvalidArgument("missing 'table'".into()))?;
    let column = params
        .get("column")
        .and_then(|v| v.as_str())
        .ok_or_else(|| Error::InvalidArgument("missing 'column'".into()))?;
    let query = params
        .get("query")
        .and_then(|v| v.as_str())
        .ok_or_else(|| Error::InvalidArgument("missing 'query'".into()))?;
    let top_k = params.get("top_k").and_then(|v| v.as_u64()).unwrap_or(10) as usize;
    let filter = filter_from_json(params)?;
    let hits = db.search_text(table, column, query, top_k, filter.as_ref())?;
    let hits_json: Vec<J> = hits
        .iter()
        .map(|h| {
            json!({"id": h.id, "score": h.score, "record": record_to_json(&h.record)})
        })
        .collect();
    Ok(json!({ "hits": hits_json }))
}

/// Hybrid (RRF) search: {"table","top_k"?,"ef_search"?,"filter"?,
/// "text"?: {"column","query"}, "vector"?: {"column","vector":[..]}}.
pub fn search_hybrid_json(db: &crate::Db, params: &J) -> Result<J> {
    let table = params
        .get("table")
        .and_then(|v| v.as_str())
        .ok_or_else(|| Error::InvalidArgument("missing 'table'".into()))?;
    let text = match params.get("text").and_then(|t| t.as_object()) {
        None => None,
        Some(t) => Some((
            t.get("column")
                .and_then(|v| v.as_str())
                .ok_or_else(|| Error::InvalidArgument("text: missing 'column'".into()))?,
            t.get("query")
                .and_then(|v| v.as_str())
                .ok_or_else(|| Error::InvalidArgument("text: missing 'query'".into()))?,
        )),
    };
    let vector_data: Option<(&str, Vec<f32>)> = match params.get("vector").and_then(|v| v.as_object()) {
        None => None,
        Some(v) => {
            let column = v
                .get("column")
                .and_then(|c| c.as_str())
                .ok_or_else(|| Error::InvalidArgument("vector: missing 'column'".into()))?;
            let vec: Vec<f32> = v
                .get("vector")
                .and_then(|a| a.as_array())
                .ok_or_else(|| Error::InvalidArgument("vector: missing 'vector'".into()))?
                .iter()
                .map(|x| x.as_f64().map(|f| f as f32))
                .collect::<Option<_>>()
                .ok_or_else(|| Error::InvalidArgument("vector: non-numeric component".into()))?;
            Some((column, vec))
        }
    };
    let query = crate::HybridQuery {
        text,
        vector: vector_data.as_ref().map(|(c, v)| (*c, v.as_slice())),
        top_k: params.get("top_k").and_then(|v| v.as_u64()).unwrap_or(10) as usize,
        ef_search: params.get("ef_search").and_then(|v| v.as_u64()).map(|n| n as usize),
        filter: filter_from_json(params)?,
    };
    let hits = db.search_hybrid(table, &query)?;
    let hits_json: Vec<J> = hits
        .iter()
        .map(|h| {
            json!({"id": h.id, "score": h.score, "record": record_to_json(&h.record)})
        })
        .collect();
    Ok(json!({ "hits": hits_json }))
}

/// Run an ANN search described by a JSON params object — the shared shape
/// of the C ABI and the sidecar protocol: {"table","column","vector":[...],
/// "top_k"?, "ef_search"?, "filter"?: {col: value}}.
pub fn search_vector_json(db: &crate::Db, params: &J) -> Result<J> {
    let table = params
        .get("table")
        .and_then(|v| v.as_str())
        .ok_or_else(|| Error::InvalidArgument("missing 'table'".into()))?;
    let column = params
        .get("column")
        .and_then(|v| v.as_str())
        .ok_or_else(|| Error::InvalidArgument("missing 'column'".into()))?;
    let vector: Vec<f32> = params
        .get("vector")
        .and_then(|v| v.as_array())
        .ok_or_else(|| Error::InvalidArgument("missing 'vector'".into()))?
        .iter()
        .map(|x| x.as_f64().map(|f| f as f32))
        .collect::<Option<_>>()
        .ok_or_else(|| Error::InvalidArgument("non-numeric vector component".into()))?;
    let top_k = params.get("top_k").and_then(|v| v.as_u64()).unwrap_or(10) as usize;
    let mut opts = crate::VectorSearchOptions {
        ef_search: params.get("ef_search").and_then(|v| v.as_u64()).map(|n| n as usize),
        filter: None,
    };
    if let Some(filter) = params.get("filter").and_then(|f| f.as_object()) {
        let mut rec = Record::new();
        for (k, v) in filter {
            rec.insert(k.clone(), json_to_value(v)?);
        }
        opts.filter = Some(rec);
    }
    let hits = db.search_vector(table, column, &vector, top_k, &opts)?;
    let hits_json: Vec<J> = hits
        .iter()
        .map(|h| {
            json!({
                "id": h.id,
                "distance": h.distance,
                "record": record_to_json(&h.record),
            })
        })
        .collect();
    Ok(json!({ "hits": hits_json }))
}

fn hex_encode(bytes: &[u8]) -> String {
    let mut s = String::with_capacity(bytes.len() * 2);
    for b in bytes {
        s.push_str(&format!("{b:02x}"));
    }
    s
}

fn hex_decode(s: &str) -> Option<Vec<u8>> {
    if !s.len().is_multiple_of(2) {
        return None;
    }
    let bytes = s.as_bytes();
    let mut out = Vec::with_capacity(bytes.len() / 2);
    for pair in bytes.chunks(2) {
        let hi = (pair[0] as char).to_digit(16)?;
        let lo = (pair[1] as char).to_digit(16)?;
        out.push((hi * 16 + lo) as u8);
    }
    Some(out)
}
