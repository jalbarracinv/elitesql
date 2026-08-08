use std::collections::HashSet;
use std::fs;
use std::path::Path;

use crate::db::{CATALOG_FILE, MARKER_FILE, SEGMENTS_DIR};
use crate::ddl::{DdlIntent, DDL_FILE};
use crate::error::Result;
use crate::manifest::{Manifest, MANIFEST_FILE, MANIFEST_PREV_FILE};
use crate::schema::Catalog;
use crate::segment::{scan_segment, segment_file_name};
use crate::wal::{scan_wal, wal_path, WAL_DIR};

/// Result of an offline integrity check. `errors` are integrity violations;
/// `warnings` are recoverable oddities (torn WAL tail, orphan files) that
/// open() handles automatically.
#[derive(Debug, Default)]
pub struct CheckReport {
    pub errors: Vec<String>,
    pub warnings: Vec<String>,
    pub used_manifest_prev: bool,
}

impl CheckReport {
    pub fn is_ok(&self) -> bool {
        self.errors.is_empty()
    }
}

/// Validate the on-disk structures of a database directory without opening
/// it: marker, manifest chain, catalog, segment checksums, WAL checksums.
/// Read-only; run it on a database that is not currently open elsewhere.
pub fn check(path: impl AsRef<Path>) -> Result<CheckReport> {
    let dir = path.as_ref();
    let mut report = CheckReport::default();

    if !dir.join(MARKER_FILE).exists() {
        report.errors.push(format!(
            "missing {MARKER_FILE} marker: not a elitesql database"
        ));
        return Ok(report);
    }

    if let Err(e) = Catalog::load(&dir.join(CATALOG_FILE)) {
        report.errors.push(format!("catalog: {e}"));
    }

    match DdlIntent::load(dir) {
        Ok(Some(intent)) => report.warnings.push(format!(
            "schema change interrupted, completed on next read-write open: {}",
            intent.describe()
        )),
        Ok(None) => {}
        Err(e) => report.errors.push(format!("{DDL_FILE}: {e}")),
    }

    let manifest = match fs::read(dir.join(MANIFEST_FILE))
        .map_err(|e| e.to_string())
        .and_then(|b| Manifest::decode(&b).map_err(|e| e.to_string()))
    {
        Ok(m) => Some(m),
        Err(primary_err) => {
            report.warnings.push(format!(
                "primary manifest unusable ({primary_err}), trying manifest.prev"
            ));
            match fs::read(dir.join(MANIFEST_PREV_FILE))
                .map_err(|e| e.to_string())
                .and_then(|b| Manifest::decode(&b).map_err(|e| e.to_string()))
            {
                Ok(m) => {
                    report.used_manifest_prev = true;
                    Some(m)
                }
                Err(prev_err) => {
                    report
                        .errors
                        .push(format!("no valid manifest (prev: {prev_err})"));
                    None
                }
            }
        }
    };

    let Some(manifest) = manifest else {
        return Ok(report);
    };

    // Segments: every listed segment must exist and validate fully.
    let mut listed = HashSet::new();
    for meta in &manifest.segments {
        listed.insert(meta.id);
        let name = segment_file_name(meta.id);
        let seg_path = dir.join(SEGMENTS_DIR).join(&name);
        let data = match fs::read(&seg_path) {
            Ok(d) => d,
            Err(e) => {
                report
                    .errors
                    .push(format!("segment {name}: unreadable: {e}"));
                continue;
            }
        };
        if (data.len() as u64) < meta.len {
            report.errors.push(format!(
                "segment {name}: shorter than manifest ({} < {})",
                data.len(),
                meta.len
            ));
            continue;
        }
        if (data.len() as u64) > meta.len {
            report.warnings.push(format!(
                "segment {name}: {} trailing bytes past manifest length",
                data.len() as u64 - meta.len
            ));
        }
        let mut entries = Vec::new();
        let outcome = scan_segment(&data[..meta.len as usize], &mut entries);
        if !outcome.clean || outcome.valid_len != meta.len {
            report.errors.push(format!(
                "segment {name}: invalid entry at offset {}",
                outcome.valid_len
            ));
        }
        let blobs_dir = dir.join(crate::db::BLOBS_DIR);
        let mut refs = Vec::new();
        for e in &entries {
            if e.version > manifest.committed_version {
                report.errors.push(format!(
                    "segment {name}: entry version {} above manifest watermark {}",
                    e.version, manifest.committed_version
                ));
                break;
            }
            if e.tombstone {
                continue;
            }
            // Out-of-line blob chunks referenced by this payload must exist
            // and validate (magic + length + crc).
            let payload = &data
                [e.payload_offset as usize..(e.payload_offset + e.payload_len as u64) as usize];
            refs.clear();
            if crate::db::scan_payload_blob_refs(payload, &mut refs).is_err() {
                report.errors.push(format!(
                    "segment {name}: unparseable payload for {}/{}",
                    e.table, e.id
                ));
                continue;
            }
            for r in &refs {
                if let Err(err) = crate::value::read_blob_file(&blobs_dir, r) {
                    report
                        .errors
                        .push(format!("segment {name}: {}/{}: {err}", e.table, e.id));
                }
            }
        }
    }
    if let Ok(dirents) = fs::read_dir(dir.join(SEGMENTS_DIR)) {
        for dirent in dirents.flatten() {
            let name = dirent.file_name();
            let name = name.to_string_lossy().into_owned();
            if let Some(stem) = name.strip_suffix(".seg") {
                if let Ok(id) = stem.parse::<u32>() {
                    if !listed.contains(&id) {
                        report
                            .warnings
                            .push(format!("orphan segment {name} (removed on next open)"));
                    }
                }
            }
        }
    }

    // WAL: records must checksum; a torn tail is normal after a crash.
    let wal_file = wal_path(dir, manifest.wal_id);
    match fs::read(&wal_file) {
        Ok(data) => {
            let scan = scan_wal(&data);
            if !scan.clean {
                report.warnings.push(format!(
                    "wal {}: torn tail at offset {} (truncated on next open)",
                    manifest.wal_id, scan.valid_len
                ));
            }
            let mut last = manifest.committed_version;
            for rec in &scan.records {
                if rec.version <= manifest.committed_version {
                    continue;
                }
                if rec.version <= last && last != manifest.committed_version {
                    report.errors.push(format!(
                        "wal {}: non-monotonic commit versions",
                        manifest.wal_id
                    ));
                    break;
                }
                last = rec.version;
                for ch in &rec.changes {
                    if let Some(p) = &ch.payload {
                        if let Err(e) =
                            crate::db::decode_record(p, Some(&dir.join(crate::db::BLOBS_DIR)))
                        {
                            report.errors.push(format!(
                                "wal {}: bad payload for {}/{}: {e}",
                                manifest.wal_id, ch.table, ch.id
                            ));
                        }
                    }
                }
            }
        }
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            report.warnings.push(format!(
                "wal {} missing (recreated on next open)",
                manifest.wal_id
            ));
        }
        Err(e) => report.errors.push(format!("wal {}: {e}", manifest.wal_id)),
    }
    if let Ok(dirents) = fs::read_dir(dir.join(WAL_DIR)) {
        for dirent in dirents.flatten() {
            let name = dirent.file_name();
            let name = name.to_string_lossy().into_owned();
            if let Some(stem) = name.strip_suffix(".wal") {
                if let Ok(id) = stem.parse::<u32>() {
                    if id != manifest.wal_id {
                        report
                            .warnings
                            .push(format!("obsolete wal {name} (removed on next open)"));
                    }
                }
            }
        }
    }

    Ok(report)
}
