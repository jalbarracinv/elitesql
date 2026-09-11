//! Logical checks use a separately sorted canonical image, never an index as
//! evidence that a row exists. Sorting spills into a private temporary folder.

use std::collections::BTreeMap;
use std::fs::{self, File};
use std::os::unix::fs::DirBuilderExt;
use std::path::{Path, PathBuf};

use memmap2::MmapOptions;
use ulid::Ulid;

use super::CheckReport;
use crate::db::{decode_record, encode_record_ordered, normalize_record, BLOBS_DIR, SEGMENTS_DIR};
use crate::error::{Error, Result};
use crate::manifest::Manifest;
use crate::paged::{ExternalPagedWriter, PagedIndex, PagedWriter};
use crate::schema::Catalog;
use crate::segment::{segment_file_name, visit_segment};
use crate::value::{encode_value, Value};
use crate::wal::{scan_wal, wal_path};

const SORT_BUDGET: usize = 16 * 1024 * 1024;

struct Scratch(PathBuf);
impl Scratch {
    fn new() -> Result<Self> {
        let path = std::env::temp_dir().join(format!("elitesql-check-{}", Ulid::new()));
        fs::DirBuilder::new().mode(0o700).create(&path)?;
        Ok(Self(path))
    }
}
impl Drop for Scratch {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

fn row_key(table: &str, id: &str) -> Vec<u8> {
    serde_json::to_vec(&(table, id)).expect("strings serialize")
}

fn unique_key(table: &str, column: &str, value: &Value) -> Vec<u8> {
    let mut key = row_key(table, column);
    encode_value(&mut key, value);
    key
}

pub(super) fn check_logical(
    dir: &Path,
    manifest: &Manifest,
    catalog: &Catalog,
    wals: &[u32],
    report: &mut CheckReport,
) -> Result<()> {
    catalog.validate()?;
    let scratch = Scratch::new()?;
    let versions_path = scratch.0.join("versions.pidx");
    let mut versions = ExternalPagedWriter::new(&versions_path, &scratch.0, 0, SORT_BUDGET)?;
    let mut identities = manifest.identity_high_water.clone();
    let mut add = |table: &str, id: &str, version: u64, payload: Option<&[u8]>| -> Result<()> {
        let Some(schema) = catalog.table(table) else {
            return Ok(());
        };
        if version <= schema.epoch {
            return Ok(());
        }
        if id.is_empty() {
            return Err(Error::Corrupt(format!("{table}: empty physical id")));
        }
        let mut key = row_key(table, id);
        key.extend_from_slice(&(!version).to_be_bytes());
        // Valid record payloads always have at least their u16 field count;
        // an empty value therefore unambiguously represents a tombstone.
        if payload.is_some_and(|bytes| bytes.len() < 2) {
            return Err(Error::Corrupt(format!(
                "{table}/{id}: incomplete record payload"
            )));
        }
        versions.add(&key, payload.unwrap_or_default())
    };
    for segment in &manifest.segments {
        let file = File::open(dir.join(SEGMENTS_DIR).join(segment_file_name(segment.id)))?;
        if segment.len == 0 {
            continue;
        }
        // The caller holds the source lock; canonical files cannot change.
        let mapped = unsafe { MmapOptions::new().map(&file) }?;
        visit_segment(&mapped[..segment.len as usize], |entry| {
            let payload = (!entry.tombstone).then(|| {
                &mapped[entry.payload_offset as usize
                    ..entry.payload_offset as usize + entry.payload_len as usize]
            });
            add(&entry.table, &entry.id, entry.version, payload)
        })?;
    }
    for id in wals {
        let bytes = fs::read(wal_path(dir, *id))?;
        for record in scan_wal(&bytes).records {
            if record.version <= manifest.committed_version {
                continue;
            }
            for (table, value) in record.identity_high_water {
                if catalog
                    .table(&table)
                    .is_some_and(|schema| record.version > schema.epoch)
                {
                    let high = identities.entry(table).or_default();
                    *high = (*high).max(value);
                }
            }
            for change in record.changes {
                add(
                    &change.table,
                    &change.id,
                    record.version,
                    change.payload.as_deref(),
                )?;
            }
        }
    }
    versions.finish()?;
    let versions = PagedIndex::open(&versions_path)?;
    let live_path = scratch.0.join("live.pidx");
    let mut live = PagedWriter::create(&live_path, 0, None)?;
    let mut previous_row = Vec::new();
    let mut previous_version_key = Vec::new();
    let mut previous_payload = Vec::new();
    versions.scan(|key, payload| {
        if key == previous_version_key && payload != previous_payload {
            report
                .errors
                .push("canonical entries disagree at the same row and commit version".into());
        }
        previous_version_key.clear();
        previous_version_key.extend_from_slice(key);
        previous_payload.clear();
        previous_payload.extend_from_slice(payload);
        let row = &key[..key.len() - 8];
        if row != previous_row {
            if !payload.is_empty() {
                let version = !u64::from_be_bytes(key[key.len() - 8..].try_into().unwrap());
                let mut value = version.to_le_bytes().to_vec();
                value.extend_from_slice(payload);
                live.add(row, &value)?;
            }
            previous_row.clear();
            previous_row.extend_from_slice(row);
        }
        Ok(())
    })?;
    live.finish()?;
    let live = PagedIndex::open(&live_path)?;
    let unique_path = scratch.0.join("unique.pidx");
    let mut unique = ExternalPagedWriter::new(&unique_path, &scratch.0, 0, SORT_BUDGET)?;
    let blobs = dir.join(BLOBS_DIR);
    let mut counts = BTreeMap::<String, u64>::new();
    live.scan(|key, payload| {
        let (table, id): (String, String) =
            serde_json::from_slice(key).expect("internally encoded key");
        *counts.entry(table.clone()).or_default() += 1;
        let schema = catalog.table(&table).expect("owned canonical entry");
        let record = match decode_record(&payload[8..], Some(&blobs))
            .and_then(|record| normalize_record(schema, record))
        {
            Ok(record) => record,
            Err(error) => {
                report
                    .errors
                    .push(format!("{table}/{id}: invalid canonical row: {error}"));
                return Ok(());
            }
        };
        if schema.has_implicit_id() {
            unique.add(
                &unique_key(&table, "id", &Value::Text(id.clone())),
                id.as_bytes(),
            )?;
        }
        for index in schema.indexes.iter().filter(|index| index.unique) {
            let value = record.get(&index.column).unwrap_or(&Value::Null);
            if !value.is_null() {
                unique.add(&unique_key(&table, &index.column, value), id.as_bytes())?;
            }
        }
        for column in schema.columns.iter().filter(|column| column.identity) {
            match record.get(&column.name) {
                Some(Value::Int64(value))
                    if *value > 0 && identities.get(&table).is_some_and(|high| high >= value) => {}
                _ => report.errors.push(format!(
                    "{table}/{id}: invalid identity or high-water mark below a live value"
                )),
            }
        }
        Ok(())
    })?;
    unique.finish()?;
    let unique = PagedIndex::open(&unique_path)?;
    let mut previous_key = Vec::new();
    let mut previous_id = Vec::new();
    unique.scan(|key, id| {
        if key == previous_key && id != previous_id {
            report.errors.push(format!(
                "duplicate unique key across physical ids {} and {}",
                String::from_utf8_lossy(&previous_id),
                String::from_utf8_lossy(id)
            ));
        }
        previous_key.clear();
        previous_key.extend_from_slice(key);
        previous_id.clear();
        previous_id.extend_from_slice(id);
        Ok(())
    })?;
    live.scan(|key, payload| {
        let (table, id): (String, String) =
            serde_json::from_slice(key).expect("internally encoded key");
        let schema = catalog.table(&table).expect("owned canonical entry");
        let Ok(record) = decode_record(&payload[8..], Some(&blobs))
            .and_then(|record| normalize_record(schema, record))
        else {
            return Ok(());
        };
        for fk in &schema.foreign_keys {
            let value = record.get(&fk.column).unwrap_or(&Value::Null);
            if value.is_null() {
                continue;
            }
            let mut exists = false;
            unique.visit_key(
                &unique_key(&fk.referenced_table, &fk.referenced_column, value),
                |_| {
                    exists = true;
                    Ok(false)
                },
            )?;
            if !exists {
                report.errors.push(format!(
                    "{table}/{id}: orphan foreign key {} -> {}.{}",
                    fk.column, fk.referenced_table, fk.referenced_column
                ));
            }
        }
        Ok(())
    })?;
    if !report.errors.is_empty() {
        return Ok(());
    }

    // Compare the normal read view with the independent canonical image.
    // A discrepancy is repairable derived state, reported distinctly from
    // invalid canonical rows/constraints above.
    if let Ok(entries) = fs::read_dir(dir.join("indexes")) {
        for entry in entries {
            let path = entry?.path();
            if path
                .extension()
                .is_some_and(|extension| extension == "pidx")
            {
                if let Err(error) =
                    PagedIndex::open(&path).and_then(|index| index.scan(|_, _| Ok(())))
                {
                    report.warnings.push(format!(
                        "derived index {} is invalid: {error}; rebuild derived indexes",
                        path.file_name().unwrap_or_default().to_string_lossy()
                    ));
                }
            }
        }
    }
    let db = match crate::Db::open_read_only(dir) {
        Ok(db) => db,
        Err(error) => {
            report.warnings.push(format!(
                "derived read view unavailable for comparison: {error}"
            ));
            return Ok(());
        }
    };
    live.scan(|key, payload| {
        let (table, id): (String, String) =
            serde_json::from_slice(key).expect("internally encoded key");
        let schema = catalog.table(&table).expect("owned canonical entry");
        let expected = normalize_record(schema, decode_record(&payload[8..], Some(&blobs))?)?;
        for index in &schema.indexes {
            let value = expected.get(&index.column).unwrap_or(&Value::Null);
            if !value.is_null() && !db.secondary_contains(&table, &index.column, value, &id)? {
                report.warnings.push(format!(
                    "secondary index {}.{} omits canonical row {id}; rebuild derived indexes",
                    table, index.column
                ));
            }
        }
        let expected_version = u64::from_le_bytes(payload[..8].try_into().unwrap());
        let agrees = db.primary_version(&table, &id)? == Some(expected_version)
            && match db.get(&table, &id)? {
                Some(record) => {
                    encode_record_ordered(schema, &normalize_record(schema, record)?, None)?
                        == encode_record_ordered(schema, &expected, None)?
                }
                None => false,
            };
        if !agrees {
            report.warnings.push(format!(
                "primary index disagrees with canonical row {table}/{id}; rebuild derived indexes"
            ));
        }
        Ok(())
    })?;
    db.visit_secondary_entries(|table, column, encoded, id| {
        let mut agrees = false;
        live.visit_key(&row_key(table, id), |payload| {
            let schema = catalog.table(table).expect("validated catalog");
            let record = normalize_record(schema, decode_record(&payload[8..], Some(&blobs))?)?;
            let value = record.get(column).unwrap_or(&Value::Null);
            let mut actual = Vec::new();
            encode_value(&mut actual, value);
            agrees = !value.is_null() && actual == encoded;
            Ok(false)
        })?;
        if !agrees { report.warnings.push(format!("secondary index {table}.{column} contains a noncanonical pair for {id}; rebuild derived indexes")); }
        Ok(())
    })?;
    for table in &catalog.tables {
        let snapshot = db.snapshot();
        let mut count = 0u64;
        let mut cursor = None;
        loop {
            let rows = db.scan_batch_at(&snapshot, &table.name, cursor.as_deref(), 256)?;
            if rows.is_empty() {
                break;
            }
            count += rows.len() as u64;
            cursor = rows.last().map(|(id, _)| id.clone());
        }
        if count != *counts.get(&table.name).unwrap_or(&0) {
            report.warnings.push(format!("primary index row count disagrees with canonical table {}; rebuild derived indexes", table.name));
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn check_detects_checksum_valid_secondary_pairs_not_present_in_canonical_rows() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("db");
        let db = crate::Db::create(&path).unwrap();
        db.query("CREATE TABLE items(n int)").unwrap();
        db.query("CREATE INDEX ON items(n)").unwrap();
        db.query("INSERT INTO items(id,n) VALUES('r1',7)").unwrap();
        db.checkpoint().unwrap();
        drop(db);
        let mut changed = 0;
        for entry in fs::read_dir(path.join("indexes")).unwrap() {
            let file = entry.unwrap().path();
            if !file.file_name().is_some_and(|name| {
                let name = name.to_string_lossy();
                name.ends_with(".sidx") || name.ends_with(".sidx.run")
            }) || file
                .file_name()
                .unwrap()
                .to_string_lossy()
                .starts_with("primary")
            {
                continue;
            }
            let index = PagedIndex::open(&file).unwrap();
            let temporary = file.with_extension("test-tmp");
            let mut writer = PagedWriter::create(&temporary, index.dump_version(), None).unwrap();
            index
                .scan(|key, value| {
                    let mut key = key.to_vec();
                    if key.ends_with(b"r1") {
                        *key.last_mut().unwrap() = b'9';
                        changed += 1;
                    }
                    writer.add(&key, value)
                })
                .unwrap();
            writer.finish().unwrap();
            drop(index);
            fs::rename(temporary, file).unwrap();
        }
        assert!(changed > 0, "fixture must alter a published secondary run");
        let report = crate::check(&path).unwrap();
        assert!(
            report.errors.is_empty(),
            "canonical rows remain valid: {:?}",
            report.errors
        );
        assert!(
            report
                .warnings
                .iter()
                .any(|warning| warning.contains("secondary index")
                    && (warning.contains("omits") || warning.contains("noncanonical"))),
            "{:?}",
            report.warnings
        );
    }
}
