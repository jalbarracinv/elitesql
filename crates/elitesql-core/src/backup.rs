//! Backup and restore. `Db::backup` produces a snapshot-consistent logical
//! copy of the database into a brand-new directory — safe to call while
//! other threads keep committing. `restore` validates a backup with `check`
//! and materializes it as a fresh database, rebuilding derived indexes.
//! Both write into a unique `<dst>.<ulid>.partial` sibling and rename it into
//! place at the end, so an interrupted run never leaves a half-written
//! directory under the final name or deletes an unrelated temporary path.

use std::fs::{self, File, OpenOptions, TryLockError};
use std::path::{Path, PathBuf};
use ulid::Ulid;

use crate::check::check;
use crate::db::{
    acquire_lock, Db, DbOptions, BLOBS_DIR, CATALOG_FILE, LOCK_FILE, MARKER_FILE, SEGMENTS_DIR,
};
use crate::ddl::DDL_FILE;
use crate::error::{Error, Result};
use crate::manifest::fsync_dir;
use crate::wal::{Durability, WAL_DIR};

#[derive(Debug, Default)]
pub struct BackupReport {
    pub tables: usize,
    pub records: u64,
}

#[derive(Debug, Default)]
pub struct RestoreReport {
    pub tables: usize,
    pub records: u64,
    /// Warnings surfaced by the pre-restore `check` of the backup.
    pub warnings: Vec<String>,
}

/// Records per destination transaction: large enough to amortize commit
/// overhead, small enough to keep staging memory bounded.
const BACKUP_BATCH: usize = 1024;

impl Db {
    /// Back up the database into a brand-new directory at `dst` (must not
    /// exist). The copy is logical and snapshot-consistent: it contains
    /// exactly the state committed when the call began, while concurrent
    /// writers keep committing unblocked. Schemas and index definitions are
    /// preserved; segment history is not (the backup is born compacted).
    /// Tables created after the call began may appear empty in the copy.
    pub fn backup(&self, dst: impl AsRef<Path>) -> Result<BackupReport> {
        let dst = dst.as_ref();
        let _destination = lock_destination(dst)?;
        if dst.exists() {
            return Err(Error::InvalidArgument(format!(
                "backup destination already exists: {}",
                dst.display()
            )));
        }
        let partial = partial_path(dst)?;

        // Snapshot versions alone do not freeze the catalog: DDL is
        // deliberately visible to all readers. Keep the DDL gate for the
        // logical copy so schemas and rows describe one coherent point while
        // ordinary DML continues through MVCC.
        let _ddl = self.acquire_ddl_guard();
        let snap = self.snapshot();
        let result = (|| {
            // Fast durability defers fsync to the final checkpoint, which
            // publishes the manifest and syncs everything before the rename.
            let out = Db::create_with(
                &partial,
                DbOptions {
                    durability: Durability::Fast,
                    ..DbOptions::default()
                },
            )?;
            let mut report = BackupReport::default();
            for table in self.tables() {
                let Some(schema) = self.table_schema(&table) else {
                    continue;
                };
                out.create_table(schema)?;
                report.tables += 1;
                let mut cursor: Option<String> = None;
                loop {
                    let rows =
                        self.scan_batch_at(&snap, &table, cursor.as_deref(), BACKUP_BATCH)?;
                    if rows.is_empty() {
                        break;
                    }
                    let mut txn = out.begin();
                    for (_, record) in &rows {
                        // The scanned record carries its `id`, so the copy
                        // keeps the original ids.
                        txn.insert(&table, record.clone())?;
                    }
                    txn.commit()?;
                    report.records += rows.len() as u64;
                    cursor = rows.last().map(|(id, _)| id.clone());
                }
            }
            out.wait_vector_indexing()?;
            out.checkpoint()?;
            Ok(report)
        })();

        match result {
            Ok(report) => {
                fs::rename(&partial, dst)?;
                if let Err(error) = fsync_dir(parent_of(dst)) {
                    return Err(Error::CommitUnknown(format!(
                        "backup was renamed to {}, but syncing its parent failed: {error}",
                        dst.display()
                    )));
                }
                Ok(report)
            }
            Err(e) => {
                let _ = fs::remove_dir_all(&partial);
                Err(e)
            }
        }
    }
}

