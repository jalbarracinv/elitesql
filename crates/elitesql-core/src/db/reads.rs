//! Reads internals; shared state and lock ownership remain in db.rs.
use super::*;

pub(super) fn shared_get_at(
    shared: &Shared,
    table: &str,
    id: &str,
    max_version: u64,
) -> Result<Option<Record>> {
    shared_get_at_keep(shared, table, id, max_version, None)
}

/// `shared_get_at` decoding only the columns in `keep`.
pub(super) fn shared_get_at_keep(
    shared: &Shared,
    table: &str,
    id: &str,
    max_version: u64,
    keep: Option<&[&str]>,
) -> Result<Option<Record>> {
    // Through the read view when it covers the version asked for: no state
    // lock, and so no admission throttle either, which only exists to keep
    // point reads from crowding a committer out of that lock.
    if let Some(view) = view_covering(shared, max_version) {
        if let (Some(schema), Some(directory)) = (view.schemas.get(table), view.tables.get(table)) {
            let mut key = Vec::new();
            let entry = directory
                .view(table, &mut key)
                .newest(id, max_version)?
                .filter(|entry| entry.version > schema.epoch && !entry.is_tombstone());
            let Some(entry) = entry else {
                return Ok(None);
            };
            let projection = RowProjection::new(Some(schema), keep);
            let mut record = decode_entry(shared, &view.readers, &entry.kind, &projection)?;
            if let Some(record) = record.as_mut() {
                if schema.has_implicit_id() {
                    record.insert(ID_COLUMN, Value::Text(id.to_owned()));
                }
            }
            return Ok(record);
        }
    }
    let _admission = PointReadAdmission::enter(shared);
    let st = shared.state.read().unwrap();
    let schema = st
        .catalog
        .table(table)
        .ok_or_else(|| Error::TableNotFound(table.into()))?;
    let implicit_id = schema.has_implicit_id();
    let projection = RowProjection::new(Some(schema), keep);
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
        VKind::MemPut(payload) => decode_record_keep(payload, blobs, &projection)?,
        VKind::SegPut {
            payload_offset,
            payload_len,
            ..
        } => reader
            .as_deref()
            .expect("segment reader captured above")
            .with_payload(*payload_offset, *payload_len, |bytes| {
                decode_record_keep(bytes, blobs, &projection)
            })?,
        VKind::MemTombstone | VKind::SegTombstone => return Ok(None),
    };
    if implicit_id {
        record.insert(ID_COLUMN, Value::Text(id.to_owned()));
    }
    Ok(Some(record))
}

