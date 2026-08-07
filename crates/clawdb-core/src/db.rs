use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet};
use std::fs::{self, File, OpenOptions, TryLockError};
use std::io::Write;
use std::os::unix::fs::FileExt;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, RwLock};

use ulid::Ulid;

use crate::error::{Error, Result};
use crate::manifest::{fsync_dir, Manifest, SegmentMeta};
use crate::schema::{Catalog, IndexDef, TableSchema, FORMAT_VERSION, ID_COLUMN};
use crate::segment::{encode_entry, scan_segment, segment_file_name};
use crate::value::{decode_value, encode_value, read_u16, Value};
use crate::wal::{encode_commit, scan_wal, wal_path, Durability, WalWriter, WAL_DIR};

pub(crate) const MARKER_FILE: &str = "CLAWDB";
pub(crate) const CATALOG_FILE: &str = "catalog.json";
pub(crate) const SEGMENTS_DIR: &str = "segments";
const LOCK_FILE: &str = "LOCK";

/// A record is a map from column name to value. On reads the implicit
/// primary key is included under the key `"id"`.
pub type Record = BTreeMap<String, Value>;

/// Engine tuning options.
#[derive(Debug, Clone)]
pub struct DbOptions {
    pub durability: Durability,
    /// Checkpoint (drain WAL into a segment) once this much committed data
    /// accumulates in memory.
    pub memtable_max_bytes: u64,
    /// Max time between WAL fsyncs in `Balanced` mode.
    pub balanced_sync_interval_ms: u64,
}

impl Default for DbOptions {
    fn default() -> Self {
        DbOptions {
            durability: Durability::Safe,
            memtable_max_bytes: 8 * 1024 * 1024,
            balanced_sync_interval_ms: 25,
        }
    }
}

/// Where one committed record version lives.
#[derive(Debug, Clone)]
enum VKind {
    /// Committed via WAL, not yet checkpointed into a segment.
    MemPut(Arc<Vec<u8>>),
    SegPut {
        segment: u32,
        payload_offset: u64,
        payload_len: u32,
    },
    MemTombstone,
    SegTombstone,
}

#[derive(Debug, Clone)]
struct VersionEntry {
    version: u64,
    kind: VKind,
}

impl VersionEntry {
    fn is_tombstone(&self) -> bool {
        matches!(self.kind, VKind::MemTombstone | VKind::SegTombstone)
    }
}

struct SecIdx {
    map: HashMap<Vec<u8>, BTreeSet<String>>,
}

struct State {
    catalog: Catalog,
    committed_version: u64,
    /// table -> id -> versions in ascending commit order.
    index: HashMap<String, BTreeMap<String, Vec<VersionEntry>>>,
    /// (table, column) -> equality index over the latest committed state.
    secondary: HashMap<(String, String), SecIdx>,
    readers: HashMap<u32, File>,
    segments: Vec<SegmentMeta>,
    next_segment_id: u32,
}

struct CommitState {
    wal: WalWriter,
    memtable_bytes: u64,
}

struct Shared {
    dir: PathBuf,
    opts: DbOptions,
    /// Held for the lifetime of the Db: process-level exclusion.
    _lock_file: File,
    state: RwLock<State>,
    /// Serializes commits, checkpoints and compaction. Writers stage in
    /// parallel without this lock and only meet here, at commit.
    commit: Mutex<CommitState>,
    /// version -> live snapshot refcount; compaction preserves these.
    snapshots: Mutex<BTreeMap<u64, usize>>,
}

/// A stable read position. Reads through a snapshot see the database exactly
/// as it was at this commit version. While the snapshot is alive, compaction
/// keeps the versions it needs.
pub struct Snapshot {
    version: u64,
    shared: Arc<Shared>,
}

impl Snapshot {
    pub fn version(&self) -> u64 {
        self.version
    }
}

impl Clone for Snapshot {
    fn clone(&self) -> Self {
        register_snapshot(&self.shared, self.version);
        Snapshot {
            version: self.version,
            shared: self.shared.clone(),
        }
    }
}

impl Drop for Snapshot {
    fn drop(&mut self) {
        let mut snaps = self.shared.snapshots.lock().unwrap();
        if let Some(count) = snaps.get_mut(&self.version) {
            *count -= 1;
            if *count == 0 {
                snaps.remove(&self.version);
            }
        }
    }
}

impl std::fmt::Debug for Snapshot {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Snapshot").field("version", &self.version).finish()
    }
}

fn register_snapshot(shared: &Arc<Shared>, version: u64) {
    *shared.snapshots.lock().unwrap().entry(version).or_insert(0) += 1;
}

/// A read-write transaction. Reads see a stable snapshot plus this
/// transaction's own staged writes. Writes are buffered locally and only
/// meet other writers at `commit`, where optimistic validation either
/// publishes them atomically or fails with `Error::Conflict`.
pub struct Txn {
    shared: Arc<Shared>,
    snapshot: Snapshot,
    /// (table, id) -> staged full record, or None for delete.
    staged: BTreeMap<(String, String), Option<Record>>,
}

/// An embedded ClawDB database backed by a self-contained directory.
///
/// Phase 1 storage: commits are appended to a durable WAL (fsync per the
/// durability mode) and applied to an in-memory MVCC index; checkpoints
/// drain committed data into immutable segments and publish an atomic
/// manifest (with `manifest.prev` as the recovery fallback). On open, the
/// manifest chain is loaded, segments are scanned, and the WAL is replayed
/// idempotently; a torn WAL tail is truncated.
pub struct Db {
    shared: Arc<Shared>,
}

impl Db {
    pub fn create(path: impl AsRef<Path>) -> Result<Db> {
        Db::create_with(path, DbOptions::default())
    }

    pub fn open(path: impl AsRef<Path>) -> Result<Db> {
        Db::open_with(path, DbOptions::default())
    }

    pub fn open_or_create(path: impl AsRef<Path>) -> Result<Db> {
        Db::open_or_create_with(path, DbOptions::default())
    }

