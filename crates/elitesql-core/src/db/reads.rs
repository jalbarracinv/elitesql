//! Reads internals; shared state and lock ownership remain in db.rs.
use super::*;

pub(super) fn shared_get_at(
    shared: &Shared,
    table: &str,
    id: &str,
    max_version: u64,
) -> Result<Option<Record>> {
    let _admission = PointReadAdmission::enter(shared);
    let st = shared.state.read().unwrap();
    let schema = st
        .catalog
        .table(table)
        .ok_or_else(|| Error::TableNotFound(table.into()))?;
    let implicit_id = schema.has_implicit_id();
    let entry = match st.visible_owned(table, id, max_version)? {
        Some(entry) if !entry.is_tombstone() => entry,
        _ => return Ok(None),
    };
    // Retain only the segment handle the payload lives in and release the
    // shared state before touching the mapping. Decoding (page faults, one
    // allocation per column) then runs concurrently with committers waiting
    // for the write lock, exactly like the batched scan path.
    let reader = match &entry.kind {
        VKind::SegPut { segment, .. } => Some(
            st.readers
                .get(segment)
                .cloned()
                .ok_or_else(|| Error::Corrupt(format!("missing segment {segment}")))?,
        ),
        _ => None,
    };
    drop(st);
    let blobs = Some(shared.blobs.as_path());
    let mut record = match &entry.kind {
        VKind::MemPut(payload) => decode_record(payload, blobs)?,
        VKind::SegPut {
            payload_offset,
            payload_len,
            ..
        } => reader
            .as_deref()
            .expect("segment reader captured above")
            .with_payload(*payload_offset, *payload_len, |bytes| {
                decode_record(bytes, blobs)
            })?,
        VKind::MemTombstone | VKind::SegTombstone => return Ok(None),
    };
    if implicit_id {
        record.insert(ID_COLUMN.into(), Value::Text(id.to_owned()));
    }
    Ok(Some(record))
}

pub(super) fn shared_scan_at(
    shared: &Shared,
    table: &str,
    max_version: u64,
) -> Result<Vec<(String, Record)>> {
    let mut out = Vec::new();
    let mut after_id = None;
    let batch_rows = shared.opts.memory.scan_batch_rows.max(1);
    loop {
        let batch =
            shared_scan_batch_at(shared, table, max_version, after_id.as_deref(), batch_rows)?;
        let Some(last_id) = batch.last().map(|(id, _)| id.clone()) else {
            break;
        };
        let complete = batch.len() < batch_rows;
        out.extend(batch);
        after_id = Some(last_id);
        if complete {
            break;
        }
    }
    Ok(out)
}

pub(super) fn shared_scan_batch_at(
    shared: &Shared,
    table: &str,
    max_version: u64,
    after_id: Option<&str>,
    limit: usize,
) -> Result<Vec<(String, Record)>> {
    shared_scan_batch_at_bytes(shared, table, max_version, after_id, limit, None)
}

pub(super) fn shared_scan_batch_at_bytes(
    shared: &Shared,
    table: &str,
    max_version: u64,
    after_id: Option<&str>,
    limit: usize,
    max_bytes: Option<usize>,
) -> Result<Vec<(String, Record)>> {
    let st = shared.state.read().unwrap();
    let schema = st
        .catalog
        .table(table)
        .ok_or_else(|| Error::TableNotFound(table.into()))?;
    let epoch = schema.epoch;
    if limit == 0 {
        return Ok(Vec::new());
    }
    let implicit_id = schema.has_implicit_id();
    let blobs = st.blobs.clone();
    let limit = max_bytes.map_or(limit, |bytes| limit.min((bytes / 256).max(1)));
    let mut prepared = Vec::with_capacity(limit.min(1024));
    st.index.visit_table(table, after_id, |id, versions| {
        let Some(entry) = versions
            .iter()
            .rev()
            .find(|entry| entry.version <= max_version && entry.version > epoch)
        else {
            return Ok(true);
        };
        if entry.is_tombstone() {
            return Ok(true);
        }
        prepared.push((id.to_owned(), entry.kind.clone()));
        if prepared.len() == limit {
            return Ok(false);
        }
        Ok(true)
    })?;
    let mut readers = SegmentReaders::new();
    for (_, kind) in &prepared {
        if let VKind::SegPut { segment, .. } = kind {
            let reader = st
                .readers
                .get(segment)
                .ok_or_else(|| Error::Corrupt(format!("missing segment {segment}")))?;
            readers.entry(*segment).or_insert_with(|| reader.clone());
        }
    }
    drop(st);

    let mut out = Vec::with_capacity(prepared.len());
    let mut retained_bytes = 0usize;
    for (id, kind) in prepared {
        if let Some(budget) = max_bytes {
            let fits = with_payload(&readers, &kind, |payload| {
                let mut refs = Vec::new();
                scan_payload_blob_refs(payload, &mut refs)?;
                let estimate = refs.iter().fold(payload.len(), |total, reference| {
                    total.saturating_add(reference.size as usize)
                });
                Ok(estimate <= budget)
            })?
            .unwrap_or(false);
            if !fits {
                if out.is_empty() {
                    return Err(Error::MemoryLimit(
                        "one row exceeds scan byte budget".into(),
                    ));
                }
                break;
            }
        }
        let mut record = read_record_kind(&blobs, &readers, &kind)?;
        if implicit_id {
            record.insert(ID_COLUMN.into(), Value::Text(id.clone()));
        }
        if let Some(budget) = max_bytes {
            let bytes = decoded_record_bytes(&record)
                .saturating_add(id.capacity())
                .saturating_add(64);
            if retained_bytes.saturating_add(bytes) > budget {
                if out.is_empty() {
                    return Err(Error::MemoryLimit(
                        "one decoded row exceeds scan byte budget".into(),
                    ));
                }
                break;
            }
            retained_bytes += bytes;
        }
        out.push((id, record));
    }
    Ok(out)
}

pub(crate) fn json_heap_bytes(value: &serde_json::Value) -> usize {
    std::mem::size_of::<serde_json::Value>()
        + match value {
            serde_json::Value::String(value) => value.capacity(),
            serde_json::Value::Array(values) => {
                values.iter().map(json_heap_bytes).sum::<usize>()
                    + values.capacity() * std::mem::size_of::<serde_json::Value>()
            }
            serde_json::Value::Object(values) => values
                .iter()
                .map(|(key, value)| key.capacity() + 64 + json_heap_bytes(value))
                .sum(),
            _ => 0,
        }
}

pub(crate) fn decoded_record_bytes(record: &Record) -> usize {
    128 + record
        .iter()
        .map(|(name, value)| {
            name.capacity()
                + 64
                + std::mem::size_of::<Value>()
                + match value {
                    Value::Text(value) => value.capacity(),
                    Value::Blob(value) => value.capacity(),
                    Value::Vector(value) => value.capacity() * 4,
                    Value::Json(value) => json_heap_bytes(value),
                    _ => 0,
                }
        })
        .sum::<usize>()
}

pub(super) fn record_fits_batch(
    record: &Record,
    id: &str,
    budget: Option<usize>,
    retained: &mut usize,
) -> Result<bool> {
    let Some(budget) = budget else {
        return Ok(true);
    };
    let bytes = decoded_record_bytes(record)
        .saturating_add(id.len())
        .saturating_add(64);
    if retained.saturating_add(bytes) > budget {
        if *retained == 0 {
            return Err(Error::MemoryLimit(
                "one decoded row exceeds scan byte budget".into(),
            ));
        }
        return Ok(false);
    }
    *retained += bytes;
    Ok(true)
}