/// Validate the backup at `src` (offline `check`, refused on any error) and
/// materialize it as a fresh database at `dst` (must not exist). Canonical
/// files are copied (marker, catalog, manifest, WAL, segments, blobs);
/// derived indexes are rebuilt by the verification open at the end, which
/// also replays the WAL and counts what the restored database holds.
pub fn restore(src: impl AsRef<Path>, dst: impl AsRef<Path>) -> Result<RestoreReport> {
    let src = src.as_ref();
    let dst = dst.as_ref();
    let _destination = lock_destination(dst)?;
    if dst.exists() {
        return Err(Error::InvalidArgument(format!(
            "restore destination already exists: {}",
            dst.display()
        )));
    }
    // `check` followed by file-by-file copy is only meaningful over an
    // immutable source generation. A writer takes the exclusive form of this
    // same process lock, so hold a shared lock through verification and copy.
    let _source_lock = acquire_lock(src, true)?;
    let check_report = check(src)?;
    if !check_report.is_ok() {
        return Err(Error::Corrupt(format!(
            "backup at {} failed check: {}",
            src.display(),
            check_report.errors.join("; ")
        )));
    }

    let partial = partial_path(dst)?;
    let copied = (|| {
        fs::create_dir_all(&partial)?;
        copy_file(src, &partial, MARKER_FILE)?;
        copy_file(src, &partial, CATALOG_FILE)?;
        // A backup produced by `Db::backup` never has one, but a directory
        // copied by other means can: carry it so the schema change is still
        // completed when the restored database is opened.
        if src.join(DDL_FILE).exists() {
            copy_file(src, &partial, DDL_FILE)?;
        }
        copy_file(src, &partial, "manifest")?;
        if src.join("manifest.prev").exists() {
            copy_file(src, &partial, "manifest.prev")?;
        }
        copy_dir(src, &partial, WAL_DIR)?;
        copy_dir(src, &partial, SEGMENTS_DIR)?;
        copy_dir(src, &partial, BLOBS_DIR)?;
        fsync_dir(&partial)?;
        Ok(())
    })();
    if let Err(e) = copied {
        let _ = fs::remove_dir_all(&partial);
        return Err(e);
    }
    // Verify and rebuild disposable indexes while the directory still has its
    // private temporary name. No validation error can leave a final destination.
    let verified = (|| {
        let db = Db::open(&partial)?;
        let mut report = RestoreReport {
            warnings: check_report.warnings,
            ..RestoreReport::default()
        };
        for table in db.tables() {
            report.tables += 1;
            report.records += db.scan(&table)?.len() as u64;
        }
        drop(db);
        Ok(report)
    })();
    let report = match verified {
        Ok(report) => report,
        Err(error) => {
            let _ = fs::remove_dir_all(&partial);
            return Err(error);
        }
    };
    fs::rename(&partial, dst)?;
    if let Err(error) = fsync_dir(parent_of(dst)) {
        return Err(Error::CommitUnknown(format!(
            "restore was renamed to {}, but syncing its parent failed: {error}",
            dst.display()
        )));
    }
    Ok(report)
}

/// A private sibling next to the destination, so the final rename stays within
/// one filesystem without deleting a pre-existing, unrelated `.partial` path.
pub(crate) fn partial_path(dst: &Path) -> Result<PathBuf> {
    let name = dst.file_name().ok_or_else(|| {
        Error::InvalidArgument(format!("invalid destination path: {}", dst.display()))
    })?;
    Ok(dst.with_file_name(format!(
        "{}.{}.partial",
        name.to_string_lossy(),
        Ulid::new()
    )))
}

pub(crate) fn parent_of(path: &Path) -> &Path {
    match path.parent() {
        Some(p) if p.as_os_str().is_empty() => Path::new("."),
        Some(p) => p,
        None => Path::new("."),
    }
}

/// Serialize all producers targeting the same final directory. The sibling
/// lock file intentionally persists after close; advisory state lives in the
/// inode lock, and retaining the inode avoids an unlink/open race between
/// consecutive producers.
pub(crate) fn lock_destination(dst: &Path) -> Result<File> {
    let name = dst.file_name().ok_or_else(|| {
        Error::InvalidArgument(format!("invalid destination path: {}", dst.display()))
    })?;
    let lock = dst.with_file_name(format!(
        "{}.elitesql-destination-lock",
        name.to_string_lossy()
    ));
    let file = OpenOptions::new()
        .create(true)
        .truncate(false)
        .write(true)
        .open(&lock)?;
    match file.try_lock() {
        Ok(()) => Ok(file),
        Err(TryLockError::WouldBlock) => Err(Error::DatabaseLocked(dst.display().to_string())),
        Err(TryLockError::Error(error)) => Err(Error::Io(error)),
    }
}

fn copy_file(src: &Path, dst: &Path, name: &str) -> Result<()> {
    let target = dst.join(name);
    fs::copy(src.join(name), &target)?;
    File::open(target)?.sync_all()?;
    Ok(())
}

/// Copy the regular files of a flat directory, skipping the lock file and
/// temporaries. A missing source directory copies as empty.
fn copy_dir(src: &Path, dst: &Path, name: &str) -> Result<()> {
    let from = src.join(name);
    let to = dst.join(name);
    fs::create_dir_all(&to)?;
    if !from.is_dir() {
        return Ok(());
    }
    for entry in fs::read_dir(&from)? {
        let entry = entry?;
        if !entry.file_type()?.is_file() {
            continue;
        }
        let file_name = entry.file_name();
        let as_str = file_name.to_string_lossy();
        if as_str == LOCK_FILE || as_str.ends_with(".tmp") {
            continue;
        }
        let target = to.join(&file_name);
        fs::copy(entry.path(), &target)?;
        File::open(target)?.sync_all()?;
    }
    fsync_dir(&to)?;
    Ok(())
}