    pub fn open_or_create_with(path: impl AsRef<Path>, opts: DbOptions) -> Result<Db> {
        if path.as_ref().join(MARKER_FILE).exists() {
            Db::open_with(path, opts)
        } else {
            Db::create_with(path, opts)
        }
    }

    pub fn create_with(path: impl AsRef<Path>, opts: DbOptions) -> Result<Db> {
        let dir = path.as_ref().to_path_buf();
        if dir.join(MARKER_FILE).exists() {
            return Err(Error::InvalidArgument(format!(
                "database already exists at {}",
                dir.display()
            )));
        }
        fs::create_dir_all(dir.join(SEGMENTS_DIR))?;
        fs::create_dir_all(dir.join(WAL_DIR))?;
        let lock_file = acquire_lock(&dir)?;

        let mut marker = File::create(dir.join(MARKER_FILE))?;
        marker.write_all(format!("clawdb format_version={FORMAT_VERSION}\n").as_bytes())?;
        marker.sync_all()?;
        Catalog::new().save(&dir.join(CATALOG_FILE))?;
        let manifest = Manifest::initial();
        manifest.publish(&dir)?;
        File::create(wal_path(&dir, manifest.wal_id))?.sync_all()?;
        fsync_dir(&dir.join(WAL_DIR))?;

        let wal = WalWriter::open(&dir, manifest.wal_id)?;
        Ok(Db {
            shared: Arc::new(Shared {
                dir,
                opts,
                _lock_file: lock_file,
                state: RwLock::new(State {
                    catalog: Catalog::new(),
                    committed_version: 0,
                    index: HashMap::new(),
                    secondary: HashMap::new(),
                    readers: HashMap::new(),
                    segments: Vec::new(),
                    next_segment_id: 1,
                }),
                commit: Mutex::new(CommitState {
                    wal,
                    memtable_bytes: 0,
                }),
                snapshots: Mutex::new(BTreeMap::new()),
            }),
        })
    }

    pub fn open_with(path: impl AsRef<Path>, opts: DbOptions) -> Result<Db> {
        let dir = path.as_ref().to_path_buf();
        if !dir.join(MARKER_FILE).exists() {
            return Err(Error::InvalidArgument(format!(
                "not a clawdb database: {}",
                dir.display()
            )));
        }
        let lock_file = acquire_lock(&dir)?;
        let catalog = Catalog::load(&dir.join(CATALOG_FILE))?;
        let (manifest, used_prev) = Manifest::load(&dir)?;
        if used_prev {
            // The primary manifest was unreadable; re-establish it from the
            // fallback without ever rotating the corrupt file over the good one.
            manifest.heal(&dir)?;
        }
        cleanup_orphans(&dir, &manifest)?;

        // Load segments listed in the manifest.
        let mut index: HashMap<String, BTreeMap<String, Vec<VersionEntry>>> = HashMap::new();
        let mut readers = HashMap::new();
        for meta in &manifest.segments {
            let seg_path = dir.join(SEGMENTS_DIR).join(segment_file_name(meta.id));
            let data = fs::read(&seg_path)?;
            if (data.len() as u64) < meta.len {
                return Err(Error::Corrupt(format!(
                    "segment {} shorter than manifest ({} < {})",
                    segment_file_name(meta.id),
                    data.len(),
                    meta.len
                )));
            }
            let valid = &data[..meta.len as usize];
            let mut entries = Vec::new();
            let outcome = scan_segment(valid, &mut entries);
            if !outcome.clean || outcome.valid_len != meta.len {
                return Err(Error::Corrupt(format!(
                    "segment {} failed validation",
                    segment_file_name(meta.id)
                )));
            }
            for e in entries {
                let kind = if e.tombstone {
                    VKind::SegTombstone
                } else {
                    VKind::SegPut {
                        segment: meta.id,
                        payload_offset: e.payload_offset,
                        payload_len: e.payload_len,
                    }
                };
                index
                    .entry(e.table)
                    .or_default()
                    .entry(e.id)
                    .or_default()
                    .push(VersionEntry {
                        version: e.version,
                        kind,
                    });
            }
            readers.insert(meta.id, File::open(&seg_path)?);
        }

        // Replay the WAL idempotently: only commits above the manifest
        // watermark apply; a torn tail is truncated.
        let mut committed_version = manifest.committed_version;
        let mut memtable_bytes = 0u64;
        let wal_file = wal_path(&dir, manifest.wal_id);
        if !wal_file.exists() {
            File::create(&wal_file)?.sync_all()?;
            fsync_dir(&dir.join(WAL_DIR))?;
        }
        let data = fs::read(&wal_file)?;
        let scan = scan_wal(&data);
        if !scan.clean {
            let f = OpenOptions::new().write(true).open(&wal_file)?;
            f.set_len(scan.valid_len)?;
            f.sync_all()?;
        }
        for rec in scan.records {
            if rec.version <= committed_version {
                continue;
            }
            for ch in rec.changes {
                let kind = match ch.payload {
                    Some(p) => {
                        memtable_bytes += p.len() as u64;
                        VKind::MemPut(Arc::new(p))
                    }
                    None => VKind::MemTombstone,
                };
                index
                    .entry(ch.table)
                    .or_default()
                    .entry(ch.id)
                    .or_default()
                    .push(VersionEntry {
                        version: rec.version,
                        kind,
                    });
            }
            committed_version = rec.version;
        }

        let secondary = build_secondary(&catalog, &index, &readers)?;
        let next_segment_id = manifest.segments.iter().map(|s| s.id).max().unwrap_or(0) + 1;
        let wal = WalWriter::open(&dir, manifest.wal_id)?;
        Ok(Db {
            shared: Arc::new(Shared {
                dir,
                opts,
                _lock_file: lock_file,
                state: RwLock::new(State {
                    catalog,
                    committed_version,
                    index,
                    secondary,
                    readers,
                    segments: manifest.segments,
                    next_segment_id,
                }),
                commit: Mutex::new(CommitState {
                    wal,
                    memtable_bytes,
                }),
                snapshots: Mutex::new(BTreeMap::new()),
            }),
        })
    }