/// The record a directory entry holds, decoded with `projection`; `None`
/// for a tombstone.
fn decode_entry(
    shared: &Shared,
    readers: &SegmentReaders,
    kind: &VKind,
    projection: &RowProjection<'_>,
) -> Result<Option<Record>> {
    let blobs = Some(shared.blobs.as_path());
    Ok(Some(match kind {
        VKind::MemPut(payload) => decode_record_keep(payload, blobs, projection)?,
        VKind::SegPut {
            segment,
            payload_offset,
            payload_len,
        } => readers
            .get(segment)
            .ok_or_else(|| Error::Corrupt(format!("missing segment {segment}")))?
            .with_payload(*payload_offset, *payload_len, |bytes| {
                decode_record_keep(bytes, blobs, projection)
            })?,
        VKind::MemTombstone | VKind::SegTombstone => return Ok(None),
    }))
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

/// Rows of the primary directory are visited in chunks of this many ids.
///
/// Chunks used to be where the shared state lock was released, so that a
/// committer waited for at most one chunk of directory visits; one of 1 024
/// rows measured best from 10 to 500 connections. The walk now runs over a
/// detached snapshot of the directory (`PrimaryIdx::table_snapshot`) without
/// the lock, and a chunk only bounds the ids buffered before the filter and
/// the batch limit are applied.
const SCAN_LOCK_CHUNK_ROWS: usize = 1024;

pub(super) fn shared_scan_batch_at_bytes(
    shared: &Shared,
    table: &str,
    max_version: u64,
    after_id: Option<&str>,
    limit: usize,
    max_bytes: Option<usize>,
) -> Result<Vec<(String, Record)>> {
    shared_scan_batch_at_bytes_filtered(
        shared,
        table,
        max_version,
        after_id,
        limit,
        max_bytes,
        None,
        None,
    )
}

/// One batch of a scan: the rows, the id of each row when the caller asked
/// for them, and the cursor to resume after.
///
/// A SQL scan does not want the ids: it resumes after the last row of the
/// batch and throws the rest away. Handing one `String` per row back cost an
/// allocation per row scanned, which on a `COUNT(*)` over a published table
/// was a quarter of what reaching the row cost at all.
pub(crate) struct ScanBatch {
    pub(crate) rows: Vec<Record>,
    /// Empty unless the caller asked for ids; otherwise as long as `rows`.
    pub(crate) ids: Vec<String>,
    /// Id to resume strictly after, or `None` when the batch is empty.
    pub(crate) next: Option<String>,
    /// When the caller asked for them, what each row was decoded from, so
    /// that more of its columns can be decoded later from the very same
    /// version; see `RowHandles`.
    pub(crate) handles: Option<Box<RowHandles>>,
}

/// The stored version behind each row of a batch, with the segment handles
/// its payload lives in. A statement that sorts many rows and returns a few
/// decodes only the sort keys for all of them and the rest of the columns
/// for the few, from the same payloads: no second lookup, and nothing a
/// commit in between could change.
pub(crate) struct RowHandles {
    pub(super) blobs: PathBuf,
    pub(super) readers: SegmentReaders,
    pub(super) schemas: Arc<SchemaMap>,
    pub(super) table: String,
    pub(super) kinds: Vec<VKind>,
    /// The id of each row, for tables whose id is not a stored column.
    pub(super) ids: Vec<String>,
}

impl RowHandles {
    /// Row `index` of the batch, decoded with `keep`.
    pub(crate) fn decode(&self, index: usize, keep: Option<&[&str]>) -> Result<Record> {
        let schema = self
            .schemas
            .get(&self.table)
            .ok_or_else(|| Error::TableNotFound(self.table.clone()))?;
        let projection = RowProjection::new(Some(schema), keep);
        let mut record =
            read_record_kind_keep(&self.blobs, &self.readers, &self.kinds[index], &projection)?;
        if schema.has_implicit_id() {
            record.insert(ID_COLUMN, Value::Text(self.ids[index].clone()));
        }
        Ok(record)
    }
}

/// Up to `limit` visible rows after `after_id`, decoded from retained segment
/// handles with the state lock released. The directory is walked over a
/// snapshot taken with the lock held only for that, and an optional `ScanFilter` is
/// evaluated on each encoded row before it counts toward `limit` or gets
/// decoded, so a selective range scan neither holds the lock nor
/// materializes the rows it rejects. The snapshot version keeps the chunks
/// consistent with each other.
///
/// Ids are held in two byte arenas rather than one `String` per row: one for
/// the chunk being visited under the lock, one for the rows that survived the
/// filter. Both are reused across chunks, so a scan allocates per batch
/// instead of per row.
#[allow(clippy::too_many_arguments)]
pub(super) fn shared_scan_batch(
    shared: &Shared,
    table: &str,
    max_version: u64,
    after_id: Option<&str>,
    limit: usize,
    max_bytes: Option<usize>,
    filter: Option<&ScanFilter>,
    keep: Option<&[&str]>,
    want_ids: bool,
) -> Result<ScanBatch> {
    let empty = || ScanBatch {
        rows: Vec::new(),
        ids: Vec::new(),
        next: None,
        handles: None,
    };
    if limit == 0 {
        return Ok(empty());
    }
    let ScanParts {
        epoch,
        implicit_id,
        blobs,
        filter_ordinal,
        projection,
        directory,
        all_readers,
    } = scan_parts(shared, table, max_version, filter, keep)?;
    let limit = max_bytes.map_or(limit, |bytes| limit.min((bytes / 256).max(1)));

    // Ids of the rows that passed the filter, packed end to end; `prepared`
    // holds the span of each one.
    let mut prepared_ids = String::new();
    let mut prepared: Vec<(IdSpan, VKind)> = Vec::with_capacity(limit.min(1024));
    let mut readers = SegmentReaders::default();
    let mut cursor: Option<String> = after_id.map(str::to_owned);
    // The id the directory walk last looked at, visible or not. It is the
    // only id a chunk has to remember, and it is rewritten in place.
    let mut last_visited = String::new();
    let mut chunk_ids = String::new();
    let mut chunk_entries: Vec<(IdSpan, VKind)> = Vec::with_capacity(SCAN_LOCK_CHUNK_ROWS);
    loop {
        let chunk = SCAN_LOCK_CHUNK_ROWS.min(limit - prepared.len()).max(1);

        let mut visited = 0usize;
        chunk_entries.clear();
        chunk_ids.clear();
        {
            directory.visit(table, cursor.as_deref(), |id, versions| {
                visited += 1;
                // Every id visited is a candidate cursor, so the walk can
                // resume past a run of invisible or filtered rows.
                last_visited.clear();
                last_visited.push_str(id);
                let Some(entry) = versions
                    .iter()
                    .rev()
                    .find(|entry| entry.version <= max_version && entry.version > epoch)
                    .filter(|entry| !entry.is_tombstone())
                else {
                    return Ok(visited < chunk);
                };
                let span = push_id(&mut chunk_ids, id);
                chunk_entries.push((span, entry.kind.clone()));
                Ok(visited < chunk)
            })?;
            for (_, kind) in &chunk_entries {
                if let VKind::SegPut { segment, .. } = kind {
                    if !readers.contains_key(segment) {
                        let reader = all_readers
                            .get(segment)
                            .ok_or_else(|| Error::Corrupt(format!("missing segment {segment}")))?;
                        readers.insert(*segment, reader.clone());
                    }
                }
            }
        }
        for (span, kind) in chunk_entries.drain(..) {
            if let Some(filter) = filter {
                let may_match = with_payload(&readers, &kind, |payload| {
                    scan_filter_may_match(payload, filter, filter_ordinal, &blobs)
                })?
                .unwrap_or(false);
                if !may_match {
                    continue;
                }
            }
            let span = push_id(&mut prepared_ids, span.of(&chunk_ids));
            prepared.push((span, kind));
            if prepared.len() >= limit {
                break;
            }
        }
        let exhausted = visited < chunk;
        if exhausted || prepared.len() >= limit {
            break;
        }
        cursor = Some(last_visited.clone());
    }

    let mut rows = Vec::with_capacity(prepared.len());
    let mut ids = Vec::with_capacity(if want_ids { prepared.len() } else { 0 });
    let mut retained_bytes = 0usize;
    // Set when the byte budget cut the batch short: the caller must resume at
    // the last row it actually received, not past the ones left behind.
    let mut truncated = false;
    for (span, kind) in &prepared {
        let id = span.of(&prepared_ids);
        if let Some(budget) = max_bytes {
            let fits = with_payload(&readers, kind, |payload| {
                let mut refs = Vec::new();
                scan_payload_blob_refs(payload, &mut refs)?;
                let estimate = refs.iter().fold(payload.len(), |total, reference| {
                    total.saturating_add(reference.size as usize)
                });
                Ok(estimate <= budget)
            })?
            .unwrap_or(false);
            if !fits {
                if rows.is_empty() {
                    return Err(Error::MemoryLimit(
                        "one row exceeds scan byte budget".into(),
                    ));
                }
                truncated = true;
                break;
            }
        }
        let mut record = read_record_kind_keep(&blobs, &readers, kind, &projection)?;
        if implicit_id {
            record.insert(ID_COLUMN, Value::Text(id.to_owned()));
        }
        if let Some(budget) = max_bytes {
            let bytes = decoded_record_bytes(&record)
                .saturating_add(id.len())
                .saturating_add(64);
            if retained_bytes.saturating_add(bytes) > budget {
                if rows.is_empty() {
                    return Err(Error::MemoryLimit(
                        "one decoded row exceeds scan byte budget".into(),
                    ));
                }
                truncated = true;
                break;
            }
            retained_bytes += bytes;
        }
        if want_ids {
            ids.push(id.to_owned());
        }
        rows.push(record);
    }
    let next = if rows.is_empty() {
        None
    } else if truncated {
        // Resume at the last row handed over; the rest of `prepared` was not.
        Some(prepared[rows.len() - 1].0.of(&prepared_ids).to_owned())
    } else {
        // Nothing between the last row and here can produce a visible row the
        // caller has not seen: the walk either found no version or the filter
        // ruled the row out, and the caller re-checks the filter anyway.
        Some(last_visited)
    };
    Ok(ScanBatch {
        rows,
        ids,
        next,
        handles: None,
    })
}

/// What a scan needs from the committed state, taken once: from the read
/// view when it covers the version asked for, so the scan takes no lock at
/// all, and otherwise with the state lock held only for this.
struct ScanParts<'k> {
    epoch: u64,
    implicit_id: bool,
    blobs: PathBuf,
    filter_ordinal: Option<usize>,
    projection: RowProjection<'k>,
    directory: PrimaryTableSnapshot,
    all_readers: Arc<SegmentReaders>,
}

