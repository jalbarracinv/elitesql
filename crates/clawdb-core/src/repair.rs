//! Salvage: extract every recoverable record from a (possibly corrupt)
//! database into a fresh one. Never silent — everything skipped is counted
//! and reported. The golden rule applies: data files are canonical, so
//! salvage reads segments and WAL directly and rebuilds the rest.

use std::collections::BTreeMap;
use std::fs;
use std::path::Path;

use crate::db::{decode_record, Db, CATALOG_FILE, SEGMENTS_DIR};
use crate::error::{Error, Result};
use crate::schema::Catalog;
use crate::segment::scan_segment;
use crate::value::Value;
use crate::wal::{scan_wal, WAL_DIR};

#[derive(Debug, Default)]
pub struct SalvageReport {
    pub tables: Vec<String>,
    pub recovered_records: u64,
    /// Records whose latest version was a tombstone (correctly absent).
    pub deleted_records: u64,
    /// Records or entries that could not be recovered or re-inserted.
    pub skipped: u64,
    pub segments_scanned: usize,
    pub wal_files_scanned: usize,
    pub notes: Vec<String>,
}

/// Salvage `src` into a brand-new database at `dst` (must not exist).
/// Requires a readable catalog; everything else is best-effort per entry.
pub fn salvage(src: impl AsRef<Path>, dst: impl AsRef<Path>) -> Result<SalvageReport> {
    let src = src.as_ref();
    let dst = dst.as_ref();
    let mut report = SalvageReport::default();

    let catalog = Catalog::load(&src.join(CATALOG_FILE)).map_err(|e| {
        Error::Corrupt(format!(
            "salvage requires a readable catalog.json ({e}); without the schema records cannot be re-typed"
        ))
    })?;

    // Latest version per (table, id): version -> payload (None = tombstone).
    type Versions = BTreeMap<u64, Option<Vec<u8>>>;
    let mut latest: BTreeMap<(String, String), Versions> = BTreeMap::new();
    let mut push = |table: String, id: String, version: u64, payload: Option<Vec<u8>>| {
        latest.entry((table, id)).or_default().insert(version, payload);
    };

    // Segments: valid prefix of every file, in id order.
    let mut seg_files: Vec<_> = list_numbered(&src.join(SEGMENTS_DIR), ".seg");
    seg_files.sort();
    for path in &seg_files {
        let Ok(data) = fs::read(path) else {
            report.notes.push(format!("unreadable segment {}", path.display()));
            continue;
        };
        let mut entries = Vec::new();
        let outcome = scan_segment(&data, &mut entries);
        if !outcome.clean {
            report.notes.push(format!(
                "{}: {} bytes discarded after a corrupt entry at offset {}",
                path.file_name().unwrap_or_default().to_string_lossy(),
                data.len() as u64 - outcome.valid_len,
                outcome.valid_len
            ));
        }
        for e in entries {
            let payload = if e.tombstone {
                None
            } else {
                Some(data[e.payload_offset as usize..(e.payload_offset + e.payload_len as u64) as usize].to_vec())
            };
            push(e.table, e.id, e.version, payload);
        }
        report.segments_scanned += 1;
    }

    // WAL files: every valid commit record.
    let mut wal_files: Vec<_> = list_numbered(&src.join(WAL_DIR), ".wal");
    wal_files.sort();
    for path in &wal_files {
        let Ok(data) = fs::read(path) else {
            report.notes.push(format!("unreadable wal {}", path.display()));
            continue;
        };
        let scan = scan_wal(&data);
        if !scan.clean {
            report.notes.push(format!(
                "{}: torn tail at offset {}",
                path.file_name().unwrap_or_default().to_string_lossy(),
                scan.valid_len
            ));
        }
        for rec in scan.records {
            for ch in rec.changes {
                push(ch.table, ch.id, rec.version, ch.payload);
            }
        }
        report.wal_files_scanned += 1;
    }

    // Rebuild into a fresh database with the same schema (indexes included).
    if dst.exists() {
        return Err(Error::InvalidArgument(format!(
            "salvage destination already exists: {}",
            dst.display()
        )));
    }
    let out = Db::create(dst)?;
    for table in &catalog.tables {
        out.create_table(table.clone())?;
        report.tables.push(table.name.clone());
    }

    let mut txn = out.begin();
    let mut in_batch = 0usize;
    for ((table, id), versions) in latest {
        let Some((_, payload)) = versions.iter().next_back() else { continue };
        let Some(payload) = payload else {
            report.deleted_records += 1;
            continue;
        };
        let mut record = match decode_record(payload) {
            Ok(r) => r,
            Err(e) => {
                report.skipped += 1;
                report.notes.push(format!("{table}/{id}: undecodable payload ({e})"));
                continue;
            }
        };
        record.insert("id".into(), Value::Text(id.clone()));
        match txn.insert(&table, record) {
            Ok(_) => {
                report.recovered_records += 1;
                in_batch += 1;
            }
            Err(e) => {
                report.skipped += 1;
                report.notes.push(format!("{table}/{id}: not re-inserted ({e})"));
            }
        }
        if in_batch >= 1000 {
            txn.commit()?;
            txn = out.begin();
            in_batch = 0;
        }
    }
    txn.commit()?;
    out.checkpoint()?;
    Ok(report)
}

fn list_numbered(dir: &Path, suffix: &str) -> Vec<std::path::PathBuf> {
    let Ok(entries) = fs::read_dir(dir) else { return Vec::new() };
    entries
        .flatten()
        .map(|e| e.path())
        .filter(|p| p.to_string_lossy().ends_with(suffix))
        .collect()
}