    // --- schema ------------------------------------------------------------

    pub fn create_table(&self, schema: TableSchema) -> Result<()> {
        schema.validate()?;
        let _cs = self.shared.commit.lock().unwrap();
        let mut st = self.shared.state.write().unwrap();
        if st.catalog.table(&schema.name).is_some() {
            return Err(Error::TableExists(schema.name));
        }
        for def in &schema.indexes {
            if schema.column(&def.column).is_none() {
                return Err(Error::SchemaViolation(format!(
                    "index over unknown column '{}'",
                    def.column
                )));
            }
            st.secondary.insert(
                (schema.name.clone(), def.column.clone()),
                SecIdx {
                    map: HashMap::new(),
                },
            );
        }
        st.catalog.tables.push(schema);
        st.catalog.save(&self.shared.dir.join(CATALOG_FILE))
    }

    /// Create a secondary (optionally unique) equality index over a column,
    /// built from the current committed state.
    pub fn create_index(&self, table: &str, column: &str, unique: bool) -> Result<()> {
        let _cs = self.shared.commit.lock().unwrap();
        let mut st = self.shared.state.write().unwrap();
        {
            let schema = st
                .catalog
                .table(table)
                .ok_or_else(|| Error::TableNotFound(table.into()))?;
            if schema.column(column).is_none() {
                return Err(Error::SchemaViolation(format!("unknown column '{column}'")));
            }
            if schema.indexes.iter().any(|d| d.column == column) {
                return Err(Error::InvalidArgument(format!(
                    "index on {table}.{column} already exists"
                )));
            }
        }
        let mut map: HashMap<Vec<u8>, BTreeSet<String>> = HashMap::new();
        if let Some(ids) = st.index.get(table) {
            for (id, versions) in ids {
                if let Some(last) = versions.last() {
                    if !last.is_tombstone() {
                        let rec = read_record_kind(&st.readers, &last.kind)?;
                        if let Some(v) = rec.get(column) {
                            if !v.is_null() {
                                let set = map.entry(index_key(v)).or_default();
                                if unique && !set.is_empty() {
                                    return Err(Error::UniqueViolation {
                                        table: table.into(),
                                        column: column.into(),
                                    });
                                }
                                set.insert(id.clone());
                            }
                        }
                    }
                }
            }
        }
        let schema = st
            .catalog
            .tables
            .iter_mut()
            .find(|t| t.name == table)
            .expect("checked above");
        schema.indexes.push(IndexDef {
            column: column.into(),
            unique,
        });
        st.catalog.save(&self.shared.dir.join(CATALOG_FILE))?;
        st.secondary
            .insert((table.into(), column.into()), SecIdx { map });
        Ok(())
    }

    pub fn tables(&self) -> Vec<String> {
        let st = self.shared.state.read().unwrap();
        st.catalog.tables.iter().map(|t| t.name.clone()).collect()
    }

    // --- transactions --------------------------------------------------------

    /// Begin a transaction reading from a stable snapshot of the current
    /// committed state. Multiple transactions stage writes in parallel; they
    /// only meet at commit.
    pub fn begin(&self) -> Txn {
        Txn {
            shared: self.shared.clone(),
            snapshot: self.snapshot(),
            staged: BTreeMap::new(),
        }
    }

    /// Take a stable read position at the current committed version.
    pub fn snapshot(&self) -> Snapshot {
        let mut snaps = self.shared.snapshots.lock().unwrap();
        let version = self.shared.state.read().unwrap().committed_version;
        *snaps.entry(version).or_insert(0) += 1;
        drop(snaps);
        Snapshot {
            version,
            shared: self.shared.clone(),
        }
    }

    // --- auto-commit convenience ops ----------------------------------------
    // Each wraps a single-op transaction and retries a bounded number of
    // times on optimistic conflict.

    pub fn insert(&self, table: &str, record: Record) -> Result<String> {
        let mut last_err = None;
        for _ in 0..3 {
            let mut txn = self.begin();
            let id = txn.insert(table, record.clone())?;
            match txn.commit() {
                Ok(_) => return Ok(id),
                Err(Error::Conflict(m)) => last_err = Some(Error::Conflict(m)),
                Err(e) => return Err(e),
            }
        }
        Err(last_err.expect("loop ran"))
    }

    pub fn update(&self, table: &str, id: &str, patch: Record) -> Result<()> {
        let mut last_err = None;
        for _ in 0..3 {
            let mut txn = self.begin();
            txn.update(table, id, patch.clone())?;
            match txn.commit() {
                Ok(_) => return Ok(()),
                Err(Error::Conflict(m)) => last_err = Some(Error::Conflict(m)),
                Err(e) => return Err(e),
            }
        }
        Err(last_err.expect("loop ran"))
    }

    pub fn delete(&self, table: &str, id: &str) -> Result<bool> {
        let mut last_err = None;
        for _ in 0..3 {
            let mut txn = self.begin();
            let deleted = txn.delete(table, id)?;
            if !deleted {
                return Ok(false);
            }
            match txn.commit() {
                Ok(_) => return Ok(true),
                Err(Error::Conflict(m)) => last_err = Some(Error::Conflict(m)),
                Err(e) => return Err(e),
            }
        }
        Err(last_err.expect("loop ran"))
    }

    // --- reads ---------------------------------------------------------------

    /// Latest committed version of a record, or `None` if absent or deleted.
    pub fn get(&self, table: &str, id: &str) -> Result<Option<Record>> {
        shared_get_at(&self.shared, table, id, u64::MAX)
    }

    /// Read a record as of a snapshot.
    pub fn get_at(&self, snapshot: &Snapshot, table: &str, id: &str) -> Result<Option<Record>> {
        shared_get_at(&self.shared, table, id, snapshot.version)
    }

    /// All visible records of a table, ordered by id.
    pub fn scan(&self, table: &str) -> Result<Vec<(String, Record)>> {
        shared_scan_at(&self.shared, table, u64::MAX)
    }