fn scan_parts<'k>(
    shared: &Shared,
    table: &str,
    max_version: u64,
    filter: Option<&ScanFilter>,
    keep: Option<&'k [&'k str]>,
) -> Result<ScanParts<'k>> {
    // The directory and the segment handles are taken once, with the lock
    // held only for that; the walk below runs without it. A committer used to
    // wait for every chunk of every scan in progress, and scans were the
    // largest share of what committers waited for.
    let filter_ordinal_in = |schema: &TableSchema| {
        filter.and_then(|filter| {
            schema
                .columns
                .iter()
                .position(|column| column.name == filter.column)
        })
    };
    // From the read view when it covers the version asked for, so the scan
    // takes no lock at all.
    let view = view_covering(shared, max_version);
    let from_view = view
        .as_ref()
        .and_then(|view| {
            view.schemas
                .get(table)
                .zip(view.tables.get(table))
                .map(|parts| (view, parts))
        })
        .map(|(view, (schema, directory))| {
            (
                schema.epoch,
                schema.has_implicit_id(),
                shared.blobs.clone(),
                filter_ordinal_in(schema),
                RowProjection::new(Some(schema), keep),
                directory.clone(),
                view.readers.clone(),
            )
        });
    let (epoch, implicit_id, blobs, filter_ordinal, projection, directory, all_readers) =
        if let Some(parts) = from_view {
            parts
        } else {
            let st = shared.state.read().unwrap();

            let schema = st
                .catalog
                .table(table)
                .ok_or_else(|| Error::TableNotFound(table.into()))?;
            let ordinal = filter.and_then(|filter| {
                schema
                    .columns
                    .iter()
                    .position(|column| column.name == filter.column)
            });
            (
                schema.epoch,
                schema.has_implicit_id(),
                st.blobs.clone(),
                ordinal,
                // Resolved once for the whole batch: with the column names out of
                // the payload, deciding what to decode is no longer per row.
                RowProjection::new(Some(schema), keep),
                st.index.table_snapshot(table),
                st.readers.clone(),
            )
        };
    drop(view);
    Ok(ScanParts {
        epoch,
        implicit_id,
        blobs,
        filter_ordinal,
        projection,
        directory,
        all_readers,
    })
}