    /// All records of a table as of a snapshot, ordered by id.
    pub fn scan_at(&self, snapshot: &Snapshot, table: &str) -> Result<Vec<(String, Record)>> {
        shared_scan_at(&self.shared, table, snapshot.version)
    }

    /// Equality lookup over the latest committed state. Uses the secondary
    /// index when one exists on the column; falls back to a full scan.
    pub fn find_eq(&self, table: &str, column: &str, value: &Value) -> Result<Vec<(String, Record)>> {
        let st = self.shared.state.read().unwrap();
        let schema = st
            .catalog
            .table(table)
            .ok_or_else(|| Error::TableNotFound(table.into()))?;
        if schema.column(column).is_none() {
            return Err(Error::SchemaViolation(format!("unknown column '{column}'")));
        }
        if value.is_null() {
            return Ok(Vec::new());
        }
        let mut out = Vec::new();
        if let Some(idx) = st.secondary.get(&(table.to_owned(), column.to_owned())) {
            if let Some(ids) = idx.map.get(&index_key(value)) {
                for id in ids {
                    if let Some(versions) = st.index.get(table).and_then(|t| t.get(id)) {
                        if let Some(last) = versions.last() {
                            if !last.is_tombstone() {
                                let mut rec = read_record_kind(&st.readers, &last.kind)?;
                                rec.insert(ID_COLUMN.into(), Value::Text(id.clone()));
                                out.push((id.clone(), rec));
                            }
                        }
                    }
                }
            }
        } else if let Some(ids) = st.index.get(table) {
            for (id, versions) in ids {
                if let Some(last) = versions.last() {
                    if !last.is_tombstone() {
                        let rec = read_record_kind(&st.readers, &last.kind)?;
                        if rec.get(column) == Some(value) {
                            let mut rec = rec;
                            rec.insert(ID_COLUMN.into(), Value::Text(id.clone()));
                            out.push((id.clone(), rec));
                        }
                    }
                }
            }
        }
        Ok(out)
    }

    // --- maintenance -----------------------------------------------------------

    /// Drain committed in-memory data into a new immutable segment, publish a
    /// new manifest, and rotate the WAL.
    pub fn checkpoint(&self) -> Result<()> {
        let mut cs = self.shared.commit.lock().unwrap();
        checkpoint_locked(&self.shared, &mut cs)
    }

    /// Rewrite segments keeping only versions visible to the latest state or
    /// to a live snapshot. Blocks writers for the duration (Phase 1).
    pub fn compact(&self) -> Result<()> {
        let mut cs = self.shared.commit.lock().unwrap();
        checkpoint_locked(&self.shared, &mut cs)?;

        let watermarks: Vec<u64> = {
            let snaps = self.shared.snapshots.lock().unwrap();
            let st = self.shared.state.read().unwrap();
            let mut w: Vec<u64> = snaps.keys().copied().collect();
            w.push(st.committed_version);
            w.sort_unstable();
            w.dedup();
            w
        };

        struct Kept {
            table: String,
            id: String,
            version: u64,
            payload: Option<Vec<u8>>,
        }
        let (mut kept, committed_version, seg_id, old_ids) = {
            let st = self.shared.state.read().unwrap();
            let mut kept: Vec<Kept> = Vec::new();
            for (table, ids) in &st.index {
                for (id, versions) in ids {
                    let mut keep_versions: BTreeSet<u64> = BTreeSet::new();
                    for &w in &watermarks {
                        if let Some(v) = versions.iter().rev().find(|e| e.version <= w) {
                            keep_versions.insert(v.version);
                        }
                    }
                    let mut have_put = false;
                    for v in versions {
                        if !keep_versions.contains(&v.version) {
                            continue;
                        }
                        if v.is_tombstone() {
                            if !have_put {
                                continue; // nothing older survives: drop entirely
                            }
                            kept.push(Kept {
                                table: table.clone(),
                                id: id.clone(),
                                version: v.version,
                                payload: None,
                            });
                        } else {
                            have_put = true;
                            kept.push(Kept {
                                table: table.clone(),
                                id: id.clone(),
                                version: v.version,
                                payload: Some(payload_bytes(&st.readers, &v.kind)?.expect("put has payload")),
                            });
                        }
                    }
                }
            }
            let old_ids: Vec<u32> = st.segments.iter().map(|s| s.id).collect();
            (kept, st.committed_version, st.next_segment_id, old_ids)
        };
        kept.sort_by_key(|k| k.version);

        let mut new_segments: Vec<SegmentMeta> = Vec::new();
        let mut new_index: HashMap<String, BTreeMap<String, Vec<VersionEntry>>> = HashMap::new();
        let mut new_readers: HashMap<u32, File> = HashMap::new();
        if !kept.is_empty() {
            let mut buf = Vec::new();
            let mut locs = Vec::with_capacity(kept.len());
            for k in &kept {
                let (entry, payload_rel) =
                    encode_entry(k.version, &k.table, &k.id, k.payload.as_deref());
                locs.push((
                    buf.len() as u64 + payload_rel,
                    k.payload.as_ref().map_or(0, |p| p.len() as u32),
                ));
                buf.extend_from_slice(&entry);
            }
            let seg_path = self
                .shared
                .dir
                .join(SEGMENTS_DIR)
                .join(segment_file_name(seg_id));
            let mut f = File::create(&seg_path)?;
            f.write_all(&buf)?;
            f.sync_all()?;
            fsync_dir(&self.shared.dir.join(SEGMENTS_DIR))?;
            new_segments.push(SegmentMeta {
                id: seg_id,
                len: buf.len() as u64,
            });
            new_readers.insert(seg_id, File::open(&seg_path)?);
            for (k, (offset, len)) in kept.iter().zip(&locs) {
                let kind = match k.payload {
                    Some(_) => VKind::SegPut {
                        segment: seg_id,
                        payload_offset: *offset,
                        payload_len: *len,
                    },
                    None => VKind::SegTombstone,
                };
                new_index
                    .entry(k.table.clone())
                    .or_default()
                    .entry(k.id.clone())
                    .or_default()
                    .push(VersionEntry {
                        version: k.version,
                        kind,
                    });
            }
        }

        Manifest {
            format_version: FORMAT_VERSION,
            committed_version,
            segments: new_segments.clone(),
            wal_id: cs.wal.id,
        }
        .publish(&self.shared.dir)?;

        {
            let mut st = self.shared.state.write().unwrap();
            st.index = new_index;
            st.readers = new_readers;
            st.segments = new_segments;
            st.next_segment_id = seg_id + 1;
        }
        for id in old_ids {
            let _ = fs::remove_file(
                self.shared
                    .dir
                    .join(SEGMENTS_DIR)
                    .join(segment_file_name(id)),
            );
        }
        Ok(())
    }
}

impl Txn {
    pub fn snapshot_version(&self) -> u64 {
        self.snapshot.version
    }

    /// Read through this transaction: staged writes first, then the snapshot.
    pub fn get(&self, table: &str, id: &str) -> Result<Option<Record>> {
        match self.staged.get(&(table.to_owned(), id.to_owned())) {
            Some(None) => Ok(None),
            Some(Some(rec)) => {
                let mut rec = rec.clone();
                rec.insert(ID_COLUMN.into(), Value::Text(id.to_owned()));
                Ok(Some(rec))
            }
            None => shared_get_at(&self.shared, table, id, self.snapshot.version),
        }
    }

    pub fn insert(&mut self, table: &str, record: Record) -> Result<String> {
        let schema = {
            let st = self.shared.state.read().unwrap();
            st.catalog
                .table(table)
                .ok_or_else(|| Error::TableNotFound(table.into()))?
                .clone()
        };
        let id = match record.get(ID_COLUMN) {
            None => Ulid::new().to_string(),
            Some(Value::Text(s)) if !s.is_empty() => s.clone(),
            Some(Value::Text(_)) => {
                return Err(Error::InvalidArgument("id must not be empty".into()))
            }
            Some(_) => return Err(Error::SchemaViolation("id must be a text value".into())),
        };
        let key = (table.to_owned(), id.clone());
        let exists = match self.staged.get(&key) {
            Some(Some(_)) => true,
            Some(None) => false,
            None => exists_at(&self.shared, table, &id, self.snapshot.version),
        };
        if exists {
            return Err(Error::DuplicateId {
                table: table.into(),
                id,
            });
        }
        let stored = build_stored(&schema, record)?;
        self.staged.insert(key, Some(stored.into_iter().collect()));
        Ok(id)
    }

    pub fn update(&mut self, table: &str, id: &str, patch: Record) -> Result<()> {
        let schema = {
            let st = self.shared.state.read().unwrap();
            st.catalog
                .table(table)
                .ok_or_else(|| Error::TableNotFound(table.into()))?
                .clone()
        };
        if patch.contains_key(ID_COLUMN) {
            return Err(Error::InvalidArgument(
                "the primary key cannot be updated".into(),
            ));
        }
        let key = (table.to_owned(), id.to_owned());
        let mut current = match self.staged.get(&key) {
            Some(Some(rec)) => rec.clone(),
            Some(None) => {
                return Err(Error::RecordNotFound {
                    table: table.into(),
                    id: id.into(),
                })
            }
            None => shared_get_at(&self.shared, table, id, self.snapshot.version)?.ok_or_else(
                || Error::RecordNotFound {
                    table: table.into(),
                    id: id.into(),
                },
            )?,
        };
        current.remove(ID_COLUMN);
        for (name, value) in patch {
            let col = schema
                .column(&name)
                .ok_or_else(|| Error::SchemaViolation(format!("unknown column '{name}'")))?;
            check_value(col, &value)?;
            current.insert(name, value);
        }
        self.staged.insert(key, Some(current));
        Ok(())
    }

    pub fn delete(&mut self, table: &str, id: &str) -> Result<bool> {
        {
            let st = self.shared.state.read().unwrap();
            if st.catalog.table(table).is_none() {
                return Err(Error::TableNotFound(table.into()));
            }
        }
        let key = (table.to_owned(), id.to_owned());
        let exists = match self.staged.get(&key) {
            Some(Some(_)) => true,
            Some(None) => false,
            None => exists_at(&self.shared, table, id, self.snapshot.version),
        };
        if !exists {
            return Ok(false);
        }
        self.staged.insert(key, None);
        Ok(true)
    }

    /// Validate optimistically and publish all staged writes as one atomic
    /// commit. Returns the commit version, or `Error::Conflict` if any
    /// touched record changed after this transaction began.
    pub fn commit(self) -> Result<u64> {
        commit_staged(&self.shared, self.snapshot.version, &self.staged)
    }

    /// Discard all staged writes.
    pub fn rollback(self) {}
}

// --- internals ----------------------------------------------------------------

fn acquire_lock(dir: &Path) -> Result<File> {
    let file = OpenOptions::new()
        .create(true)
        .truncate(false)
        .write(true)
        .open(dir.join(LOCK_FILE))?;
    match file.try_lock() {
        Ok(()) => Ok(file),
        Err(TryLockError::WouldBlock) => Err(Error::DatabaseLocked(dir.display().to_string())),
        Err(TryLockError::Error(e)) => Err(Error::Io(e)),
    }
}

fn cleanup_orphans(dir: &Path, manifest: &Manifest) -> Result<()> {
    let listed: HashSet<u32> = manifest.segments.iter().map(|s| s.id).collect();
    for dirent in fs::read_dir(dir.join(SEGMENTS_DIR))? {
        let dirent = dirent?;
        let name = dirent.file_name();
        let name = name.to_string_lossy();
        if let Some(stem) = name.strip_suffix(".seg") {
            if let Ok(id) = stem.parse::<u32>() {
                if !listed.contains(&id) {
                    let _ = fs::remove_file(dirent.path());
                }
            }
        }
    }
    for dirent in fs::read_dir(dir.join(WAL_DIR))? {
        let dirent = dirent?;
        let name = dirent.file_name();
        let name = name.to_string_lossy();
        if let Some(stem) = name.strip_suffix(".wal") {
            if let Ok(id) = stem.parse::<u32>() {
                if id != manifest.wal_id {
                    let _ = fs::remove_file(dirent.path());
                }
            }
        }
    }
    Ok(())
}