/// Rows a streaming scan visits over one directory snapshot before it takes
/// a fresh one and resumes after the last id. A snapshot keeps the delta
/// chunks and segment handles it saw alive; renewing it bounds what a long
/// scan with a slow consumer holds back from checkpoints and compaction,
/// while one resume per this many rows costs nothing measurable.
const SCAN_VISIT_SNAPSHOT_ROWS: usize = 16 * 1024;

/// Every visible row of `table` as of `max_version`, in id order, decoded
/// with `keep` and handed to `visit` one at a time; `visit` returns `false`
/// to stop. `filter` rules rows out on their encoded payload before they are
/// decoded, exactly as in `shared_scan_batch`, and the caller still has to
/// re-check it.
///
/// This is the batch scan without the batch: no id is copied, no row is
/// buffered, and the run cursors are opened once per snapshot instead of
/// once per batch. An aggregate over a table visits every row and keeps
/// none of them, and for it the batch machinery was most of the cost: in
/// the SaaS workload a dashboard of three aggregates, one call in a hundred,
/// was 30 % of the server's CPU.
pub(super) fn shared_scan_visit(
    shared: &Shared,
    table: &str,
    max_version: u64,
    filter: Option<&ScanFilter>,
    keep: Option<&[&str]>,
    mut visit: impl FnMut(Record) -> Result<bool>,
) -> Result<()> {
    let mut cursor: Option<String> = None;
    loop {
        let ScanParts {
            epoch,
            implicit_id,
            blobs,
            filter_ordinal,
            projection,
            directory,
            all_readers,
        } = scan_parts(shared, table, max_version, filter, keep)?;
        let mut visited = 0usize;
        let mut stopped = false;
        let mut resume: Option<String> = None;
        directory.visit(table, cursor.as_deref(), |id, versions| {
            visited += 1;
            if visited >= SCAN_VISIT_SNAPSHOT_ROWS {
                resume = Some(id.to_owned());
            }
            let more = || Ok(visited < SCAN_VISIT_SNAPSHOT_ROWS);
            let Some(entry) = versions
                .iter()
                .rev()
                .find(|entry| entry.version <= max_version && entry.version > epoch)
                .filter(|entry| !entry.is_tombstone())
            else {
                return more();
            };
            if let Some(filter) = filter {
                let may_match = with_payload(&all_readers, &entry.kind, |payload| {
                    scan_filter_may_match(payload, filter, filter_ordinal, &blobs)
                })?
                .unwrap_or(false);
                if !may_match {
                    return more();
                }
            }
            let mut record = read_record_kind_keep(&blobs, &all_readers, &entry.kind, &projection)?;
            if implicit_id {
                record.insert(ID_COLUMN, Value::Text(id.to_owned()));
            }
            if !visit(record)? {
                stopped = true;
                return Ok(false);
            }
            more()
        })?;
        match resume {
            Some(id) if !stopped => cursor = Some(id),
            _ => return Ok(()),
        }
    }
}

/// A span of the id arena a batch packs its ids into.
#[derive(Clone, Copy)]
pub(crate) struct IdSpan {
    start: u32,
    end: u32,
}

impl IdSpan {
    pub(crate) fn of<'a>(&self, arena: &'a str) -> &'a str {
        &arena[self.start as usize..self.end as usize]
    }
}

/// Append `id` to `arena` and return its span. Ids are at most 4 GiB apart in
/// one batch by construction: a batch is bounded by `limit`.
pub(crate) fn push_id(arena: &mut String, id: &str) -> IdSpan {
    let start = arena.len() as u32;
    arena.push_str(id);
    IdSpan {
        start,
        end: arena.len() as u32,
    }
}

/// `shared_scan_batch` for the callers that still want one id per row.
#[allow(clippy::too_many_arguments)]
pub(super) fn shared_scan_batch_at_bytes_filtered(
    shared: &Shared,
    table: &str,
    max_version: u64,
    after_id: Option<&str>,
    limit: usize,
    max_bytes: Option<usize>,
    filter: Option<&ScanFilter>,
    keep: Option<&[&str]>,
) -> Result<Vec<(String, Record)>> {
    let batch = shared_scan_batch(
        shared,
        table,
        max_version,
        after_id,
        limit,
        max_bytes,
        filter,
        keep,
        true,
    )?;
    Ok(batch.ids.into_iter().zip(batch.rows).collect())
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
            name.len()
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