fn shared_get_at(shared: &Shared, table: &str, id: &str, max_version: u64) -> Result<Option<Record>> {
    let st = shared.state.read().unwrap();
    if st.catalog.table(table).is_none() {
        return Err(Error::TableNotFound(table.into()));
    }
    let entry = st
        .index
        .get(table)
        .and_then(|t| t.get(id))
        .and_then(|versions| versions.iter().rev().find(|e| e.version <= max_version));
    match entry {
        Some(e) if !e.is_tombstone() => {
            let mut rec = read_record_kind(&st.readers, &e.kind)?;
            rec.insert(ID_COLUMN.into(), Value::Text(id.to_owned()));
            Ok(Some(rec))
        }
        _ => Ok(None),
    }
}

fn shared_scan_at(shared: &Shared, table: &str, max_version: u64) -> Result<Vec<(String, Record)>> {
    let st = shared.state.read().unwrap();
    if st.catalog.table(table).is_none() {
        return Err(Error::TableNotFound(table.into()));
    }
    let mut out = Vec::new();
    if let Some(ids) = st.index.get(table) {
        for (id, versions) in ids {
            if let Some(e) = versions.iter().rev().find(|e| e.version <= max_version) {
                if !e.is_tombstone() {
                    let mut rec = read_record_kind(&st.readers, &e.kind)?;
                    rec.insert(ID_COLUMN.into(), Value::Text(id.clone()));
                    out.push((id.clone(), rec));
                }
            }
        }
    }
    Ok(out)
}

fn exists_at(shared: &Shared, table: &str, id: &str, max_version: u64) -> bool {
    let st = shared.state.read().unwrap();
    st.index
        .get(table)
        .and_then(|t| t.get(id))
        .and_then(|versions| versions.iter().rev().find(|e| e.version <= max_version))
        .map(|e| !e.is_tombstone())
        .unwrap_or(false)
}

/// The optimistic commit path. Serialized by the commit mutex; readers are
/// only blocked during the short in-memory apply at the end.
fn commit_staged(
    shared: &Arc<Shared>,
    snap_version: u64,
    staged: &BTreeMap<(String, String), Option<Record>>,
) -> Result<u64> {
    if staged.is_empty() {
        return Ok(shared.state.read().unwrap().committed_version);
    }
    let mut cs = shared.commit.lock().unwrap();

    type EncodedChange = (String, String, Option<(Arc<Vec<u8>>, Record)>);
    let mut encoded: Vec<EncodedChange> = Vec::with_capacity(staged.len());
    let commit_version;
    {
        let st = shared.state.read().unwrap();
        for ((table, id), op) in staged {
            // Write-write conflict: someone committed a change to this record
            // after our snapshot.
            if let Some(last) = st
                .index
                .get(table)
                .and_then(|t| t.get(id))
                .and_then(|v| v.last())
            {
                if last.version > snap_version {
                    return Err(Error::Conflict(format!(
                        "{table}/{id} changed after this transaction began"
                    )));
                }
            }
            match op {
                Some(rec) => {
                    let schema = st
                        .catalog
                        .table(table)
                        .ok_or_else(|| Error::TableNotFound(table.clone()))?;
                    let stored: Vec<(String, Value)> = schema
                        .columns
                        .iter()
                        .map(|c| {
                            (
                                c.name.clone(),
                                rec.get(&c.name).cloned().unwrap_or(Value::Null),
                            )
                        })
                        .collect();
                    encoded.push((
                        table.clone(),
                        id.clone(),
                        Some((Arc::new(encode_record(&stored)), rec.clone())),
                    ));
                }
                None => encoded.push((table.clone(), id.clone(), None)),
            }
        }
        validate_unique(&st, staged)?;
        commit_version = st.committed_version + 1;
    }

    // Durability point: the WAL record is the commit.
    let wal_changes: Vec<(&str, &str, Option<&[u8]>)> = encoded
        .iter()
        .map(|(t, i, p)| (t.as_str(), i.as_str(), p.as_ref().map(|(b, _)| b.as_slice())))
        .collect();
    let bytes = encode_commit(commit_version, &wal_changes);
    cs.wal
        .append_commit(&bytes, shared.opts.durability, shared.opts.balanced_sync_interval_ms)?;

    // Publish atomically to readers.
    let mut added = 0u64;
    {
        let mut st = shared.state.write().unwrap();
        for (table, id, p) in &encoded {
            apply_one(&mut st, commit_version, table, id, p.as_ref())?;
            added += p.as_ref().map_or(0, |(b, _)| b.len() as u64) + 32;
        }
        st.committed_version = commit_version;
    }
    cs.memtable_bytes += added;
    if cs.memtable_bytes >= shared.opts.memtable_max_bytes {
        checkpoint_locked(shared, &mut cs)?;
    }
    Ok(commit_version)
}

fn validate_unique(
    st: &State,
    staged: &BTreeMap<(String, String), Option<Record>>,
) -> Result<()> {
    let mut staged_new: HashMap<(String, String, Vec<u8>), String> = HashMap::new();
    for ((table, id), op) in staged {
        let Some(rec) = op else { continue };
        let Some(schema) = st.catalog.table(table) else {
            continue;
        };
        for def in &schema.indexes {
            if !def.unique {
                continue;
            }
            let Some(v) = rec.get(&def.column) else { continue };
            if v.is_null() {
                continue;
            }
            let keyb = index_key(v);
            if let Some(prev) = staged_new.insert(
                (table.clone(), def.column.clone(), keyb.clone()),
                id.clone(),
            ) {
                if prev != *id {
                    return Err(Error::UniqueViolation {
                        table: table.clone(),
                        column: def.column.clone(),
                    });
                }
            }
            if let Some(idx) = st.secondary.get(&(table.clone(), def.column.clone())) {
                if let Some(holders) = idx.map.get(&keyb) {
                    for holder in holders {
                        // A holder also written by this txn is judged by its
                        // staged value (covered by the staged_new check).
                        if holder != id && !staged.contains_key(&(table.clone(), holder.clone()))
                        {
                            return Err(Error::UniqueViolation {
                                table: table.clone(),
                                column: def.column.clone(),
                            });
                        }
                    }
                }
            }
        }
    }
    Ok(())
}

/// Apply one committed change to the in-memory state, maintaining secondary
/// indexes (which track the latest committed state only).
fn apply_one(
    st: &mut State,
    version: u64,
    table: &str,
    id: &str,
    put: Option<&(Arc<Vec<u8>>, Record)>,
) -> Result<()> {
    let defs: Vec<IndexDef> = st
        .catalog
        .table(table)
        .map(|t| t.indexes.clone())
        .unwrap_or_default();
    if !defs.is_empty() {
        let prior: Option<Record> = match st
            .index
            .get(table)
            .and_then(|t| t.get(id))
            .and_then(|v| v.last())
        {
            Some(last) if !last.is_tombstone() => {
                Some(read_record_kind(&st.readers, &last.kind)?)
            }
            _ => None,
        };
        if let Some(prior) = prior {
            for def in &defs {
                if let Some(v) = prior.get(&def.column) {
                    if !v.is_null() {
                        if let Some(idx) =
                            st.secondary.get_mut(&(table.to_owned(), def.column.clone()))
                        {
                            let key = index_key(v);
                            if let Some(set) = idx.map.get_mut(&key) {
                                set.remove(id);
                                if set.is_empty() {
                                    idx.map.remove(&key);
                                }
                            }
                        }
                    }
                }
            }
        }
    }
    let kind = match put {
        Some((payload, _)) => VKind::MemPut(payload.clone()),
        None => VKind::MemTombstone,
    };
    st.index
        .entry(table.to_owned())
        .or_default()
        .entry(id.to_owned())
        .or_default()
        .push(VersionEntry { version, kind });
    if let Some((_, rec)) = put {
        for def in &defs {
            if let Some(v) = rec.get(&def.column) {
                if !v.is_null() {
                    if let Some(idx) =
                        st.secondary.get_mut(&(table.to_owned(), def.column.clone()))
                    {
                        idx.map
                            .entry(index_key(v))
                            .or_default()
                            .insert(id.to_owned());
                    }
                }
            }
        }
    }
    Ok(())
}

/// Drain committed in-memory data into a new segment, publish a new manifest
/// referencing it, and rotate the WAL. Runs under the commit mutex.
fn checkpoint_locked(shared: &Arc<Shared>, cs: &mut CommitState) -> Result<()> {
    struct MemEntry {
        table: String,
        id: String,
        version: u64,
        payload: Option<Arc<Vec<u8>>>,
    }
    let (mut mem, segments, committed_version, next_segment_id) = {
        let st = shared.state.read().unwrap();
        let mut mem: Vec<MemEntry> = Vec::new();
        for (table, ids) in &st.index {
            for (id, versions) in ids {
                for v in versions {
                    match &v.kind {
                        VKind::MemPut(p) => mem.push(MemEntry {
                            table: table.clone(),
                            id: id.clone(),
                            version: v.version,
                            payload: Some(p.clone()),
                        }),
                        VKind::MemTombstone => mem.push(MemEntry {
                            table: table.clone(),
                            id: id.clone(),
                            version: v.version,
                            payload: None,
                        }),
                        _ => {}
                    }
                }
            }
        }
        (
            mem,
            st.segments.clone(),
            st.committed_version,
            st.next_segment_id,
        )
    };
    if mem.is_empty() && cs.wal.len == 0 {
        return Ok(());
    }
    mem.sort_by_key(|m| m.version);

    let mut new_segments = segments;
    let mut written: Option<(u32, Vec<(u64, u32)>)> = None;
    if !mem.is_empty() {
        let seg_id = next_segment_id;
        let mut buf = Vec::new();
        let mut locs = Vec::with_capacity(mem.len());
        for m in &mem {
            let (entry, payload_rel) =
                encode_entry(m.version, &m.table, &m.id, m.payload.as_ref().map(|p| p.as_slice()));
            locs.push((
                buf.len() as u64 + payload_rel,
                m.payload.as_ref().map_or(0, |p| p.len() as u32),
            ));
            buf.extend_from_slice(&entry);
        }
        let seg_path = shared.dir.join(SEGMENTS_DIR).join(segment_file_name(seg_id));
        let mut f = File::create(&seg_path)?;
        f.write_all(&buf)?;
        f.sync_all()?;
        fsync_dir(&shared.dir.join(SEGMENTS_DIR))?;
        new_segments.push(SegmentMeta {
            id: seg_id,
            len: buf.len() as u64,
        });
        written = Some((seg_id, locs));
    }

    // Create the new WAL before the manifest that references it, so the
    // manifest never points at a missing file.
    let new_wal_id = cs.wal.id + 1;
    File::create(wal_path(&shared.dir, new_wal_id))?.sync_all()?;
    fsync_dir(&shared.dir.join(WAL_DIR))?;
    Manifest {
        format_version: FORMAT_VERSION,
        committed_version,
        segments: new_segments.clone(),
        wal_id: new_wal_id,
    }
    .publish(&shared.dir)?;
    let _ = fs::remove_file(wal_path(&shared.dir, cs.wal.id));

    {
        let mut st = shared.state.write().unwrap();
        if let Some((seg_id, locs)) = &written {
            let seg_path = shared.dir.join(SEGMENTS_DIR).join(segment_file_name(*seg_id));
            st.readers.insert(*seg_id, File::open(&seg_path)?);
            st.next_segment_id = seg_id + 1;
            for (m, (offset, len)) in mem.iter().zip(locs) {
                let versions = st
                    .index
                    .get_mut(&m.table)
                    .and_then(|t| t.get_mut(&m.id))
                    .expect("indexed entry present");
                let ve = versions
                    .iter_mut()
                    .find(|v| v.version == m.version)
                    .expect("version present");
                ve.kind = match m.payload {
                    Some(_) => VKind::SegPut {
                        segment: *seg_id,
                        payload_offset: *offset,
                        payload_len: *len,
                    },
                    None => VKind::SegTombstone,
                };
            }
        }
        st.segments = new_segments;
    }
    cs.wal = WalWriter::open(&shared.dir, new_wal_id)?;
    cs.memtable_bytes = 0;
    Ok(())
}

fn build_secondary(
    catalog: &Catalog,
    index: &HashMap<String, BTreeMap<String, Vec<VersionEntry>>>,
    readers: &HashMap<u32, File>,
) -> Result<HashMap<(String, String), SecIdx>> {
    let mut secondary: HashMap<(String, String), SecIdx> = HashMap::new();
    for table in &catalog.tables {
        for def in &table.indexes {
            secondary.insert(
                (table.name.clone(), def.column.clone()),
                SecIdx {
                    map: HashMap::new(),
                },
            );
        }
    }
    if secondary.is_empty() {
        return Ok(secondary);
    }
    for table in &catalog.tables {
        if table.indexes.is_empty() {
            continue;
        }
        let Some(ids) = index.get(&table.name) else {
            continue;
        };
        for (id, versions) in ids {
            let Some(last) = versions.last() else { continue };
            if last.is_tombstone() {
                continue;
            }
            let rec = read_record_kind(readers, &last.kind)?;
            for def in &table.indexes {
                if let Some(v) = rec.get(&def.column) {
                    if !v.is_null() {
                        secondary
                            .get_mut(&(table.name.clone(), def.column.clone()))
                            .expect("initialized above")
                            .map
                            .entry(index_key(v))
                            .or_default()
                            .insert(id.clone());
                    }
                }
            }
        }
    }
    Ok(secondary)
}

fn payload_bytes(readers: &HashMap<u32, File>, kind: &VKind) -> Result<Option<Vec<u8>>> {
    match kind {
        VKind::MemPut(p) => Ok(Some(p.as_ref().clone())),
        VKind::SegPut {
            segment,
            payload_offset,
            payload_len,
        } => {
            let file = readers
                .get(segment)
                .ok_or_else(|| Error::Corrupt(format!("missing segment {segment}")))?;
            let mut buf = vec![0u8; *payload_len as usize];
            file.read_exact_at(&mut buf, *payload_offset)?;
            Ok(Some(buf))
        }
        VKind::MemTombstone | VKind::SegTombstone => Ok(None),
    }
}

fn read_record_kind(readers: &HashMap<u32, File>, kind: &VKind) -> Result<Record> {
    match kind {
        VKind::MemPut(p) => decode_record(p),
        VKind::SegPut { .. } => {
            let bytes = payload_bytes(readers, kind)?.expect("put has payload");
            decode_record(&bytes)
        }
        VKind::MemTombstone | VKind::SegTombstone => {
            Err(Error::Corrupt("attempted to read a tombstone".into()))
        }
    }
}

fn index_key(v: &Value) -> Vec<u8> {
    let mut buf = Vec::new();
    encode_value(&mut buf, v);
    buf
}

fn check_value(col: &crate::schema::Column, value: &Value) -> Result<()> {
    if value.is_null() {
        if !col.nullable {
            return Err(Error::SchemaViolation(format!(
                "column '{}' is not nullable",
                col.name
            )));
        }
        return Ok(());
    }
    if !value.matches(col.ty) {
        return Err(Error::SchemaViolation(format!(
            "column '{}' expects {}, got {:?}",
            col.name, col.ty, value
        )));
    }
    Ok(())
}

/// Validate an insert against the schema and produce the stored field list
/// in schema column order. Missing nullable columns are stored as Null.
fn build_stored(schema: &TableSchema, mut record: Record) -> Result<Vec<(String, Value)>> {
    record.remove(ID_COLUMN);
    for name in record.keys() {
        if schema.column(name).is_none() {
            return Err(Error::SchemaViolation(format!("unknown column '{name}'")));
        }
    }
    let mut stored = Vec::with_capacity(schema.columns.len());
    for col in &schema.columns {
        let value = record.remove(&col.name).unwrap_or(Value::Null);
        check_value(col, &value)?;
        stored.push((col.name.clone(), value));
    }
    Ok(stored)
}

// Record payload layout: u16 field count, then per field a u16-length-prefixed
// column name followed by a tagged value. Self-describing so that schema
// evolution in later phases does not invalidate old segments.

pub(crate) fn encode_record(fields: &[(String, Value)]) -> Vec<u8> {
    let mut buf = Vec::with_capacity(64);
    buf.extend_from_slice(&(fields.len() as u16).to_le_bytes());
    for (name, value) in fields {
        buf.extend_from_slice(&(name.len() as u16).to_le_bytes());
        buf.extend_from_slice(name.as_bytes());
        encode_value(&mut buf, value);
    }
    buf
}

pub(crate) fn decode_record(buf: &[u8]) -> Result<Record> {
    let mut pos = 0usize;
    let count = read_u16(buf, &mut pos)? as usize;
    let mut record = Record::new();
    for _ in 0..count {
        let name_len = read_u16(buf, &mut pos)? as usize;
        let end = pos
            .checked_add(name_len)
            .ok_or_else(|| Error::Corrupt("length overflow".into()))?;
        let name_bytes = buf
            .get(pos..end)
            .ok_or_else(|| Error::Corrupt("unexpected end of record".into()))?;
        pos = end;
        let name = std::str::from_utf8(name_bytes)
            .map_err(|_| Error::Corrupt("invalid utf8 in column name".into()))?
            .to_owned();
        let value = decode_value(buf, &mut pos)?;
        record.insert(name, value);
    }
    Ok(record)
}
