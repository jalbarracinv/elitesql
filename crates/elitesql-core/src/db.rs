use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet, VecDeque};
use std::fs::{self, File, OpenOptions, TryLockError};
use std::io::{BufWriter, Read, Seek, SeekFrom, Write};
use std::ops::{Deref, DerefMut};
use std::os::unix::fs::FileExt;
use std::panic::{catch_unwind, AssertUnwindSafe};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering as AtomicOrdering};
use std::sync::{mpsc, Arc, Condvar, Mutex, MutexGuard, RwLock};
use std::time::{Duration, Instant};

use memmap2::{Advice, MmapOptions};
use parking_lot::{Mutex as ParkingMutex, MutexGuard as ParkingMutexGuard};
use ulid::Ulid;

use crate::ddl::{DdlIntent, Rewrite};
use crate::error::{Error, Result};
use crate::manifest::PublishOutcome;
use crate::manifest::{fsync_dir, Manifest, SegmentMeta};
use crate::memory::{GlobalMemoryStats, MemoryGovernor, MemoryLimits, MemoryPermit, MemoryPool};
use crate::paged::{
    merge_paged_indexes, merge_paged_indexes_latest_value, ExternalPagedWriter, PagedIndex,
    PagedPrefixCursor, PagedWriter,
};
use crate::run_manifest::{
    DerivedRunKind, DerivedRunManifest, DerivedRunMeta, PreparedDerivedRunManifest,
    PrimaryRunManifest, PrimaryRunMeta,
};
use crate::schema::{
    validate_short_string, Catalog, Column, IndexDef, ReferentialAction, TableSchema,
    FORMAT_VERSION, ID_COLUMN,
};
use crate::segment::{
    encode_entry_into, segment_file_name, validate_segment, visit_segment, KIND_PUT, KIND_TOMBSTONE,
};
use crate::text::{
    validate_run as validate_text_run, FrozenTextDelta, TextHit, TextIdx, TextIndexDef, TextRun,
};
use crate::value::{
    decode_value, encode_blob_ref, encode_value, encoded_value_eq, read_u16, read_u32, read_u64,
    read_u8, skip_value, write_blob_file_bytes, BlobRef, ColumnType, Value, TAG_NULL,
};
use crate::vector::{
    IndexingMode, VecIdx, VectorHit, VectorIndexDef, VectorIndexOptions, VectorSearchOptions,
};
use crate::wal::{
    encode_commit, scan_wal, set_encoded_commit_version, wal_path, Durability, WalAppendOutcome,
    WalWriter, WAL_DIR,
};

pub(crate) const MARKER_FILE: &str = "ELITESQL";
pub(crate) const CATALOG_FILE: &str = "catalog.json";
pub(crate) const SEGMENTS_DIR: &str = "segments";
pub(crate) const BLOBS_DIR: &str = "blobs";
const VECTORS_DIR: &str = "vectors";
const INDEXES_DIR: &str = "indexes";
pub(crate) const LOCK_FILE: &str = "LOCK";
const CREATION_FILE: &str = ".ELITESQL.creating";
const CREATION_MAGIC: &[u8] = b"elitesql creation journal v1\n";

// Primary runs are compact and range-pruned. A wider tier reduces rewrite
// amplification for append-heavy ingest while keeping the number of searched
// runs logarithmically bounded.
const PRIMARY_LEVEL_FANOUT: usize = 16;
const PRIMARY_BASE_LEVEL: u8 = u8::MAX;
const DERIVED_LEVEL_FANOUT: usize = 8;
const DERIVED_BASE_LEVEL: u8 = u8::MAX;
const MAX_MAINTENANCE_WAIT_RETRIES: u64 = 8;
/// Bound one coordinated publication so a sustained producer stream cannot
/// monopolize the commit mutex indefinitely.
const COMMIT_COORDINATOR_MAX_BATCH: usize = 64;

/// Optimistic-conflict retries per backfill batch during `ADD COLUMN`.
const BACKFILL_RETRIES: usize = 3;

/// Which of the three index kinds a DDL statement targets.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum IndexKind {
    Secondary,
    Vector,
    Text,
}

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
    /// Maximum time a contended `Safe` commit leader waits for peer commits
    /// before issuing the strict WAL durability barrier. `0` disables the
    /// intentional coalescing window without changing durability semantics.
    pub safe_group_commit_delay_us: u64,
    /// Open read-only: shared lock, tolerant loading (valid prefixes of
    /// corrupt files are exposed instead of failing), zero writes to disk,
    /// every write operation returns `Error::ReadOnly`. This is the
    /// last-resort mode for inspecting a damaged database before `salvage`.
    pub read_only: bool,
    /// Blob values at or above this size are stored out-of-line in `blobs/`
    /// (checksummed chunk files) instead of inline in segments/WAL.
    pub external_blob_threshold: usize,
    /// Policy for reclaiming obsolete versions and merging excessive segment
    /// counts without application intervention.
    pub auto_compaction: AutoCompactionOptions,
    /// Memory policy shared by traditional SQL execution. The returned result
    /// itself is owned by the caller and is not charged to this working-set
    /// budget; use a cursor/streaming API for an unbounded result set.
    pub memory: MemoryOptions,
}

/// Bounded working-memory policy for query execution.
///
/// Operators that cannot stay within `query_working_bytes` must switch to a
/// bounded algorithm (top-k, batching, or temporary spill files) instead of
/// growing in proportion to the table size.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MemoryOptions {
    /// Total engine-owned working-memory envelope for one open database.
    /// Clean file-backed mmap pages and result values already returned to the
    /// caller are controlled by the OS/caller and are not charged here.
    pub total_memory_bytes: usize,
    /// Shared capacity for all concurrent SQL, text and vector searches.
    pub query_pool_bytes: usize,
    /// Working memory available to one SQL query, excluding rows returned to
    /// the caller. This is deliberately per-query; a global concurrent-query
    /// governor admits only as many simultaneous reservations as fit in
    /// `query_pool_bytes`.
    pub query_working_bytes: usize,
    /// Maximum retained estimate for WAL/primary and derived-index deltas.
    /// Crossing the threshold consolidates mutable state into mmap bases.
    pub index_delta_pool_bytes: usize,
    /// Capacity reserved for one checkpoint, index consolidation or
    /// compaction. Maintenance and queries therefore cannot consume each
    /// other's guaranteed pool.
    pub maintenance_pool_bytes: usize,
    /// Deliberately unused headroom for allocator/runtime overhead and error
    /// handling. It belongs to the total but to no allocatable pool.
    pub reserved_memory_bytes: usize,
    /// Maximum number of records decoded by a table scan at once.
    pub scan_batch_rows: usize,
    /// Directory for ephemeral query runs. `None` uses the operating system's
    /// temporary directory. Spill files are removed on success and error.
    pub spill_directory: Option<PathBuf>,
}

impl MemoryOptions {
    /// Opt-in 512 MiB profile for sustained transactional ingestion. Query
    /// memory stays conservative; the extra budget goes to larger mutable
    /// deltas and maintenance batches.
    pub fn ingest_performance() -> Self {
        Self {
            total_memory_bytes: 512 * 1024 * 1024,
            query_pool_bytes: 64 * 1024 * 1024,
            query_working_bytes: 16 * 1024 * 1024,
            index_delta_pool_bytes: 192 * 1024 * 1024,
            maintenance_pool_bytes: 192 * 1024 * 1024,
            reserved_memory_bytes: 8 * 1024 * 1024,
            scan_batch_rows: 512,
            spill_directory: None,
        }
    }

    fn validate(&self) -> Result<()> {
        if self.total_memory_bytes == 0 {
            return Err(Error::InvalidArgument(
                "memory.total_memory_bytes must be positive".into(),
            ));
        }
        if self.query_pool_bytes == 0
            || self.index_delta_pool_bytes == 0
            || self.maintenance_pool_bytes == 0
        {
            return Err(Error::InvalidArgument(
                "memory query/index/maintenance pools must be positive".into(),
            ));
        }
        if self.query_working_bytes == 0 {
            return Err(Error::InvalidArgument(
                "memory.query_working_bytes must be positive".into(),
            ));
        }
        if self.scan_batch_rows == 0 {
            return Err(Error::InvalidArgument(
                "memory.scan_batch_rows must be positive".into(),
            ));
        }
        if self.query_working_bytes > self.query_pool_bytes {
            return Err(Error::InvalidArgument(
                "memory.query_working_bytes cannot exceed memory.query_pool_bytes".into(),
            ));
        }
        if self.index_delta_pool_bytes > self.maintenance_pool_bytes {
            return Err(Error::InvalidArgument(
                "memory.index_delta_pool_bytes cannot exceed memory.maintenance_pool_bytes".into(),
            ));
        }
        let assigned = self
            .query_pool_bytes
            .checked_add(self.index_delta_pool_bytes)
            .and_then(|bytes| bytes.checked_add(self.maintenance_pool_bytes))
            .and_then(|bytes| bytes.checked_add(self.reserved_memory_bytes))
            .ok_or_else(|| Error::InvalidArgument("memory pool sizes overflow usize".into()))?;
        if assigned > self.total_memory_bytes {
            return Err(Error::InvalidArgument(format!(
                "memory pools plus reserve ({assigned} bytes) exceed total_memory_bytes ({})",
                self.total_memory_bytes
            )));
        }
        Ok(())
    }

    fn limits(&self) -> MemoryLimits {
        MemoryLimits {
            total: self.total_memory_bytes,
            query: self.query_pool_bytes,
            index_delta: self.index_delta_pool_bytes,
            maintenance: self.maintenance_pool_bytes,
            reserve: self.reserved_memory_bytes,
        }
    }
}

impl Default for MemoryOptions {
    fn default() -> Self {
        Self {
            total_memory_bytes: 384 * 1024 * 1024,
            query_pool_bytes: 64 * 1024 * 1024,
            query_working_bytes: 16 * 1024 * 1024,
            index_delta_pool_bytes: 128 * 1024 * 1024,
            maintenance_pool_bytes: 128 * 1024 * 1024,
            reserved_memory_bytes: 8 * 1024 * 1024,
            scan_batch_rows: 512,
            spill_directory: None,
        }
    }
}

/// Conservative automatic-compaction policy.
///
/// Superseding writes accumulate a compaction debt. After a checkpoint, the
/// background maintenance worker compacts when the operation threshold and
/// reclaimable-byte ratio agree, when the absolute reclaimable-byte limit is
/// reached, or when too many immutable segments have accumulated.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AutoCompactionOptions {
    pub enabled: bool,
    /// Superseding committed row operations before evaluating the reclaim
    /// ratio. Multi-row statements count affected rows, not statements.
    pub min_obsolete_operations: u64,
    /// Minimum estimated obsolete share of segment bytes, from 0 through 100.
    pub min_reclaim_ratio_percent: u8,
    /// Compact regardless of operation count once this many segment bytes are
    /// estimated to be reclaimable.
    pub force_reclaim_bytes: u64,
    /// Merge segments at this count even for an insert-only workload.
    pub max_segments: usize,
    /// Minimum delay after one automatic attempt before another can be queued.
    pub min_interval_ms: u64,
}

impl AutoCompactionOptions {
    pub fn disabled() -> Self {
        Self {
            enabled: false,
            ..Self::default()
        }
    }

    fn validate(&self) -> Result<()> {
        if self.min_reclaim_ratio_percent > 100 {
            return Err(Error::InvalidArgument(
                "auto_compaction.min_reclaim_ratio_percent must be between 0 and 100".into(),
            ));
        }
        if self.enabled && self.min_obsolete_operations == 0 {
            return Err(Error::InvalidArgument(
                "auto_compaction.min_obsolete_operations must be positive".into(),
            ));
        }
        if self.enabled && self.force_reclaim_bytes == 0 {
            return Err(Error::InvalidArgument(
                "auto_compaction.force_reclaim_bytes must be positive".into(),
            ));
        }
        if self.enabled && self.max_segments < 2 {
            return Err(Error::InvalidArgument(
                "auto_compaction.max_segments must be at least 2".into(),
            ));
        }
        Ok(())
    }
}

impl Default for AutoCompactionOptions {
    fn default() -> Self {
        Self {
            enabled: true,
            min_obsolete_operations: 10_000,
            min_reclaim_ratio_percent: 25,
            force_reclaim_bytes: 256 * 1024 * 1024,
            max_segments: 64,
            min_interval_ms: 60_000,
        }
    }
}

impl Default for DbOptions {
    fn default() -> Self {
        DbOptions {
            durability: Durability::Safe,
            // The measured 100K x 64-dimensional vector workload stays below
            // the default 128 MiB mutable-index pool and publishes one complete
            // restartable graph. A 64 MiB memtable avoids an earlier checkpoint.
            memtable_max_bytes: 64 * 1024 * 1024,
            balanced_sync_interval_ms: 25,
            safe_group_commit_delay_us: 200,
            read_only: false,
            external_blob_threshold: 256 * 1024,
            auto_compaction: AutoCompactionOptions::default(),
            memory: MemoryOptions::default(),
        }
    }
}

impl DbOptions {
    /// Opt-in bounded profile for faster sustained ingestion. The 384 MiB
    /// profile remains [`Default`].
    pub fn ingest_performance() -> Self {
        Self {
            memtable_max_bytes: 128 * 1024 * 1024,
            memory: MemoryOptions::ingest_performance(),
            ..Self::default()
        }
    }
}

/// Cumulative memory-pressure counters for SQL queries on this handle.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct QueryMemoryStats {
    /// Temporary sorted/partitioned runs created by queries.
    pub spill_files: u64,
    /// Bytes written to temporary query runs.
    pub spilled_bytes: u64,
    /// Largest estimated operator buffer observed before flushing/truncating.
    pub peak_buffer_bytes: u64,
}

/// Cumulative maintenance work performed by this database handle.
///
/// The scalable benchmark uses this to separate time spent committing rows
/// from automatic and explicit checkpoints. Counters start at zero whenever
/// a database is created or opened.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct MaintenanceStats {
    /// Successful non-empty commits observed by this handle.
    pub commits: u64,
    /// End-to-end time inside `Txn::commit`, including lock wait and any
    /// synchronous checkpoint triggered by memory thresholds.
    pub commit_time: Duration,
    pub commit_lock_wait_time: Duration,
    /// Time spent owning the serialization mutex in the main commit phase.
    pub commit_lock_hold_time: Duration,
    /// Conflict checks, record encoding, unique validation and memory sizing.
    pub commit_prepare_time: Duration,
    /// Conflict/constraint validation performed while serialization is held.
    pub commit_locked_prepare_time: Duration,
    /// WAL encoding and append time (including the configured durability sync).
    pub commit_wal_time: Duration,
    /// Version patching and WAL append, excluding group-sync wait.
    pub commit_wal_append_time: Duration,
    /// Physical WAL sync calls. In Safe/Balanced modes this may be lower than
    /// `commits` because concurrent commits share one group sync.
    pub wal_syncs: u64,
    /// Total wall time spent inside physical WAL sync calls.
    pub wal_sync_time: Duration,
    /// WAL bytes covered by physical sync calls and the largest observed
    /// commits/bytes group for one call.
    pub wal_synced_bytes: u64,
    pub wal_sync_max_group_commits: u64,
    pub wal_sync_max_group_bytes: u64,
    /// Intentional Safe coalescing delay and time subsequently spent waiting
    /// to reacquire the commit mutex as group leader.
    pub wal_group_coalesce_time: Duration,
    pub wal_group_leader_lock_wait_time: Duration,
    /// Commits that joined a WAL sync group containing at least two commits.
    pub grouped_commits: u64,
    /// Eligible insert batches published by the commit coordinator and
    /// the number of individual transactions they contained.
    pub coordinated_batches: u64,
    pub coordinated_commits: u64,
    /// Point reads that yielded because active state writers had already
    /// filled the CPU-aware mixed-workload reader allowance.
    pub point_read_throttles: u64,
    /// In-memory publication to primary and derived mutable indexes.
    pub commit_apply_time: Duration,
    pub checkpoints: u64,
    pub checkpoint_time: Duration,
    pub automatic_compactions: u64,
    pub automatic_compaction_time: Duration,
    pub automatic_compaction_failures: u64,
    pub automatic_compaction_bytes_reclaimed: u64,
    /// Current committed operations/obsolete entries awaiting reclamation.
    pub compaction_debt_operations: u64,
    /// Current conservative estimate based on immutable segment bytes.
    pub estimated_reclaimable_bytes: u64,
    pub segments: usize,
    /// Immutable mmap runs currently searched by the primary MVCC directory.
    pub primary_runs: usize,
    /// Completed background level promotions for primary-index runs.
    pub primary_run_compactions: u64,
    pub primary_run_compaction_time: Duration,
    pub primary_run_compaction_bytes_read: u64,
    pub primary_run_compaction_bytes_written: u64,
    /// Bytes written when checkpoints publish their primary-index delta runs.
    pub primary_checkpoint_bytes_written: u64,
    /// Immutable runs across every secondary equality index.
    pub secondary_runs: usize,
    pub secondary_run_compactions: u64,
    pub secondary_run_compaction_time: Duration,
    pub secondary_run_compaction_bytes_read: u64,
    pub secondary_run_compaction_bytes_written: u64,
    pub secondary_checkpoint_bytes_written: u64,
    pub text_runs: usize,
    pub text_run_compactions: u64,
    pub text_run_compaction_time: Duration,
    pub text_run_compaction_bytes_read: u64,
    pub text_run_compaction_bytes_written: u64,
    pub text_checkpoint_bytes_written: u64,
    /// Frozen secondary/text/vector generations successfully serialized and
    /// published by the dedicated background worker.
    pub derived_publications: u64,
    pub derived_publication_time: Duration,
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

/// Immutable primary MVCC directory plus changes since its publication.
/// Canonical payloads remain in segments/WAL; this only pages the lookup
/// metadata that used to require one heap allocation per record/version.
struct PrimaryIdx {
    generation: u64,
    runs: Vec<PrimaryRun>,
    delta: HashMap<String, BTreeMap<String, Vec<VersionEntry>>>,
    /// Immutable resident generation currently being written by the
    /// background checkpoint thread. Readers merge it with the new active
    /// delta until canonical publication replaces it with an mmap run.
    frozen: Option<FrozenPrimary>,
}

type PrimaryDelta = HashMap<String, BTreeMap<String, Vec<VersionEntry>>>;

struct FrozenPrimary {
    version: u64,
    delta: Arc<PrimaryDelta>,
}

struct PrimaryRun {
    meta: PrimaryRunMeta,
    index: Arc<PagedIndex>,
}

impl PrimaryIdx {
    fn empty() -> Self {
        Self {
            generation: 0,
            runs: Vec::new(),
            delta: HashMap::new(),
            frozen: None,
        }
    }

    fn resident(delta: HashMap<String, BTreeMap<String, Vec<VersionEntry>>>) -> Self {
        Self {
            generation: 0,
            runs: Vec::new(),
            delta,
            frozen: None,
        }
    }

    #[cfg(test)]
    fn paged(base: PagedIndex) -> Self {
        let generation = base.dump_version();
        Self::paged_named(
            base,
            "primary.pidx".into(),
            PRIMARY_BASE_LEVEL,
            0,
            generation,
        )
    }

    fn paged_named(base: PagedIndex, file: String, level: u8, bytes: u64, generation: u64) -> Self {
        Self::paged_runs(
            generation,
            vec![PrimaryRun {
                meta: PrimaryRunMeta {
                    file,
                    level,
                    bytes,
                    generation,
                },
                index: Arc::new(base),
            }],
        )
    }

    fn paged_runs(generation: u64, runs: Vec<PrimaryRun>) -> Self {
        Self {
            generation,
            runs,
            delta: HashMap::new(),
            frozen: None,
        }
    }

    fn run_metas(&self) -> Vec<PrimaryRunMeta> {
        self.runs.iter().map(|run| run.meta.clone()).collect()
    }

    fn push(&mut self, table: &str, id: String, entry: VersionEntry) {
        if let Some(ids) = self.delta.get_mut(table) {
            ids.entry(id).or_default().push(entry);
        } else {
            let mut ids = BTreeMap::new();
            ids.insert(id, vec![entry]);
            self.delta.insert(table.to_owned(), ids);
        }
    }

    fn remove_delta_table(&mut self, table: &str) {
        self.delta.remove(table);
    }

    fn delta_memory_bytes(&self) -> usize {
        Self::map_memory_bytes(&self.delta)
    }

    fn map_memory_bytes(delta: &PrimaryDelta) -> usize {
        delta
            .iter()
            .map(|(table, ids)| {
                table.len()
                    + 96
                    + ids
                        .iter()
                        .map(|(id, versions)| {
                            id.len()
                                + 96
                                + versions
                                    .iter()
                                    .map(|entry| match &entry.kind {
                                        VKind::MemPut(payload) => payload.len() + 40,
                                        _ => 40,
                                    })
                                    .sum::<usize>()
                        })
                        .sum::<usize>()
            })
            .sum()
    }

    fn latest(&self, table: &str, id: &str) -> Result<Option<VersionEntry>> {
        self.newest_at_or_before(table, id, u64::MAX)
    }

    fn visible(&self, table: &str, id: &str, max_version: u64) -> Result<Option<VersionEntry>> {
        self.newest_at_or_before(table, id, max_version)
    }

    fn newest_at_or_before(
        &self,
        table: &str,
        id: &str,
        max_version: u64,
    ) -> Result<Option<VersionEntry>> {
        let key = primary_key(table, id);
        let mut newest: Option<VersionEntry> = None;
        for run in &self.runs {
            if !run.index.may_contain_key(&key) {
                continue;
            }
            run.index.visit_key(&key, |value| {
                let entry = decode_primary_entry(value)?;
                if entry.version <= max_version
                    && newest
                        .as_ref()
                        .is_none_or(|current| entry.version > current.version)
                {
                    newest = Some(entry);
                }
                Ok(true)
            })?;
        }
        if let Some(delta) = self
            .frozen
            .as_ref()
            .and_then(|frozen| frozen.delta.get(table))
            .and_then(|table| table.get(id))
        {
            for entry in delta {
                if entry.version <= max_version
                    && newest
                        .as_ref()
                        .is_none_or(|current| entry.version > current.version)
                {
                    newest = Some(entry.clone());
                }
            }
        }
        if let Some(delta) = self.delta.get(table).and_then(|table| table.get(id)) {
            for entry in delta {
                if entry.version <= max_version
                    && newest
                        .as_ref()
                        .is_none_or(|current| entry.version > current.version)
                {
                    newest = Some(entry.clone());
                }
            }
        }
        Ok(newest)
    }

    fn visit_table(
        &self,
        table: &str,
        after_id: Option<&str>,
        mut visit: impl FnMut(&str, &[VersionEntry]) -> Result<bool>,
    ) -> Result<()> {
        use std::ops::Bound::{Excluded, Unbounded};

        let prefix = primary_table_prefix(table);
        let after_key = after_id.map(|after| primary_key(table, after));
        let mut cursors: Vec<_> = self
            .runs
            .iter()
            .map(|run| {
                PrimaryTableCursor::new(
                    run.index.prefix_cursor_after(&prefix, after_key.as_deref()),
                    table,
                )
            })
            .collect();
        let mut heads: Vec<_> = cursors
            .iter_mut()
            .map(PrimaryTableCursor::next_group)
            .collect::<Result<_>>()?;
        let mut deltas = Vec::with_capacity(2);
        if let Some(ids) = self
            .frozen
            .as_ref()
            .and_then(|frozen| frozen.delta.get(table))
        {
            deltas.push(match after_id {
                Some(after) => ids.range::<str, _>((Excluded(after), Unbounded)),
                None => ids.range::<str, _>((Unbounded, Unbounded)),
            });
        }
        if let Some(ids) = self.delta.get(table) {
            deltas.push(match after_id {
                Some(after) => ids.range::<str, _>((Excluded(after), Unbounded)),
                None => ids.range::<str, _>((Unbounded, Unbounded)),
            });
        }
        let mut delta_heads: Vec<_> = deltas.iter_mut().map(Iterator::next).collect();

        loop {
            let run_id = heads
                .iter()
                .filter_map(|head| head.as_ref().map(|(id, _)| id.as_str()))
                .min();
            let delta_id = delta_heads
                .iter()
                .filter_map(|head| head.as_ref().map(|(id, _)| id.as_str()))
                .min();
            let next_id = match (run_id, delta_id) {
                (None, None) => break,
                (Some(id), None) | (None, Some(id)) => id.to_owned(),
                (Some(run), Some(delta)) => run.min(delta).to_owned(),
            };
            let mut versions = Vec::new();
            for (cursor, head) in cursors.iter_mut().zip(&mut heads) {
                if head
                    .as_ref()
                    .is_some_and(|(id, _)| id.as_str() == next_id.as_str())
                {
                    let (_, run_versions) = head.take().expect("matching run head");
                    versions.extend(run_versions);
                    *head = cursor.next_group()?;
                }
            }
            for (delta, head) in deltas.iter_mut().zip(&mut delta_heads) {
                if head
                    .as_ref()
                    .is_some_and(|(id, _)| id.as_str() == next_id.as_str())
                {
                    let (_, delta_versions) = head.take().expect("matching delta head");
                    versions.extend(delta_versions.iter().cloned());
                    *head = delta.next();
                }
            }
            if after_id.is_some_and(|after| next_id.as_str() <= after) {
                continue;
            }
            versions.sort_unstable_by_key(|entry| entry.version);
            versions.dedup_by_key(|entry| entry.version);
            if !visit(&next_id, &versions)? {
                break;
            }
        }
        Ok(())
    }

    #[cfg(test)]
    fn write_paged_for_catalog(
        &self,
        target: &Path,
        temp_dir: &Path,
        dump_version: u64,
        budget: usize,
        catalog: &Catalog,
    ) -> Result<()> {
        let mut writer = ExternalPagedWriter::new(target, temp_dir, dump_version, budget)?;
        for schema in &catalog.tables {
            self.visit_table(&schema.name, None, |id, versions| {
                let key = primary_key(&schema.name, id);
                for entry in versions.iter().filter(|entry| entry.version > schema.epoch) {
                    writer.add(&key, &encode_primary_entry(entry)?)?;
                }
                Ok(true)
            })?;
        }
        writer.finish()
    }
}

struct PrimaryTableCursor<'a> {
    cursor: PagedPrefixCursor<'a>,
    table: &'a str,
    pending: Option<(String, VersionEntry)>,
}

impl<'a> PrimaryTableCursor<'a> {
    fn new(cursor: PagedPrefixCursor<'a>, table: &'a str) -> Self {
        Self {
            cursor,
            table,
            pending: None,
        }
    }

    fn next_group(&mut self) -> Result<Option<(String, Vec<VersionEntry>)>> {
        let first = match self.pending.take() {
            Some(entry) => Some(entry),
            None => self.next_entry()?,
        };
        let Some((id, first)) = first else {
            return Ok(None);
        };
        let mut versions = vec![first];
        while let Some((next_id, entry)) = self.next_entry()? {
            if next_id != id {
                self.pending = Some((next_id, entry));
                break;
            }
            versions.push(entry);
        }
        Ok(Some((id, versions)))
    }

    fn next_entry(&mut self) -> Result<Option<(String, VersionEntry)>> {
        let Some((key, value)) = self.cursor.next()? else {
            return Ok(None);
        };
        let (table, id) = decode_primary_key(key)?;
        if table != self.table {
            return Err(Error::Corrupt(
                "primary index: table prefix mismatch".into(),
            ));
        }
        Ok(Some((id.to_owned(), decode_primary_entry(value)?)))
    }
}

fn primary_table_prefix(table: &str) -> Vec<u8> {
    let mut key = Vec::with_capacity(4 + table.len());
    key.extend_from_slice(&(table.len() as u32).to_be_bytes());
    key.extend_from_slice(table.as_bytes());
    key
}

fn primary_key(table: &str, id: &str) -> Vec<u8> {
    let mut key = Vec::with_capacity(4 + table.len() + id.len());
    encode_primary_key_into(table, id, &mut key);
    key
}

fn encode_primary_key_into(table: &str, id: &str, key: &mut Vec<u8>) {
    key.clear();
    key.extend_from_slice(&(table.len() as u32).to_be_bytes());
    key.extend_from_slice(table.as_bytes());
    key.extend_from_slice(id.as_bytes());
}

fn decode_primary_key(key: &[u8]) -> Result<(&str, &str)> {
    if key.len() < 4 {
        return Err(Error::Corrupt("primary index: truncated key".into()));
    }
    let table_len = u32::from_be_bytes(key[..4].try_into().expect("four bytes")) as usize;
    let table_end = 4usize
        .checked_add(table_len)
        .filter(|end| *end <= key.len())
        .ok_or_else(|| Error::Corrupt("primary index: invalid table length".into()))?;
    let table = std::str::from_utf8(&key[4..table_end])
        .map_err(|_| Error::Corrupt("primary index: invalid table utf8".into()))?;
    let id = std::str::from_utf8(&key[table_end..])
        .map_err(|_| Error::Corrupt("primary index: invalid id utf8".into()))?;
    Ok((table, id))
}

fn encode_primary_entry(entry: &VersionEntry) -> Result<Vec<u8>> {
    let mut value = Vec::with_capacity(25);
    encode_primary_entry_into(entry, &mut value)?;
    Ok(value)
}

fn encode_primary_entry_into(entry: &VersionEntry, value: &mut Vec<u8>) -> Result<()> {
    value.clear();
    value.reserve(25);
    value.extend_from_slice(&entry.version.to_be_bytes());
    match entry.kind {
        VKind::SegPut {
            segment,
            payload_offset,
            payload_len,
        } => {
            value.push(0);
            value.extend_from_slice(&segment.to_le_bytes());
            value.extend_from_slice(&payload_offset.to_le_bytes());
            value.extend_from_slice(&payload_len.to_le_bytes());
        }
        VKind::SegTombstone => value.push(1),
        VKind::MemPut(_) | VKind::MemTombstone => {
            return Err(Error::InvalidArgument(
                "primary index base cannot contain memtable entries".into(),
            ))
        }
    }
    Ok(())
}

fn decode_primary_entry(value: &[u8]) -> Result<VersionEntry> {
    if value.len() < 9 {
        return Err(Error::Corrupt("primary index: truncated entry".into()));
    }
    let version = u64::from_be_bytes(value[..8].try_into().expect("eight bytes"));
    let kind = match value[8] {
        0 if value.len() == 25 => VKind::SegPut {
            segment: u32::from_le_bytes(value[9..13].try_into().expect("four bytes")),
            payload_offset: u64::from_le_bytes(value[13..21].try_into().expect("eight bytes")),
            payload_len: u32::from_le_bytes(value[21..25].try_into().expect("four bytes")),
        },
        1 if value.len() == 9 => VKind::SegTombstone,
        _ => return Err(Error::Corrupt("primary index: invalid entry".into())),
    };
    Ok(VersionEntry { version, kind })
}

fn primary_index_path(dir: &Path) -> PathBuf {
    dir.join(INDEXES_DIR).join("primary.pidx")
}

fn load_primary_runs(dir: &Path, generation: u64) -> Result<PrimaryIdx> {
    let indexes_dir = dir.join(INDEXES_DIR);
    let manifest = PrimaryRunManifest::load(&indexes_dir, generation)?;
    let mut seen = HashSet::new();
    let mut runs = Vec::with_capacity(manifest.runs.len());
    for meta in manifest.runs {
        if !seen.insert(meta.file.clone()) {
            return Err(Error::Corrupt(
                "primary run manifest: duplicate filename".into(),
            ));
        }
        let path = indexes_dir.join(&meta.file);
        if fs::metadata(&path)?.len() != meta.bytes {
            return Err(Error::Corrupt(format!(
                "primary run {} has the wrong length",
                meta.file
            )));
        }
        let index = PagedIndex::open(&path)?;
        if index.dump_version() != meta.generation {
            return Err(Error::Corrupt(format!(
                "primary run {} has the wrong generation",
                meta.file
            )));
        }
        runs.push(PrimaryRun {
            index: Arc::new(index),
            meta,
        });
    }
    Ok(PrimaryIdx::paged_runs(manifest.generation, runs))
}

fn publish_primary_run_manifest(dir: &Path, generation: u64, index: &PrimaryIdx) -> Result<()> {
    PrimaryRunManifest::new(generation, index.run_metas()).publish(&dir.join(INDEXES_DIR))
}

fn cleanup_primary_run_orphans(dir: &Path) {
    let indexes_dir = dir.join(INDEXES_DIR);
    let keep: HashSet<_> = PrimaryRunManifest::referenced_files(&indexes_dir)
        .into_iter()
        .collect();
    let Ok(entries) = fs::read_dir(&indexes_dir) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        let Some(name) = path.file_name().and_then(|name| name.to_str()) else {
            continue;
        };
        let is_run = name.starts_with("primary-L") && name.ends_with(".pidx.run");
        let is_partial = name.starts_with("primary-L") && name.ends_with(".run.tmp");
        if (is_run && !keep.contains(name)) || is_partial {
            let _ = fs::remove_file(path);
        }
    }
}

fn primary_compaction_level(index: &PrimaryIdx) -> Option<u8> {
    let mut counts = BTreeMap::<u8, usize>::new();
    for run in &index.runs {
        if run.meta.level != PRIMARY_BASE_LEVEL {
            *counts.entry(run.meta.level).or_default() += 1;
        }
    }
    counts
        .into_iter()
        .find_map(|(level, count)| (count >= PRIMARY_LEVEL_FANOUT).then_some(level))
}

fn primary_compaction_needed(shared: &Shared) -> bool {
    !shared.opts.read_only
        && primary_compaction_level(&shared.state.read().unwrap().index).is_some()
}

fn maybe_schedule_primary_compaction(shared: &Shared) {
    if !primary_compaction_needed(shared)
        || shared
            .primary_compaction_scheduled
            .compare_exchange(false, true, AtomicOrdering::AcqRel, AtomicOrdering::Acquire)
            .is_err()
    {
        return;
    }
    let sent = shared
        .maintenance_tx
        .lock()
        .unwrap()
        .as_ref()
        .is_some_and(|sender| sender.send(MaintenanceJob::CompactPrimaryRuns).is_ok());
    if !sent {
        shared
            .primary_compaction_scheduled
            .store(false, AtomicOrdering::Release);
    }
}

/// Merge one size tier without holding the commit or state locks while files
/// are scanned. Publication rechecks the selected immutable inputs: a
/// concurrent checkpoint may append another L0 safely, while canonical data
/// compaction replaces the selected set and makes this output disposable.
fn compact_one_primary_level(shared: &Arc<Shared>) -> Result<()> {
    let started = Instant::now();
    let (generation, level, selected) = {
        let state = shared.state.read().unwrap();
        let Some(level) = primary_compaction_level(&state.index) else {
            return Ok(());
        };
        let selected = state
            .index
            .runs
            .iter()
            .filter(|run| run.meta.level == level)
            .take(PRIMARY_LEVEL_FANOUT)
            .map(|run| (run.meta.clone(), run.index.clone()))
            .collect::<Vec<_>>();
        (state.index.generation, level, selected)
    };
    let indexes_dir = shared.dir.join(INDEXES_DIR);
    let next_level = level.saturating_add(1).min(PRIMARY_BASE_LEVEL - 1);
    let file = format!("primary-L{next_level}-{}.pidx.run", Ulid::new());
    let path = indexes_dir.join(&file);
    // Keep the merge under a name that runtime orphan cleanup deliberately
    // ignores. It becomes a normal run only after acquiring the publication
    // mutex, so a concurrent checkpoint cannot delete an in-flight output.
    let pending = path.with_file_name(format!("{file}.primary-pending"));
    let tmp = path.with_file_name(format!("{file}.primary-pending.tmp"));
    let inputs: Vec<&PagedIndex> = selected.iter().map(|(_, index)| index.as_ref()).collect();
    if let Err(error) = merge_paged_indexes(&tmp, &inputs, generation) {
        let _ = fs::remove_file(&tmp);
        return Err(error);
    }
    fs::rename(&tmp, &pending)?;
    fsync_dir(&indexes_dir)?;
    let output_bytes = fs::metadata(&pending)?.len();
    let output_meta = PrimaryRunMeta {
        file,
        level: next_level,
        bytes: output_bytes,
        generation,
    };
    let directory_estimate = usize::try_from(output_bytes / 32)
        .unwrap_or(usize::MAX)
        .clamp(4096, shared.opts.memory.maintenance_pool_bytes);
    let maintenance_memory = shared
        .memory_governor
        .acquire(MemoryPool::Maintenance, directory_estimate);
    let output_index = Arc::new(PagedIndex::open(&pending)?);

    // Commit paths acquire the commit mutex before maintenance admission.
    // Release the working-set permit before publication to preserve that
    // global lock order; publication itself allocates no database-sized set.
    drop(maintenance_memory);
    let _publication = shared.primary_manifest_publication.lock().unwrap();
    fs::rename(&pending, &path)?;
    fsync_dir(&indexes_dir)?;
    let selected_names: HashSet<_> = selected
        .iter()
        .map(|(meta, _)| meta.file.as_str())
        .collect();
    let (expected_generation, metas) = {
        let _commit = shared.commit.lock();
        let state = shared.state.read().unwrap();
        // A checkpoint may append another immutable L0 while this merge runs.
        // Those selected inputs remain valid. A canonical segment rewrite, on
        // the other hand, replaces them and makes this predicate fail.
        let still_current = selected
            .iter()
            .all(|(meta, _)| state.index.runs.iter().any(|run| run.meta == *meta));
        if !still_current {
            drop(state);
            let _ = fs::remove_file(&path);
            return Ok(());
        }
        let mut metas: Vec<_> = state
            .index
            .runs
            .iter()
            .filter(|run| !selected_names.contains(run.meta.file.as_str()))
            .map(|run| run.meta.clone())
            .collect();
        metas.push(output_meta.clone());
        (state.index.generation, metas)
    };
    PrimaryRunManifest::new(expected_generation, metas)
        .prepare(&indexes_dir)?
        .publish()?;

    let _commit = shared.commit.lock();
    let mut state = shared.state.write().unwrap();
    let still_current = state.index.generation == expected_generation
        && selected
            .iter()
            .all(|(meta, _)| state.index.runs.iter().any(|run| run.meta == *meta));
    if !still_current {
        // A canonical rewrite does not depend on this disposable manifest and
        // may race the unlocked rename. Restore the run set matching its new
        // generation before deleting our unused output.
        PrimaryRunManifest::new(state.index.generation, state.index.run_metas())
            .publish(&indexes_dir)?;
        drop(state);
        let _ = fs::remove_file(&path);
        return Ok(());
    }
    state
        .index
        .runs
        .retain(|run| !selected_names.contains(run.meta.file.as_str()));
    state.index.runs.push(PrimaryRun {
        meta: output_meta,
        index: output_index,
    });
    drop(state);
    shared
        .primary_run_compaction_count
        .fetch_add(1, AtomicOrdering::Relaxed);
    shared.primary_run_compaction_nanos.fetch_add(
        started.elapsed().as_nanos().min(u64::MAX as u128) as u64,
        AtomicOrdering::Relaxed,
    );
    shared.primary_run_compaction_bytes_read.fetch_add(
        selected.iter().map(|(meta, _)| meta.bytes).sum(),
        AtomicOrdering::Relaxed,
    );
    shared
        .primary_run_compaction_bytes_written
        .fetch_add(output_bytes, AtomicOrdering::Relaxed);
    cleanup_primary_run_orphans(&shared.dir);
    Ok(())
}

fn secondary_compaction_target(state: &State) -> Option<((String, String), u8)> {
    state.secondary.iter().find_map(|(key, index)| {
        let mut counts = BTreeMap::<u8, usize>::new();
        for run in &index.runs {
            if run.meta.level != DERIVED_BASE_LEVEL {
                *counts.entry(run.meta.level).or_default() += 1;
            }
        }
        counts.into_iter().find_map(|(level, count)| {
            (count >= DERIVED_LEVEL_FANOUT).then(|| (key.clone(), level))
        })
    })
}

fn secondary_compaction_needed(shared: &Shared) -> bool {
    !shared.opts.read_only && secondary_compaction_target(&shared.state.read().unwrap()).is_some()
}

fn maybe_schedule_secondary_compaction(shared: &Shared) {
    if !secondary_compaction_needed(shared)
        || shared
            .secondary_compaction_scheduled
            .compare_exchange(false, true, AtomicOrdering::AcqRel, AtomicOrdering::Acquire)
            .is_err()
    {
        return;
    }
    let sent = shared
        .maintenance_tx
        .lock()
        .unwrap()
        .as_ref()
        .is_some_and(|sender| sender.send(MaintenanceJob::CompactSecondaryRuns).is_ok());
    if !sent {
        shared
            .secondary_compaction_scheduled
            .store(false, AtomicOrdering::Release);
    }
}

/// Compact one equality-index level. Operation versions are embedded in each
/// value, so the streaming merge can retain exactly the newest add/tombstone
/// for every `(indexed value, id)` pair regardless of overlapping levels.
fn compact_one_secondary_level(shared: &Arc<Shared>) -> Result<()> {
    let started = Instant::now();
    let (key, generation, level, selected) = {
        let state = shared.state.read().unwrap();
        let Some((key, level)) = secondary_compaction_target(&state) else {
            return Ok(());
        };
        let index = state
            .secondary
            .get(&key)
            .expect("secondary compaction target exists");
        let selected = index
            .runs
            .iter()
            .filter(|run| run.meta.level == level)
            .take(DERIVED_LEVEL_FANOUT)
            .map(|run| (run.meta.clone(), run.index.clone()))
            .collect::<Vec<_>>();
        (key, index.generation, level, selected)
    };
    let indexes_dir = shared.dir.join(INDEXES_DIR);
    let next_level = level.saturating_add(1).min(DERIVED_BASE_LEVEL - 1);
    let file = sidx_run_filename(&shared.dir, &key.0, &key.1, next_level);
    let path = indexes_dir.join(&file);
    let tmp = path.with_file_name(format!("{file}.tmp"));
    let inputs: Vec<&PagedIndex> = selected.iter().map(|(_, index)| index.as_ref()).collect();
    if let Err(error) = merge_paged_indexes_latest_value(&tmp, &inputs, generation) {
        let _ = fs::remove_file(&tmp);
        return Err(error);
    }
    fs::rename(&tmp, &path)?;
    fsync_dir(&indexes_dir)?;
    let output_bytes = fs::metadata(&path)?.len();
    let output_meta = DerivedRunMeta {
        file,
        level: next_level,
        bytes: output_bytes,
        generation,
    };
    let output_index = Arc::new(PagedIndex::open(&path)?);
    validate_secondary_run(&output_index)?;
    let _publication = shared.derived_manifest_publication.lock().unwrap();

    let selected_names: HashSet<_> = selected
        .iter()
        .map(|(meta, _)| meta.file.as_str())
        .collect();
    let manifest = {
        let _commit = shared.commit.lock();
        let state = shared.state.read().unwrap();
        let Some(index) = state.secondary.get(&key) else {
            drop(state);
            let _ = fs::remove_file(&path);
            return Ok(());
        };
        let still_current = index.generation == generation
            && selected
                .iter()
                .all(|(meta, _)| index.runs.iter().any(|run| run.meta == *meta));
        if !still_current {
            drop(state);
            let _ = fs::remove_file(&path);
            return Ok(());
        }
        let mut metas: Vec<_> = index
            .runs
            .iter()
            .filter(|run| !selected_names.contains(run.meta.file.as_str()))
            .map(|run| run.meta.clone())
            .collect();
        metas.push(output_meta.clone());
        DerivedRunManifest::new(
            DerivedRunKind::Secondary,
            &key.0,
            &key.1,
            generation,
            metas,
            [0, 0],
        )
    };
    let manifest_outcome = manifest
        .prepare(&sidx_manifest_path(&shared.dir, &key.0, &key.1))?
        .publish()?;
    if let PublishOutcome::SyncFailed(error) = manifest_outcome {
        return Err(error.into());
    }

    let _commit = shared.commit.lock();
    let mut state = shared.state.write().unwrap();
    let Some(index) = state.secondary.get_mut(&key) else {
        drop(state);
        let _ = fs::remove_file(&path);
        return Ok(());
    };
    let still_current = index.generation == generation
        && selected
            .iter()
            .all(|(meta, _)| index.runs.iter().any(|run| run.meta == *meta));
    if !still_current {
        publish_secondary_manifest(&shared.dir, &key.0, &key.1, index.generation, index)?;
        drop(state);
        let _ = fs::remove_file(&path);
        return Ok(());
    }
    index
        .runs
        .retain(|run| !selected_names.contains(run.meta.file.as_str()));
    index.runs.push(SecRun {
        meta: output_meta,
        index: output_index,
    });
    let catalog = state.catalog.clone();
    drop(state);

    shared
        .secondary_run_compaction_count
        .fetch_add(1, AtomicOrdering::Relaxed);
    shared.secondary_run_compaction_nanos.fetch_add(
        started.elapsed().as_nanos().min(u64::MAX as u128) as u64,
        AtomicOrdering::Relaxed,
    );
    shared.secondary_run_compaction_bytes_read.fetch_add(
        selected.iter().map(|(meta, _)| meta.bytes).sum(),
        AtomicOrdering::Relaxed,
    );
    shared
        .secondary_run_compaction_bytes_written
        .fetch_add(output_bytes, AtomicOrdering::Relaxed);
    if !shared.background_derived.lock().unwrap().running {
        cleanup_orphan_sidx(&shared.dir, &catalog);
    }
    Ok(())
}

fn text_compaction_target(state: &State) -> Option<((String, String), u8)> {
    state.text.iter().find_map(|(key, index)| {
        let mut counts = BTreeMap::<u8, usize>::new();
        for run in &index.runs {
            if run.meta.level != DERIVED_BASE_LEVEL {
                *counts.entry(run.meta.level).or_default() += 1;
            }
        }
        counts.into_iter().find_map(|(level, count)| {
            (count >= DERIVED_LEVEL_FANOUT).then(|| (key.clone(), level))
        })
    })
}

fn text_compaction_needed(shared: &Shared) -> bool {
    !shared.opts.read_only && text_compaction_target(&shared.state.read().unwrap()).is_some()
}

fn maybe_schedule_text_compaction(shared: &Shared) {
    if !text_compaction_needed(shared)
        || shared
            .text_compaction_scheduled
            .compare_exchange(false, true, AtomicOrdering::AcqRel, AtomicOrdering::Acquire)
            .is_err()
    {
        return;
    }
    let sent = shared
        .maintenance_tx
        .lock()
        .unwrap()
        .as_ref()
        .is_some_and(|sender| sender.send(MaintenanceJob::CompactTextRuns).is_ok());
    if !sent {
        shared
            .text_compaction_scheduled
            .store(false, AtomicOrdering::Release);
    }
}

fn compact_one_text_level(shared: &Arc<Shared>) -> Result<()> {
    let started = Instant::now();
    let (key, generation, level, selected) = {
        let state = shared.state.read().unwrap();
        let Some((key, level)) = text_compaction_target(&state) else {
            return Ok(());
        };
        let index = state.text.get(&key).expect("text compaction target exists");
        let selected = index
            .runs
            .iter()
            .filter(|run| run.meta.level == level)
            .take(DERIVED_LEVEL_FANOUT)
            .map(|run| (run.meta.clone(), run.index.clone()))
            .collect::<Vec<_>>();
        (key, index.generation, level, selected)
    };
    let indexes_dir = shared.dir.join(INDEXES_DIR);
    let next_level = level.saturating_add(1).min(DERIVED_BASE_LEVEL - 1);
    let file = tidx_run_filename(&shared.dir, &key.0, &key.1, next_level);
    let path = indexes_dir.join(&file);
    let tmp = path.with_file_name(format!("{file}.tmp"));
    let inputs: Vec<&PagedIndex> = selected.iter().map(|(_, index)| index.as_ref()).collect();
    if let Err(error) = merge_paged_indexes_latest_value(&tmp, &inputs, generation) {
        let _ = fs::remove_file(&tmp);
        return Err(error);
    }
    fs::rename(&tmp, &path)?;
    fsync_dir(&indexes_dir)?;
    let output_bytes = fs::metadata(&path)?.len();
    let output_meta = DerivedRunMeta {
        file,
        level: next_level,
        bytes: output_bytes,
        generation,
    };
    let output_index = Arc::new(PagedIndex::open(&path)?);
    validate_text_run(&output_index)?;
    let _publication = shared.derived_manifest_publication.lock().unwrap();

    let selected_names: HashSet<_> = selected
        .iter()
        .map(|(meta, _)| meta.file.as_str())
        .collect();
    let manifest = {
        let _commit = shared.commit.lock();
        let state = shared.state.read().unwrap();
        let Some(index) = state.text.get(&key) else {
            drop(state);
            let _ = fs::remove_file(&path);
            return Ok(());
        };
        let still_current = index.generation == generation
            && selected
                .iter()
                .all(|(meta, _)| index.runs.iter().any(|run| run.meta == *meta));
        if !still_current {
            drop(state);
            let _ = fs::remove_file(&path);
            return Ok(());
        }
        let mut metas: Vec<_> = index
            .runs
            .iter()
            .filter(|run| !selected_names.contains(run.meta.file.as_str()))
            .map(|run| run.meta.clone())
            .collect();
        metas.push(output_meta.clone());
        let (doc_count, total_len) = index.doc_stats();
        DerivedRunManifest::new(
            DerivedRunKind::Text,
            &key.0,
            &key.1,
            generation,
            metas,
            [doc_count, total_len],
        )
    };
    let manifest_outcome = manifest
        .prepare(&tidx_manifest_path(&shared.dir, &key.0, &key.1))?
        .publish()?;
    if let PublishOutcome::SyncFailed(error) = manifest_outcome {
        return Err(error.into());
    }

    let _commit = shared.commit.lock();
    let mut state = shared.state.write().unwrap();
    let Some(index) = state.text.get_mut(&key) else {
        drop(state);
        let _ = fs::remove_file(&path);
        return Ok(());
    };
    let still_current = index.generation == generation
        && selected
            .iter()
            .all(|(meta, _)| index.runs.iter().any(|run| run.meta == *meta));
    if !still_current {
        publish_text_manifest(&shared.dir, &key.0, &key.1, index.generation, index)?;
        drop(state);
        let _ = fs::remove_file(&path);
        return Ok(());
    }
    index
        .runs
        .retain(|run| !selected_names.contains(run.meta.file.as_str()));
    index.runs.push(TextRun {
        meta: output_meta,
        index: output_index,
    });
    let catalog = state.catalog.clone();
    drop(state);

    shared
        .text_run_compaction_count
        .fetch_add(1, AtomicOrdering::Relaxed);
    shared.text_run_compaction_nanos.fetch_add(
        started.elapsed().as_nanos().min(u64::MAX as u128) as u64,
        AtomicOrdering::Relaxed,
    );
    shared.text_run_compaction_bytes_read.fetch_add(
        selected.iter().map(|(meta, _)| meta.bytes).sum(),
        AtomicOrdering::Relaxed,
    );
    shared
        .text_run_compaction_bytes_written
        .fetch_add(output_bytes, AtomicOrdering::Relaxed);
    if !shared.background_derived.lock().unwrap().running {
        cleanup_orphan_tidx(&shared.dir, &catalog);
    }
    Ok(())
}

fn primary_generation(committed_version: u64, segments: &[SegmentMeta], catalog: &Catalog) -> u64 {
    let mut hash = 0xcbf2_9ce4_8422_2325u64;
    let mut mix = |byte: u8| {
        hash ^= byte as u64;
        hash = hash.wrapping_mul(0x100_0000_01b3);
    };
    for byte in committed_version.to_le_bytes() {
        mix(byte);
    }
    for segment in segments {
        for byte in segment
            .id
            .to_le_bytes()
            .into_iter()
            .chain(segment.len.to_le_bytes())
        {
            mix(byte);
        }
    }
    for table in &catalog.tables {
        for byte in (table.name.len() as u64)
            .to_le_bytes()
            .into_iter()
            .chain(table.name.bytes())
            .chain(table.epoch.to_le_bytes())
        {
            mix(byte);
        }
    }
    hash
}

const SECONDARY_FORMAT_KEY: &[u8] = &[0];
const SECONDARY_FORMAT_VALUE: &[u8] = b"ESQLSID2";
const SECONDARY_ENTRY_TAG: u8 = 1;
const SECONDARY_DELETE: u8 = 0;
const SECONDARY_ADD: u8 = 1;

struct SecRun {
    meta: DerivedRunMeta,
    index: Arc<PagedIndex>,
}

struct SecIdx {
    generation: u64,
    runs: Vec<SecRun>,
    /// Final additions since the last immutable run was published.
    delta: HashMap<Vec<u8>, BTreeSet<String>>,
    /// Final removals since publication. Tombstones are required even when
    /// the matching add lives in a non-base level.
    removed: HashMap<Vec<u8>, BTreeSet<String>>,
    /// Immutable in-memory overlay being written by background maintenance.
    /// New commits land in `delta`/`removed` and therefore never mutate the
    /// generation owned by the worker.
    frozen: Option<Arc<FrozenSecDelta>>,
}

struct FrozenSecDelta {
    generation: u64,
    delta: HashMap<Vec<u8>, BTreeSet<String>>,
    removed: HashMap<Vec<u8>, BTreeSet<String>>,
}

struct SecPairCursor<'a> {
    cursor: PagedPrefixCursor<'a>,
    prefix: Vec<u8>,
    head: Option<(String, u64, u8)>,
}

impl<'a> SecPairCursor<'a> {
    fn new(index: &'a PagedIndex, key: &[u8], after: Option<&str>) -> Result<Self> {
        let prefix = secondary_pair_prefix(key);
        let after_key = after.map(|after| secondary_pair_key(key, after));
        let mut cursor = Self {
            cursor: index.prefix_cursor_after(&prefix, after_key.as_deref()),
            prefix,
            head: None,
        };
        cursor.advance()?;
        Ok(cursor)
    }

    fn advance(&mut self) -> Result<()> {
        self.head = None;
        let Some((key, value)) = self.cursor.next()? else {
            return Ok(());
        };
        let id = key
            .strip_prefix(self.prefix.as_slice())
            .ok_or_else(|| Error::Corrupt("secondary index: invalid pair prefix".into()))?;
        let id = std::str::from_utf8(id)
            .map_err(|_| Error::Corrupt("secondary index: invalid id utf8".into()))?;
        let (version, operation) = decode_secondary_operation(value)?;
        self.head = Some((id.to_owned(), version, operation));
        Ok(())
    }
}

impl SecIdx {
    fn resident(map: HashMap<Vec<u8>, BTreeSet<String>>) -> Self {
        Self {
            generation: 0,
            runs: Vec::new(),
            delta: map,
            removed: HashMap::new(),
            frozen: None,
        }
    }

    fn paged_runs(generation: u64, runs: Vec<SecRun>) -> Result<Self> {
        for run in &runs {
            validate_secondary_run(&run.index)?;
        }
        Ok(Self {
            generation,
            runs,
            delta: HashMap::new(),
            removed: HashMap::new(),
            frozen: None,
        })
    }

    fn run_metas(&self) -> Vec<DerivedRunMeta> {
        self.runs.iter().map(|run| run.meta.clone()).collect()
    }

    fn ids(&self, key: &[u8]) -> Result<BTreeSet<String>> {
        Ok(self.ids_batch(key, None, usize::MAX)?.into_iter().collect())
    }

    /// Merge one cursor per immutable run plus the bounded mutable overlay.
    /// Versioned tombstones make the result independent of level order.
    fn ids_batch(&self, key: &[u8], after: Option<&str>, limit: usize) -> Result<Vec<String>> {
        use std::ops::Bound::{Excluded, Unbounded};

        if limit == 0 {
            return Ok(Vec::new());
        }
        let mut cursors = self
            .runs
            .iter()
            .filter(|run| run.index.may_contain_prefix(&secondary_pair_prefix(key)))
            .map(|run| SecPairCursor::new(&run.index, key, after))
            .collect::<Result<Vec<_>>>()?;
        let mut added = self.delta.get(key).map(|ids| match after {
            Some(after) => ids.range::<str, _>((Excluded(after), Unbounded)).peekable(),
            None => ids.range::<str, _>((Unbounded, Unbounded)).peekable(),
        });
        let mut removed = self.removed.get(key).map(|ids| match after {
            Some(after) => ids.range::<str, _>((Excluded(after), Unbounded)).peekable(),
            None => ids.range::<str, _>((Unbounded, Unbounded)).peekable(),
        });
        let mut frozen_added = self.frozen.as_ref().and_then(|frozen| {
            frozen.delta.get(key).map(|ids| match after {
                Some(after) => ids.range::<str, _>((Excluded(after), Unbounded)).peekable(),
                None => ids.range::<str, _>((Unbounded, Unbounded)).peekable(),
            })
        });
        let mut frozen_removed = self.frozen.as_ref().and_then(|frozen| {
            frozen.removed.get(key).map(|ids| match after {
                Some(after) => ids.range::<str, _>((Excluded(after), Unbounded)).peekable(),
                None => ids.range::<str, _>((Unbounded, Unbounded)).peekable(),
            })
        });
        let mut out = Vec::with_capacity(limit.min(1024));
        loop {
            let next_persisted = cursors
                .iter()
                .filter_map(|cursor| cursor.head.as_ref().map(|head| head.0.as_str()))
                .min();
            let next_added = added
                .as_mut()
                .and_then(|iter| iter.peek().map(|id| id.as_str()));
            let next_removed = removed
                .as_mut()
                .and_then(|iter| iter.peek().map(|id| id.as_str()));
            let next_frozen_added = frozen_added
                .as_mut()
                .and_then(|iter| iter.peek().map(|id| id.as_str()));
            let next_frozen_removed = frozen_removed
                .as_mut()
                .and_then(|iter| iter.peek().map(|id| id.as_str()));
            let Some(id) = next_persisted
                .into_iter()
                .chain(next_frozen_added)
                .chain(next_frozen_removed)
                .chain(next_added)
                .chain(next_removed)
                .min()
                .map(str::to_owned)
            else {
                break;
            };
            let mut newest: Option<(u64, u8)> = None;
            for cursor in &mut cursors {
                while cursor.head.as_ref().is_some_and(|head| head.0 == id) {
                    let (_, version, operation) =
                        cursor.head.take().expect("matching secondary head");
                    if newest.is_none_or(|current| (version, operation) > current) {
                        newest = Some((version, operation));
                    }
                    cursor.advance()?;
                }
            }
            let frozen_generation = self.frozen.as_ref().map(|frozen| frozen.generation);
            if frozen_added.as_mut().is_some_and(|iter| {
                iter.peek()
                    .is_some_and(|candidate| candidate.as_str() == id)
            }) {
                frozen_added.as_mut().expect("checked above").next();
                let operation = (
                    frozen_generation.expect("frozen iterator has generation"),
                    SECONDARY_ADD,
                );
                if newest.is_none_or(|current| operation.0 > current.0) {
                    newest = Some(operation);
                }
            }
            if frozen_removed.as_mut().is_some_and(|iter| {
                iter.peek()
                    .is_some_and(|candidate| candidate.as_str() == id)
            }) {
                frozen_removed.as_mut().expect("checked above").next();
                let generation = frozen_generation.expect("frozen iterator has generation");
                if newest.is_none_or(|current| generation >= current.0) {
                    newest = Some((generation, SECONDARY_DELETE));
                }
            }
            if added.as_mut().is_some_and(|iter| {
                iter.peek()
                    .is_some_and(|candidate| candidate.as_str() == id)
            }) {
                added.as_mut().expect("checked above").next();
                newest = Some((u64::MAX, SECONDARY_ADD));
            }
            if removed.as_mut().is_some_and(|iter| {
                iter.peek()
                    .is_some_and(|candidate| candidate.as_str() == id)
            }) {
                removed.as_mut().expect("checked above").next();
                newest = Some((u64::MAX, SECONDARY_DELETE));
            }
            if newest.is_some_and(|(_, operation)| operation == SECONDARY_ADD) {
                out.push(id);
                if out.len() == limit {
                    break;
                }
            }
        }
        Ok(out)
    }

    fn add(&mut self, key: Vec<u8>, id: &str) {
        if let Some(removed) = self.removed.get_mut(&key) {
            removed.remove(id);
            if removed.is_empty() {
                self.removed.remove(&key);
            }
        }
        self.delta.entry(key).or_default().insert(id.to_owned());
    }

    fn remove(&mut self, key: &[u8], id: &str) {
        if let Some(delta) = self.delta.get_mut(key) {
            delta.remove(id);
            if delta.is_empty() {
                self.delta.remove(key);
            }
        }
        if !self.runs.is_empty() || self.frozen.is_some() {
            self.removed
                .entry(key.to_vec())
                .or_default()
                .insert(id.to_owned());
        }
    }

    fn delta_memory_bytes(&self) -> usize {
        fn map_bytes(map: &HashMap<Vec<u8>, BTreeSet<String>>) -> usize {
            map.iter()
                .map(|(key, ids)| {
                    key.len() + 96 + ids.iter().map(|id| id.len() + 48).sum::<usize>()
                })
                .sum()
        }
        map_bytes(&self.delta) + map_bytes(&self.removed)
    }

    fn frozen_delta_memory_bytes(&self) -> usize {
        fn map_bytes(map: &HashMap<Vec<u8>, BTreeSet<String>>) -> usize {
            map.iter()
                .map(|(key, ids)| {
                    key.len() + 96 + ids.iter().map(|id| id.len() + 48).sum::<usize>()
                })
                .sum()
        }
        self.frozen.as_ref().map_or(0, |frozen| {
            map_bytes(&frozen.delta).saturating_add(map_bytes(&frozen.removed))
        })
    }

    fn freeze_delta(&mut self, generation: u64) -> Option<Arc<FrozenSecDelta>> {
        if self.frozen.is_some() || (self.delta.is_empty() && self.removed.is_empty()) {
            return None;
        }
        let frozen = Arc::new(FrozenSecDelta {
            generation,
            delta: std::mem::take(&mut self.delta),
            removed: std::mem::take(&mut self.removed),
        });
        self.frozen = Some(frozen.clone());
        Some(frozen)
    }

    fn frozen_matches(&self, frozen: &Arc<FrozenSecDelta>) -> bool {
        self.frozen
            .as_ref()
            .is_some_and(|current| Arc::ptr_eq(current, frozen))
    }

    fn clear_frozen(&mut self, frozen: &Arc<FrozenSecDelta>) {
        if self.frozen_matches(frozen) {
            self.frozen = None;
        }
    }
}

fn secondary_pair_prefix(key: &[u8]) -> Vec<u8> {
    let mut pair = Vec::with_capacity(5 + key.len());
    pair.push(SECONDARY_ENTRY_TAG);
    pair.extend_from_slice(&(key.len() as u32).to_be_bytes());
    pair.extend_from_slice(key);
    pair
}

fn secondary_pair_key(key: &[u8], id: &str) -> Vec<u8> {
    let mut pair = secondary_pair_prefix(key);
    pair.extend_from_slice(id.as_bytes());
    pair
}

fn secondary_pair_parts(pair: &[u8]) -> Result<(&[u8], &str)> {
    if pair.first() != Some(&SECONDARY_ENTRY_TAG) || pair.len() < 5 {
        return Err(Error::Corrupt("secondary index: invalid pair key".into()));
    }
    let key_len = u32::from_be_bytes(pair[1..5].try_into().expect("four bytes")) as usize;
    let key_end = 5usize
        .checked_add(key_len)
        .filter(|end| *end <= pair.len())
        .ok_or_else(|| Error::Corrupt("secondary index: truncated pair key".into()))?;
    let id = std::str::from_utf8(&pair[key_end..])
        .map_err(|_| Error::Corrupt("secondary index: invalid id utf8".into()))?;
    Ok((&pair[5..key_end], id))
}

fn secondary_operation(version: u64, operation: u8) -> [u8; 9] {
    let mut value = [0; 9];
    value[..8].copy_from_slice(&version.to_be_bytes());
    value[8] = operation;
    value
}

fn decode_secondary_operation(value: &[u8]) -> Result<(u64, u8)> {
    if value.len() != 9 || !matches!(value[8], SECONDARY_DELETE | SECONDARY_ADD) {
        return Err(Error::Corrupt("secondary index: invalid operation".into()));
    }
    Ok((
        u64::from_be_bytes(value[..8].try_into().expect("eight bytes")),
        value[8],
    ))
}

fn validate_secondary_run(index: &PagedIndex) -> Result<()> {
    let mut valid = false;
    index.visit_key(SECONDARY_FORMAT_KEY, |value| {
        valid = value == SECONDARY_FORMAT_VALUE;
        Ok(false)
    })?;
    if valid {
        Ok(())
    } else {
        Err(Error::Corrupt(
            "secondary index: unsupported run format".into(),
        ))
    }
}

struct State {
    catalog: Catalog,
    committed_version: u64,
    /// Highest reserved integer identity per table. Committed values are
    /// carried in the WAL and checkpoints copy the map into the manifest.
    identity_high_water: HashMap<String, i64>,
    /// table -> id -> versions in ascending commit order.
    index: PrimaryIdx,
    /// Greatest primary key observed in the current table epoch. Keys above
    /// this watermark are provably absent, which avoids run lookups for
    /// ordered imports and the normal monotonically increasing ULID path.
    table_high_ids: HashMap<String, String>,
    /// Segments containing at least one put superseded by a newer version.
    /// Clean append-only segments can be filtered sequentially without one
    /// primary-index lookup per row.
    superseded_segments: HashSet<u32>,
    /// (table, column) -> equality index over the latest committed state.
    secondary: HashMap<(String, String), SecIdx>,
    /// (table, column) -> ANN index over the latest committed state.
    vector: HashMap<(String, String), VecIdx>,
    /// (table, column) -> full-text index over the latest committed state.
    text: HashMap<(String, String), TextIdx>,
    /// Directory for out-of-line blob chunks.
    blobs: PathBuf,
    readers: SegmentReaders,
    segments: Vec<SegmentMeta>,
    next_segment_id: u32,
}

/// Open immutable segment handles. Keeping them behind `Arc` lets read paths
/// retain exactly the files they need after releasing the global state lock.
/// Compaction may unlink an old segment meanwhile, but the open descriptor
/// remains valid until the last in-flight reader drops its clone.
type SegmentReaders = HashMap<u32, Arc<File>>;

impl State {
    fn id_is_above_high_watermark(&self, table: &str, id: &str) -> bool {
        self.table_high_ids
            .get(table)
            .is_none_or(|high| id > high.as_str())
    }

    fn record_high_id(&mut self, table: &str, id: &str) {
        match self.table_high_ids.entry(table.to_owned()) {
            std::collections::hash_map::Entry::Occupied(mut entry) => {
                if id > entry.get().as_str() {
                    *entry.get_mut() = id.to_owned();
                }
            }
            std::collections::hash_map::Entry::Vacant(entry) => {
                entry.insert(id.to_owned());
            }
        }
    }

    fn latest_owned(&self, table: &str, id: &str) -> Result<Option<VersionEntry>> {
        let Some(schema) = self.catalog.table(table) else {
            return Ok(None);
        };
        Ok(self
            .index
            .latest(table, id)?
            .filter(|entry| entry.version > schema.epoch))
    }

    fn visible_owned(
        &self,
        table: &str,
        id: &str,
        max_version: u64,
    ) -> Result<Option<VersionEntry>> {
        let Some(schema) = self.catalog.table(table) else {
            return Ok(None);
        };
        Ok(self
            .index
            .visible(table, id, max_version)?
            .filter(|entry| entry.version > schema.epoch))
    }

    fn index_delta_memory_bytes(&self) -> usize {
        self.index
            .delta_memory_bytes()
            .saturating_add(self.derived_delta_memory_bytes())
    }

    fn derived_delta_memory_bytes(&self) -> usize {
        self.secondary
            .values()
            .map(SecIdx::delta_memory_bytes)
            .sum::<usize>()
            .saturating_add(self.text.values().map(TextIdx::delta_memory_bytes).sum())
            .saturating_add(self.vector.values().map(VecIdx::delta_memory_bytes).sum())
    }

    fn derived_delta_memory_bytes_including_frozen(&self) -> usize {
        self.derived_delta_memory_bytes()
            .saturating_add(
                self.secondary
                    .values()
                    .map(SecIdx::frozen_delta_memory_bytes)
                    .sum(),
            )
            .saturating_add(
                self.text
                    .values()
                    .map(TextIdx::frozen_delta_memory_bytes)
                    .sum(),
            )
            .saturating_add(
                self.vector
                    .values()
                    .map(VecIdx::frozen_delta_memory_bytes)
                    .sum(),
            )
    }

    fn has_frozen_derived(&self) -> bool {
        self.secondary.values().any(|index| index.frozen.is_some())
            || self
                .text
                .values()
                .any(|index| index.frozen_delta().is_some())
            || self
                .vector
                .values()
                .any(|index| index.frozen_delta().is_some())
    }
}

fn identity_manifest(state: &State) -> BTreeMap<String, i64> {
    state
        .identity_high_water
        .iter()
        .map(|(table, value)| (table.clone(), *value))
        .collect()
}

/// A vector waiting to be indexed in background (Async mode).
struct VecJob {
    version: u64,
    table: String,
    column: String,
    id: String,
    vector: Vec<f32>,
}

struct CommitState {
    /// None only in read-only mode, where every write path is guarded.
    wal: Option<WalWriter>,
    memtable_bytes: u64,
    /// The current group accepts commits during its short coalescing window.
    /// Its own condition variable lets waiters sleep without retaining the
    /// global commit mutex.
    wal_sync_group: Option<Arc<WalSyncGroup>>,
}

#[derive(Default)]
struct CommitCoordinatorState {
    active: bool,
    queue: VecDeque<Arc<CoordinatedCommit>>,
}

struct CoordinatedCommit {
    prepared: Mutex<Option<PreparedCommit>>,
    result: Mutex<Option<Result<u64>>>,
    changed: Condvar,
    lead: AtomicBool,
    queued_at: Instant,
}

impl CoordinatedCommit {
    fn new(prepared: PreparedCommit) -> Self {
        Self {
            prepared: Mutex::new(Some(prepared)),
            result: Mutex::new(None),
            changed: Condvar::new(),
            lead: AtomicBool::new(false),
            queued_at: Instant::now(),
        }
    }

    fn complete(&self, result: Result<u64>) {
        *self.result.lock().unwrap() = Some(result);
        self.changed.notify_one();
    }

    fn promote_to_leader(&self) {
        // Hold the same mutex used by the waiter while publishing the baton.
        // Otherwise a notify between the waiter's predicate check and its
        // actual sleep could be lost and leave the coordinator stalled.
        let _result = self.result.lock().unwrap();
        self.lead.store(true, AtomicOrdering::Release);
        self.changed.notify_one();
    }

    fn take_result(&self) -> Option<Result<u64>> {
        self.result.lock().unwrap().take()
    }

    fn wait_for_result_or_lead(&self) -> Option<Result<u64>> {
        let mut result = self.result.lock().unwrap();
        while result.is_none() && !self.lead.swap(false, AtomicOrdering::AcqRel) {
            result = self.changed.wait(result).unwrap();
        }
        result.take()
    }
}

struct ActiveStateWriter<'a>(&'a Shared);

impl<'a> ActiveStateWriter<'a> {
    fn enter(shared: &'a Shared) -> Self {
        shared
            .active_state_writers
            .fetch_add(1, AtomicOrdering::Relaxed);
        Self(shared)
    }
}

impl Drop for ActiveStateWriter<'_> {
    fn drop(&mut self) {
        self.0
            .active_state_writers
            .fetch_sub(1, AtomicOrdering::Relaxed);
    }
}

struct PointReadAdmission<'a> {
    shared: &'a Shared,
    counted: bool,
}

impl<'a> PointReadAdmission<'a> {
    fn enter(shared: &'a Shared) -> Self {
        let mut throttled = false;
        loop {
            let writers = shared.active_state_writers.load(AtomicOrdering::Relaxed);
            if writers == 0 {
                return Self {
                    shared,
                    counted: false,
                };
            }
            let parallelism = shared.point_read_parallelism.max(1);
            let writer_slots = usize::try_from(writers)
                .unwrap_or(usize::MAX)
                .min(parallelism.saturating_sub(1));
            let reader_limit = parallelism.saturating_sub(writer_slots).max(1);
            let previous = shared
                .admitted_point_reads
                .fetch_add(1, AtomicOrdering::Relaxed);
            if shared.active_state_writers.load(AtomicOrdering::Relaxed) == 0
                || previous < reader_limit as u64
            {
                return Self {
                    shared,
                    counted: true,
                };
            }
            shared
                .admitted_point_reads
                .fetch_sub(1, AtomicOrdering::Relaxed);
            if !throttled {
                shared
                    .point_read_throttle_count
                    .fetch_add(1, AtomicOrdering::Relaxed);
                throttled = true;
            }
            std::thread::yield_now();
        }
    }
}

impl Drop for PointReadAdmission<'_> {
    fn drop(&mut self) {
        if self.counted {
            self.shared
                .admitted_point_reads
                .fetch_sub(1, AtomicOrdering::Relaxed);
        }
    }
}

enum CommitMutex {
    /// Preserve the standard mutex's scheduling behavior for Safe group
    /// commit, where short bursts should append before one elected fsync.
    Safe(Mutex<CommitState>),
    /// Adaptive spinning plus explicit fair handoff bounds the queue for
    /// Fast/Balanced commits that do not require an fsync every time.
    Concurrent(ParkingMutex<CommitState>),
}

enum CommitGuard<'a> {
    Safe(MutexGuard<'a, CommitState>),
    Concurrent(ParkingMutexGuard<'a, CommitState>),
}

impl CommitMutex {
    fn new(state: CommitState, durability: Durability) -> Self {
        if durability == Durability::Safe {
            Self::Safe(Mutex::new(state))
        } else {
            Self::Concurrent(ParkingMutex::new(state))
        }
    }

    fn lock(&self) -> CommitGuard<'_> {
        match self {
            Self::Safe(mutex) => CommitGuard::Safe(mutex.lock().unwrap()),
            Self::Concurrent(mutex) => CommitGuard::Concurrent(mutex.lock()),
        }
    }
}

impl Deref for CommitGuard<'_> {
    type Target = CommitState;

    fn deref(&self) -> &Self::Target {
        match self {
            Self::Safe(guard) => guard,
            Self::Concurrent(guard) => guard,
        }
    }
}

impl DerefMut for CommitGuard<'_> {
    fn deref_mut(&mut self) -> &mut Self::Target {
        match self {
            Self::Safe(guard) => guard,
            Self::Concurrent(guard) => guard,
        }
    }
}

impl CommitGuard<'_> {
    fn unlock_fair(self) {
        match self {
            Self::Safe(guard) => drop(guard),
            Self::Concurrent(guard) => ParkingMutexGuard::unlock_fair(guard),
        }
    }
}

type WalGroupResult = std::result::Result<(), String>;

struct WalSyncGroup {
    result: Mutex<Option<WalGroupResult>>,
    done: Condvar,
    commits: AtomicU64,
    bytes: AtomicU64,
}

struct TimedCommitGuard<'a> {
    guard: Option<CommitGuard<'a>>,
    started: Instant,
    total_nanos: &'a AtomicU64,
    waiters: &'a AtomicU64,
    fair_handoff: bool,
}

impl Deref for TimedCommitGuard<'_> {
    type Target = CommitState;

    fn deref(&self) -> &Self::Target {
        self.guard.as_deref().expect("commit guard is held")
    }
}

impl DerefMut for TimedCommitGuard<'_> {
    fn deref_mut(&mut self) -> &mut Self::Target {
        self.guard.as_deref_mut().expect("commit guard is held")
    }
}

impl Drop for TimedCommitGuard<'_> {
    fn drop(&mut self) {
        self.total_nanos.fetch_add(
            self.started.elapsed().as_nanos().min(u64::MAX as u128) as u64,
            AtomicOrdering::Relaxed,
        );
        let guard = self.guard.take().expect("commit guard is held");
        if self.fair_handoff && self.waiters.load(AtomicOrdering::Acquire) > 0 {
            guard.unlock_fair();
        }
    }
}

impl WalSyncGroup {
    fn new(bytes: u64) -> Self {
        Self {
            result: Mutex::new(None),
            done: Condvar::new(),
            commits: AtomicU64::new(1),
            bytes: AtomicU64::new(bytes),
        }
    }

    fn join(&self, bytes: u64) {
        self.commits.fetch_add(1, AtomicOrdering::Relaxed);
        self.bytes.fetch_add(bytes, AtomicOrdering::Relaxed);
    }

    fn complete(&self, result: WalGroupResult) {
        *self.result.lock().unwrap() = Some(result);
        self.done.notify_all();
    }

    fn wait(&self) -> WalGroupResult {
        let mut result = self.result.lock().unwrap();
        while result.is_none() {
            result = self.done.wait(result).unwrap();
        }
        result.as_ref().expect("group completion stored").clone()
    }
}

struct FrozenCheckpointJob {
    frozen: Arc<PrimaryDelta>,
    version: u64,
    segments: Vec<SegmentMeta>,
    next_segment_id: u32,
    catalog: Catalog,
    identity_high_water: BTreeMap<String, i64>,
    first_primary_run: bool,
    wal_id: u32,
    wal_cutoff: u64,
    memory: Option<MemoryPermit>,
}

struct FrozenSecondaryJob {
    key: (String, String),
    frozen: Arc<FrozenSecDelta>,
}

struct FrozenTextJob {
    key: (String, String),
    frozen: Arc<FrozenTextDelta>,
}

struct FrozenVectorJob {
    table: String,
    def: VectorIndexDef,
    frozen: Arc<VecIdx>,
    generation: u64,
}

struct DerivedCheckpointJob {
    secondary: Vec<FrozenSecondaryJob>,
    text: Vec<FrozenTextJob>,
    vector: Vec<FrozenVectorJob>,
    memory: Option<MemoryPermit>,
}

#[derive(Debug, Default)]
struct BackgroundCheckpointState {
    running: bool,
    last_error: Option<String>,
    commit_unknown: bool,
}

#[derive(Debug, Default)]
struct BackgroundDerivedState {
    running: bool,
    last_error: Option<String>,
}

#[derive(Debug, Default)]
struct AutoCompactionState {
    debt_operations: u64,
    estimated_reclaimable_bytes: u64,
    scheduled: bool,
    last_attempt: Option<Instant>,
}

enum MaintenanceJob {
    Compact,
    CompactPrimaryRuns,
    CompactSecondaryRuns,
    CompactTextRuns,
}

impl CommitState {
    fn wal(&mut self) -> &mut WalWriter {
        self.wal
            .as_mut()
            .expect("write paths are guarded by read_only")
    }
}

struct Shared {
    dir: PathBuf,
    opts: DbOptions,
    memory_governor: Arc<MemoryGovernor>,
    /// Held for the lifetime of the Db: process-level exclusion.
    _lock_file: File,
    state: RwLock<State>,
    /// Serializes commits, checkpoints and compaction. Writers stage in
    /// parallel without this lock and only meet here, at commit.
    commit: CommitMutex,
    /// Prepared transactions queue here before one elected caller drains a
    /// bounded batch. Complex transactions retain the general serialized
    /// path; disjoint inserts share WAL and publication overhead.
    commit_coordinator: Mutex<CommitCoordinatorState>,
    /// A canonical rename completed but its directory sync did not. Reads may
    /// continue over the adopted generation, but no later write may extend an
    /// outcome whose crash durability is unknown.
    canonical_error: Mutex<Option<String>>,
    /// Blob filenames made durable before their WAL record is appended.
    /// Compaction GC preserves these while commit validation runs without the
    /// global commit mutex.
    pending_blob_publications: Mutex<HashSet<String>>,
    /// Serializes catalog changes from validation through durable intent,
    /// application and intent cleanup. The commit mutex alone is insufficient
    /// for multi-phase DDL because backfills deliberately release it.
    ddl: Mutex<()>,
    commit_count: AtomicU64,
    commit_nanos: AtomicU64,
    commit_lock_wait_nanos: AtomicU64,
    commit_lock_hold_nanos: AtomicU64,
    commit_prepare_nanos: AtomicU64,
    commit_locked_prepare_nanos: AtomicU64,
    commit_wal_nanos: AtomicU64,
    commit_wal_append_nanos: AtomicU64,
    commit_apply_nanos: AtomicU64,
    /// Committers currently queued for the serialization mutex. A group
    /// leader only opens a coalescing window when contention already exists,
    /// preserving single-writer latency.
    commit_waiters: AtomicU64,
    wal_sync_count: AtomicU64,
    wal_sync_nanos: AtomicU64,
    wal_synced_bytes: AtomicU64,
    wal_sync_max_group_commits: AtomicU64,
    wal_sync_max_group_bytes: AtomicU64,
    wal_group_coalesce_nanos: AtomicU64,
    wal_group_leader_lock_wait_nanos: AtomicU64,
    grouped_commit_count: AtomicU64,
    coordinated_batch_count: AtomicU64,
    coordinated_commit_count: AtomicU64,
    /// CPU-aware admission used only while commits or identity reservations
    /// are active. Read-only workloads retain an unthrottled point-read path.
    point_read_parallelism: usize,
    active_state_writers: AtomicU64,
    /// Short-lived memory that a concurrent Safe burst was observed. Leaders
    /// consume it only after direct concurrency is no longer visible.
    safe_coalesce_budget: AtomicU64,
    admitted_point_reads: AtomicU64,
    point_read_throttle_count: AtomicU64,
    /// version -> live snapshot refcount; compaction preserves these.
    snapshots: Mutex<BTreeMap<u64, usize>>,
    /// Last generated or observed ULID, used to keep implicit ids increasing.
    /// It is initialized from the largest persisted ULID when the database opens.
    last_generated_id: Mutex<Ulid>,
    /// Queue into the background vector-indexing thread (Async mode).
    vector_tx: Mutex<Option<mpsc::Sender<VecJob>>>,
    /// Vectors enqueued but not yet searchable.
    vector_backlog: AtomicU64,
    vector_worker_alive: AtomicBool,
    vector_worker_error: Mutex<Option<String>>,
    /// Queue into the single background compaction worker.
    maintenance_tx: Mutex<Option<mpsc::Sender<MaintenanceJob>>>,
    maintenance_worker_alive: AtomicBool,
    maintenance_worker_error: Mutex<Option<String>>,
    /// Failed primary/derived run promotions. Waiters permit bounded retries:
    /// publication races may be transient, persistent I/O errors must not spin.
    index_maintenance_failures: AtomicU64,
    /// Dedicated writer for one frozen primary memtable. It is separate from
    /// the compaction queue because the queued job owns the maintenance pool
    /// while its resident generation remains visible to readers.
    checkpoint_tx: Mutex<Option<mpsc::Sender<FrozenCheckpointJob>>>,
    derived_tx: Mutex<Option<mpsc::Sender<DerivedCheckpointJob>>>,
    background_checkpoint: Mutex<BackgroundCheckpointState>,
    background_checkpoint_done: Condvar,
    background_derived: Mutex<BackgroundDerivedState>,
    background_derived_done: Condvar,
    /// Orders equality/BM25 run-set replacements without making ordinary
    /// commits wait for manifest writes or directory syncs.
    derived_manifest_publication: Mutex<()>,
    derived_publication_count: AtomicU64,
    derived_publication_nanos: AtomicU64,
    #[cfg(test)]
    derived_test_pause_before_publish: AtomicBool,
    #[cfg(test)]
    derived_test_reached_publish: AtomicBool,
    #[cfg(test)]
    derived_test_fail_before_manifest_rename: AtomicBool,
    #[cfg(test)]
    derived_test_fail_after_manifest_rename: AtomicBool,
    #[cfg(test)]
    blob_test_pause_after_publish: AtomicBool,
    #[cfg(test)]
    blob_test_reached_publish: AtomicBool,
    #[cfg(test)]
    primary_test_pause_before_manifest: AtomicBool,
    #[cfg(test)]
    primary_test_reached_manifest: AtomicBool,
    #[cfg(test)]
    primary_test_fail_before_manifest_rename: AtomicBool,
    #[cfg(test)]
    primary_test_fail_after_manifest_rename: AtomicBool,
    primary_compaction_scheduled: AtomicBool,
    /// Orders primary run-set replacements while their manifests are written
    /// without holding the commit mutex.
    primary_manifest_publication: Mutex<()>,
    primary_run_compaction_count: AtomicU64,
    primary_run_compaction_nanos: AtomicU64,
    primary_run_compaction_bytes_read: AtomicU64,
    primary_run_compaction_bytes_written: AtomicU64,
    primary_checkpoint_bytes_written: AtomicU64,
    secondary_compaction_scheduled: AtomicBool,
    secondary_run_compaction_count: AtomicU64,
    secondary_run_compaction_nanos: AtomicU64,
    secondary_run_compaction_bytes_read: AtomicU64,
    secondary_run_compaction_bytes_written: AtomicU64,
    secondary_checkpoint_bytes_written: AtomicU64,
    text_compaction_scheduled: AtomicBool,
    text_run_compaction_count: AtomicU64,
    text_run_compaction_nanos: AtomicU64,
    text_run_compaction_bytes_read: AtomicU64,
    text_run_compaction_bytes_written: AtomicU64,
    text_checkpoint_bytes_written: AtomicU64,
    auto_compaction_state: Mutex<AutoCompactionState>,
    checkpoint_count: AtomicU64,
    checkpoint_nanos: AtomicU64,
    automatic_compaction_count: AtomicU64,
    automatic_compaction_nanos: AtomicU64,
    automatic_compaction_failures: AtomicU64,
    automatic_compaction_bytes_reclaimed: AtomicU64,
    /// Snapshot versions that forced the most recently compacted segment set
    /// to retain an otherwise obsolete record version.
    compaction_retained_snapshots: Mutex<HashSet<u64>>,
    /// A released snapshot can make a previously retained version obsolete.
    /// Recompute that debt once at the next maintenance boundary, rather than
    /// rescanning the full primary index after every checkpoint.
    compaction_refresh_needed: AtomicBool,
    query_spill_files: AtomicU64,
    query_spilled_bytes: AtomicU64,
    query_peak_buffer_bytes: AtomicU64,
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
                if self
                    .shared
                    .compaction_retained_snapshots
                    .lock()
                    .unwrap()
                    .remove(&self.version)
                {
                    self.shared
                        .compaction_refresh_needed
                        .store(true, AtomicOrdering::Release);
                }
            }
        }
    }
}

impl std::fmt::Debug for Snapshot {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Snapshot")
            .field("version", &self.version)
            .finish()
    }
}

fn register_snapshot(shared: &Arc<Shared>, version: u64) {
    *shared.snapshots.lock().unwrap().entry(version).or_insert(0) += 1;
}

/// Parameters for [`Db::search_hybrid`]: one or both modalities.
#[derive(Debug, Clone, Default)]
pub struct HybridQuery<'a> {
    /// (text column, query string).
    pub text: Option<(&'a str, &'a str)>,
    /// (vector column, query embedding).
    pub vector: Option<(&'a str, &'a [f32])>,
    pub top_k: usize,
    pub ef_search: Option<usize>,
    /// Equality filters on other columns.
    pub filter: Option<Record>,
}

/// One hybrid hit; higher `score` (RRF) is better.
#[derive(Debug, Clone)]
pub struct HybridHit {
    pub id: String,
    pub score: f32,
    pub record: Record,
}

/// A read-write transaction. Reads see a stable snapshot plus this
/// transaction's own staged writes. Writes are buffered locally and only
/// meet other writers at `commit`, where optimistic validation either
/// publishes them atomically or fails with `Error::Conflict`.
pub struct Txn {
    shared: Arc<Shared>,
    snapshot: Snapshot,
    started_at: Instant,
    /// One interned table name/schema plus its staged operations. Keeping IDs
    /// in a per-table map avoids allocating and hashing the table name once
    /// per row during large transactions.
    staged: Vec<(String, StagedTable)>,
    staged_bytes: usize,
}

struct StagedTable {
    schema: TableSchema,
    /// High watermark observed when the table was first touched. IDs above it
    /// were absent from this transaction's snapshot, so monotonic inserts do
    /// not need to reacquire the shared state lock per row. Commit validation
    /// still catches a concurrent writer that inserts the same ID.
    snapshot_high_id: Option<String>,
    operations: HashMap<String, StagedOperation>,
    next_position: usize,
}

struct StagedOperation {
    /// Original insertion order. Monotonic batches can be reconstructed in
    /// linear time at commit instead of sorting every row by primary key.
    position: usize,
    operation: Option<Record>,
    /// Transaction-visible value that was removed. Cascades need this even
    /// when the row was inserted or updated earlier in the same transaction.
    deleted_record: Option<Record>,
}

struct PreparedTable {
    name: String,
    schema: TableSchema,
    changes: Vec<PreparedChange>,
}

struct PreparedChange {
    id: String,
    operation: Option<Record>,
    payload: Option<Arc<Vec<u8>>>,
}

struct PreparedCommit {
    snap_version: u64,
    staged: Vec<PreparedTable>,
    preencoded_wal: Option<Vec<u8>>,
    optimistic_prior_records: Vec<Vec<Option<(u64, Record)>>>,
    incoming_index_bytes: usize,
    outside_prepare_time: Duration,
    commit_started: Instant,
    _pending_blobs: PendingBlobPublications,
}

struct ApplyRecordState<'a> {
    put: Option<(&'a Arc<Vec<u8>>, &'a Record)>,
    prior: Option<Record>,
}

/// An embedded EliteSQL database backed by a self-contained directory.
///
/// Storage: commits are appended to a durable WAL (fsync per the
/// durability mode) and applied to an in-memory MVCC index; checkpoints
/// drain committed data into immutable segments and publish an atomic
/// self-contained data/schema manifest (with a redundant `manifest.prev`
/// recovery copy). On open, the manifest chain is loaded, segments are scanned, and the WAL is replayed
/// idempotently; a torn WAL tail is truncated. Vector (ANN) indexes are
/// derived structures rebuilt from canonical data on open and compaction.
pub struct Db {
    shared: Arc<Shared>,
    vector_thread: Option<std::thread::JoinHandle<()>>,
    maintenance_thread: Option<std::thread::JoinHandle<()>>,
    checkpoint_thread: Option<std::thread::JoinHandle<()>>,
    derived_thread: Option<std::thread::JoinHandle<()>>,
}

impl Drop for Db {
    fn drop(&mut self) {
        // A frozen generation remains necessary for reads and WAL recovery
        // until its publication finishes, so drain it before other workers.
        *self.shared.checkpoint_tx.lock().unwrap() = None;
        if let Some(handle) = self.checkpoint_thread.take() {
            let _ = handle.join();
        }
        *self.shared.derived_tx.lock().unwrap() = None;
        if let Some(handle) = self.derived_thread.take() {
            let _ = handle.join();
        }
        // Finish an already queued/running compaction before releasing the
        // process lock. No new jobs can be queued once this sender is gone.
        *self.shared.maintenance_tx.lock().unwrap() = None;
        if let Some(handle) = self.maintenance_thread.take() {
            let _ = handle.join();
        }
        // Close the channel so the background indexer exits, then join it
        // so every pending async vector lands before the graph is dumped.
        *self.shared.vector_tx.lock().unwrap() = None;
        if let Some(handle) = self.vector_thread.take() {
            let _ = handle.join();
        }
        // A transaction may outlive its originating handle, so preserve the
        // normal commit -> state lock order while publishing final deltas.
        let _commit = self.shared.commit.lock();
        let _ = consolidate_derived_indexes(&self.shared);
    }
}

fn record_maintenance_error(shared: &Shared, message: impl Into<String>) {
    *shared
        .maintenance_worker_error
        .lock()
        .unwrap_or_else(|poison| poison.into_inner()) = Some(message.into());
}

fn maintenance_wait_error(shared: &Shared, operation: &str) -> Error {
    let message = shared
        .maintenance_worker_error
        .lock()
        .unwrap_or_else(|poison| poison.into_inner())
        .clone()
        .unwrap_or_else(|| format!("{operation} did not complete after bounded retries"));
    Error::Io(std::io::Error::other(message))
}

/// Attach the background vector-indexing thread and produce the handle.
fn finish_db(shared: Arc<Shared>) -> Db {
    if !shared.opts.read_only {
        cleanup_derived_pending_files(&shared.dir);
    }
    let initial_delta_bytes = shared.state.read().unwrap().index_delta_memory_bytes();
    shared
        .memory_governor
        .set_index_delta_bytes(initial_delta_bytes);
    // Pending vector payloads are admitted through and charged to the bounded
    // index-delta pool before commit. An unbounded channel is therefore safe:
    // the memory governor supplies the bound, while `send` never parks the
    // global commit mutex behind a slower indexing worker.
    let (tx, rx) = mpsc::channel::<VecJob>();
    *shared.vector_tx.lock().unwrap() = Some(tx);
    let sh = shared.clone();
    let handle = std::thread::spawn(move || {
        let outcome = catch_unwind(AssertUnwindSafe(|| {
            while let Ok(job) = rx.recv() {
                {
                    let mut st = sh.state.write().unwrap();
                    // A newer update/delete can commit before this background
                    // job runs. Never let an old queued vector resurrect or
                    // overwrite that newer canonical state.
                    let still_current = st
                        .latest_owned(&job.table, &job.id)
                        .ok()
                        .flatten()
                        .is_some_and(|entry| entry.version == job.version && !entry.is_tombstone());
                    if still_current {
                        if let Some(vidx) = st.vector.get_mut(&(job.table, job.column)) {
                            vidx.insert(&job.id, &job.vector);
                        }
                    }
                }
                sh.vector_backlog.fetch_sub(1, AtomicOrdering::SeqCst);
            }
        }));
        if outcome.is_err() {
            *sh.vector_worker_error
                .lock()
                .unwrap_or_else(|poison| poison.into_inner()) =
                Some("vector indexing worker panicked".into());
        }
        sh.vector_worker_alive.store(false, AtomicOrdering::Release);
    });

    let (maintenance_tx, maintenance_rx) = mpsc::channel::<MaintenanceJob>();
    *shared.maintenance_tx.lock().unwrap() = Some(maintenance_tx);
    let maintenance_shared = shared.clone();
    let maintenance_handle = std::thread::spawn(move || {
        let outcome = catch_unwind(AssertUnwindSafe(|| {
            while let Ok(job) = maintenance_rx.recv() {
                match job {
                    MaintenanceJob::Compact => {
                        let should_run = auto_compaction_needed(&maintenance_shared);
                        if should_run {
                            let before = segment_bytes(&maintenance_shared);
                            let started = Instant::now();
                            let result =
                                Db::rewrite_segments_shared(&maintenance_shared, &Rewrite::None);
                            let nanos = started.elapsed().as_nanos().min(u64::MAX as u128) as u64;
                            if result.is_ok() {
                                let after = segment_bytes(&maintenance_shared);
                                maintenance_shared
                                    .automatic_compaction_count
                                    .fetch_add(1, AtomicOrdering::Relaxed);
                                maintenance_shared
                                    .automatic_compaction_nanos
                                    .fetch_add(nanos, AtomicOrdering::Relaxed);
                                maintenance_shared
                                    .automatic_compaction_bytes_reclaimed
                                    .fetch_add(
                                        before.saturating_sub(after),
                                        AtomicOrdering::Relaxed,
                                    );
                            } else {
                                record_maintenance_error(
                                    &maintenance_shared,
                                    "automatic segment compaction failed",
                                );
                                maintenance_shared
                                    .automatic_compaction_failures
                                    .fetch_add(1, AtomicOrdering::Relaxed);
                            }
                        }
                        let mut auto = maintenance_shared.auto_compaction_state.lock().unwrap();
                        auto.scheduled = false;
                        if should_run {
                            auto.last_attempt = Some(Instant::now());
                        }
                    }
                    MaintenanceJob::CompactPrimaryRuns => {
                        let mut failed = false;
                        while primary_compaction_needed(&maintenance_shared) {
                            if compact_one_primary_level(&maintenance_shared).is_err() {
                                record_maintenance_error(
                                    &maintenance_shared,
                                    "primary run compaction failed",
                                );
                                failed = true;
                                maintenance_shared
                                    .index_maintenance_failures
                                    .fetch_add(1, AtomicOrdering::Relaxed);
                                break;
                            }
                        }
                        maintenance_shared
                            .primary_compaction_scheduled
                            .store(false, AtomicOrdering::Release);
                        if !failed {
                            maybe_schedule_primary_compaction(&maintenance_shared);
                        }
                    }
                    MaintenanceJob::CompactSecondaryRuns => {
                        let mut failed = false;
                        while secondary_compaction_needed(&maintenance_shared) {
                            if compact_one_secondary_level(&maintenance_shared).is_err() {
                                record_maintenance_error(
                                    &maintenance_shared,
                                    "secondary run compaction failed",
                                );
                                failed = true;
                                maintenance_shared
                                    .index_maintenance_failures
                                    .fetch_add(1, AtomicOrdering::Relaxed);
                                break;
                            }
                        }
                        maintenance_shared
                            .secondary_compaction_scheduled
                            .store(false, AtomicOrdering::Release);
                        if !failed {
                            maybe_schedule_secondary_compaction(&maintenance_shared);
                        }
                    }
                    MaintenanceJob::CompactTextRuns => {
                        let mut failed = false;
                        while text_compaction_needed(&maintenance_shared) {
                            if compact_one_text_level(&maintenance_shared).is_err() {
                                record_maintenance_error(
                                    &maintenance_shared,
                                    "text run compaction failed",
                                );
                                failed = true;
                                maintenance_shared
                                    .index_maintenance_failures
                                    .fetch_add(1, AtomicOrdering::Relaxed);
                                break;
                            }
                        }
                        maintenance_shared
                            .text_compaction_scheduled
                            .store(false, AtomicOrdering::Release);
                        if !failed {
                            maybe_schedule_text_compaction(&maintenance_shared);
                        }
                    }
                }
            }
        }));
        maintenance_shared
            .maintenance_worker_alive
            .store(false, AtomicOrdering::Release);
        if outcome.is_err() {
            record_maintenance_error(&maintenance_shared, "maintenance worker panicked");
            maintenance_shared
                .index_maintenance_failures
                .fetch_add(1, AtomicOrdering::Relaxed);
            maintenance_shared
                .primary_compaction_scheduled
                .store(false, AtomicOrdering::Release);
            maintenance_shared
                .secondary_compaction_scheduled
                .store(false, AtomicOrdering::Release);
            maintenance_shared
                .text_compaction_scheduled
                .store(false, AtomicOrdering::Release);
            maintenance_shared
                .auto_compaction_state
                .lock()
                .unwrap()
                .scheduled = false;
        }
    });

    let (checkpoint_tx, checkpoint_rx) = mpsc::channel::<FrozenCheckpointJob>();
    *shared.checkpoint_tx.lock().unwrap() = Some(checkpoint_tx);
    let checkpoint_shared = shared.clone();
    let checkpoint_handle = std::thread::spawn(move || {
        let outcome = catch_unwind(AssertUnwindSafe(|| {
            while let Ok(job) = checkpoint_rx.recv() {
                let result = flush_frozen_checkpoint(&checkpoint_shared, job);
                let mut status = checkpoint_shared.background_checkpoint.lock().unwrap();
                status.running = false;
                status.commit_unknown = matches!(&result, Err(Error::CommitUnknown(_)));
                status.last_error = result.err().map(|error| error.to_string());
                checkpoint_shared.background_checkpoint_done.notify_all();
            }
        }));
        if outcome.is_err() {
            let mut status = checkpoint_shared
                .background_checkpoint
                .lock()
                .unwrap_or_else(|poison| poison.into_inner());
            status.running = false;
            status.commit_unknown = false;
            status.last_error = Some("background checkpoint worker panicked".into());
            checkpoint_shared.background_checkpoint_done.notify_all();
            drop(status);
            let _ = catch_unwind(AssertUnwindSafe(|| {
                thaw_frozen_checkpoint(&checkpoint_shared)
            }));
        }
    });

    let (derived_tx, derived_rx) = mpsc::channel::<DerivedCheckpointJob>();
    *shared.derived_tx.lock().unwrap() = Some(derived_tx);
    let derived_shared = shared.clone();
    let derived_handle = std::thread::spawn(move || {
        let outcome = catch_unwind(AssertUnwindSafe(|| {
            while let Ok(job) = derived_rx.recv() {
                let started = Instant::now();
                let result = flush_frozen_derived(&derived_shared, job);
                if result.is_err() {
                    // The failed job no longer owns its maintenance permit,
                    // but its frozen generation remains queryable for retry.
                    // Transfer that resident memory back to the delta pool.
                    let _commit = derived_shared.commit.lock();
                    let state = derived_shared.state.read().unwrap();
                    let retained = state
                        .index
                        .delta_memory_bytes()
                        .saturating_add(state.derived_delta_memory_bytes_including_frozen());
                    drop(state);
                    derived_shared
                        .memory_governor
                        .set_index_delta_bytes(retained);
                }
                if result.is_ok() {
                    derived_shared
                        .derived_publication_count
                        .fetch_add(1, AtomicOrdering::Relaxed);
                    derived_shared.derived_publication_nanos.fetch_add(
                        started.elapsed().as_nanos().min(u64::MAX as u128) as u64,
                        AtomicOrdering::Relaxed,
                    );
                }
                let mut status = derived_shared.background_derived.lock().unwrap();
                status.running = false;
                status.last_error = result.err().map(|error| error.to_string());
                if status.last_error.is_some() {
                    derived_shared
                        .index_maintenance_failures
                        .fetch_add(1, AtomicOrdering::Relaxed);
                }
                derived_shared.background_derived_done.notify_all();
            }
        }));
        if outcome.is_err() {
            let mut status = derived_shared
                .background_derived
                .lock()
                .unwrap_or_else(|poison| poison.into_inner());
            status.running = false;
            status.last_error = Some("background derived-index worker panicked".into());
            derived_shared.background_derived_done.notify_all();
        }
    });

    Db {
        shared,
        vector_thread: Some(handle),
        maintenance_thread: Some(maintenance_handle),
        checkpoint_thread: Some(checkpoint_handle),
        derived_thread: Some(derived_handle),
    }
}

fn cleanup_derived_pending_files(dir: &Path) {
    for child in [INDEXES_DIR, VECTORS_DIR] {
        let Ok(entries) = fs::read_dir(dir.join(child)) else {
            continue;
        };
        for entry in entries.flatten() {
            let path = entry.path();
            let pending = path
                .file_name()
                .and_then(|name| name.to_str())
                .is_some_and(|name| {
                    name.ends_with(".derived-pending")
                        || name.ends_with(".derived-pending.tmp")
                        || name.ends_with(".primary-pending")
                        || name.ends_with(".primary-pending.tmp")
                });
            if pending {
                let _ = fs::remove_file(path);
            }
        }
    }
}

impl Db {
    pub(crate) fn acquire_ddl_guard(&self) -> MutexGuard<'_, ()> {
        self.shared.ddl.lock().unwrap()
    }

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
        let path = path.as_ref();
        if path.join(MARKER_FILE).exists() {
            Db::open_with(path, opts)
        } else {
            match Db::create_with(path, opts.clone()) {
                Ok(db) => Ok(db),
                // Another creator may have won between the initial probe and
                // our exclusive lock. `create_with` rechecks under that lock
                // and refuses to overwrite; reopen through the normal path.
                Err(_) if path.join(MARKER_FILE).exists() => Db::open_with(path, opts),
                Err(error) => Err(error),
            }
        }
    }

    /// Open read-only: reads work (over the valid prefix of any damaged
    /// file), nothing on disk is touched, every write returns
    /// [`Error::ReadOnly`]. Several read-only handles may coexist.
    pub fn open_read_only(path: impl AsRef<Path>) -> Result<Db> {
        Db::open_with(
            path,
            DbOptions {
                read_only: true,
                ..DbOptions::default()
            },
        )
    }

    pub fn create_with(path: impl AsRef<Path>, opts: DbOptions) -> Result<Db> {
        opts.auto_compaction.validate()?;
        opts.memory.validate()?;
        let memory_governor = MemoryGovernor::new(opts.memory.limits());
        if opts.read_only {
            return Err(Error::InvalidArgument(
                "cannot create a database in read-only mode".into(),
            ));
        }
        let dir = path.as_ref().to_path_buf();
        let _creation_lock = lock_creation_destination(&dir)?;
        prepare_creation_directory(&dir)?;
        fs::create_dir_all(dir.join(SEGMENTS_DIR))?;
        fs::create_dir_all(dir.join(WAL_DIR))?;
        fs::create_dir_all(dir.join(VECTORS_DIR))?;
        fs::create_dir_all(dir.join(INDEXES_DIR))?;
        fs::create_dir_all(dir.join(BLOBS_DIR))?;
        let lock_file = acquire_lock(&dir, false)?;
        // The pre-lock check is only an early error. A concurrent creator can
        // publish the marker before this process acquires LOCK, so the
        // authoritative existence check must happen while exclusion is held.
        if dir.join(MARKER_FILE).exists() {
            return Err(Error::InvalidArgument(format!(
                "database already exists at {}",
                dir.display()
            )));
        }

        Catalog::new().save(&dir.join(CATALOG_FILE))?;
        let manifest = Manifest::initial();
        if let PublishOutcome::SyncFailed(error) = manifest.publish(&dir)? {
            return Err(Error::CommitUnknown(format!(
                "database manifest was created, but syncing its directory failed: {error}"
            )));
        }
        File::create(wal_path(&dir, manifest.wal_id))?.sync_all()?;
        fsync_dir(&dir.join(WAL_DIR))?;
        // The marker is the creation commit record: publish it only after all
        // mandatory files are durable. A crash before this point leaves an
        // unmarked directory that a later creator may safely initialize; a
        // visible marker always names a complete database.
        let marker_path = dir.join(MARKER_FILE);
        let marker_tmp = dir.join(format!(".{MARKER_FILE}.tmp"));
        let mut marker = File::create(&marker_tmp)?;
        marker.write_all(format!("elitesql format_version={FORMAT_VERSION}\n").as_bytes())?;
        marker.sync_all()?;
        fs::rename(&marker_tmp, &marker_path)?;
        if let Err(error) = fsync_dir(&dir) {
            return Err(Error::CommitUnknown(format!(
                "database marker was renamed into place, but syncing its directory failed: {error}"
            )));
        }
        match fs::remove_file(dir.join(CREATION_FILE)) {
            Ok(()) => {}
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => return Err(error.into()),
        }
        fsync_dir(&dir)?;

        let wal = Some(WalWriter::open(&dir, manifest.wal_id)?);
        let blobs_dir = dir.join(BLOBS_DIR);
        let db = finish_db(Arc::new(Shared {
            dir,
            opts: opts.clone(),
            memory_governor,
            _lock_file: lock_file,
            state: RwLock::new(State {
                catalog: Catalog::new(),
                committed_version: 0,
                identity_high_water: HashMap::new(),
                index: PrimaryIdx::empty(),
                table_high_ids: HashMap::new(),
                superseded_segments: HashSet::new(),
                secondary: HashMap::new(),
                vector: HashMap::new(),
                text: HashMap::new(),
                blobs: blobs_dir,
                readers: HashMap::new(),
                segments: Vec::new(),
                next_segment_id: 1,
            }),
            commit: CommitMutex::new(
                CommitState {
                    wal,
                    memtable_bytes: 0,
                    wal_sync_group: None,
                },
                opts.durability,
            ),
            commit_coordinator: Mutex::new(CommitCoordinatorState::default()),
            canonical_error: Mutex::new(None),
            pending_blob_publications: Mutex::new(HashSet::new()),
            ddl: Mutex::new(()),
            commit_count: AtomicU64::new(0),
            commit_nanos: AtomicU64::new(0),
            commit_lock_wait_nanos: AtomicU64::new(0),
            commit_lock_hold_nanos: AtomicU64::new(0),
            commit_prepare_nanos: AtomicU64::new(0),
            commit_locked_prepare_nanos: AtomicU64::new(0),
            commit_wal_nanos: AtomicU64::new(0),
            commit_wal_append_nanos: AtomicU64::new(0),
            commit_apply_nanos: AtomicU64::new(0),
            commit_waiters: AtomicU64::new(0),
            wal_sync_count: AtomicU64::new(0),
            wal_sync_nanos: AtomicU64::new(0),
            wal_synced_bytes: AtomicU64::new(0),
            wal_sync_max_group_commits: AtomicU64::new(0),
            wal_sync_max_group_bytes: AtomicU64::new(0),
            wal_group_coalesce_nanos: AtomicU64::new(0),
            wal_group_leader_lock_wait_nanos: AtomicU64::new(0),
            grouped_commit_count: AtomicU64::new(0),
            coordinated_batch_count: AtomicU64::new(0),
            coordinated_commit_count: AtomicU64::new(0),
            point_read_parallelism: std::thread::available_parallelism()
                .map_or(1, std::num::NonZeroUsize::get),
            active_state_writers: AtomicU64::new(0),
            safe_coalesce_budget: AtomicU64::new(0),
            admitted_point_reads: AtomicU64::new(0),
            point_read_throttle_count: AtomicU64::new(0),
            snapshots: Mutex::new(BTreeMap::new()),
            last_generated_id: Mutex::new(Ulid::nil()),
            vector_tx: Mutex::new(None),
            vector_backlog: AtomicU64::new(0),
            vector_worker_alive: AtomicBool::new(true),
            vector_worker_error: Mutex::new(None),
            maintenance_tx: Mutex::new(None),
            maintenance_worker_alive: AtomicBool::new(true),
            maintenance_worker_error: Mutex::new(None),
            index_maintenance_failures: AtomicU64::new(0),
            checkpoint_tx: Mutex::new(None),
            derived_tx: Mutex::new(None),
            background_checkpoint: Mutex::new(BackgroundCheckpointState::default()),
            background_checkpoint_done: Condvar::new(),
            background_derived: Mutex::new(BackgroundDerivedState::default()),
            background_derived_done: Condvar::new(),
            derived_manifest_publication: Mutex::new(()),
            derived_publication_count: AtomicU64::new(0),
            derived_publication_nanos: AtomicU64::new(0),
            #[cfg(test)]
            derived_test_pause_before_publish: AtomicBool::new(false),
            #[cfg(test)]
            derived_test_reached_publish: AtomicBool::new(false),
            #[cfg(test)]
            derived_test_fail_before_manifest_rename: AtomicBool::new(false),
            #[cfg(test)]
            derived_test_fail_after_manifest_rename: AtomicBool::new(false),
            #[cfg(test)]
            blob_test_pause_after_publish: AtomicBool::new(false),
            #[cfg(test)]
            blob_test_reached_publish: AtomicBool::new(false),
            #[cfg(test)]
            primary_test_pause_before_manifest: AtomicBool::new(false),
            #[cfg(test)]
            primary_test_reached_manifest: AtomicBool::new(false),
            #[cfg(test)]
            primary_test_fail_before_manifest_rename: AtomicBool::new(false),
            #[cfg(test)]
            primary_test_fail_after_manifest_rename: AtomicBool::new(false),
            primary_compaction_scheduled: AtomicBool::new(false),
            primary_manifest_publication: Mutex::new(()),
            primary_run_compaction_count: AtomicU64::new(0),
            primary_run_compaction_nanos: AtomicU64::new(0),
            primary_run_compaction_bytes_read: AtomicU64::new(0),
            primary_run_compaction_bytes_written: AtomicU64::new(0),
            primary_checkpoint_bytes_written: AtomicU64::new(0),
            secondary_compaction_scheduled: AtomicBool::new(false),
            secondary_run_compaction_count: AtomicU64::new(0),
            secondary_run_compaction_nanos: AtomicU64::new(0),
            secondary_run_compaction_bytes_read: AtomicU64::new(0),
            secondary_run_compaction_bytes_written: AtomicU64::new(0),
            secondary_checkpoint_bytes_written: AtomicU64::new(0),
            text_compaction_scheduled: AtomicBool::new(false),
            text_run_compaction_count: AtomicU64::new(0),
            text_run_compaction_nanos: AtomicU64::new(0),
            text_run_compaction_bytes_read: AtomicU64::new(0),
            text_run_compaction_bytes_written: AtomicU64::new(0),
            text_checkpoint_bytes_written: AtomicU64::new(0),
            auto_compaction_state: Mutex::new(AutoCompactionState::default()),
            checkpoint_count: AtomicU64::new(0),
            checkpoint_nanos: AtomicU64::new(0),
            automatic_compaction_count: AtomicU64::new(0),
            automatic_compaction_nanos: AtomicU64::new(0),
            automatic_compaction_failures: AtomicU64::new(0),
            automatic_compaction_bytes_reclaimed: AtomicU64::new(0),
            compaction_retained_snapshots: Mutex::new(HashSet::new()),
            compaction_refresh_needed: AtomicBool::new(false),
            query_spill_files: AtomicU64::new(0),
            query_spilled_bytes: AtomicU64::new(0),
            query_peak_buffer_bytes: AtomicU64::new(0),
        }));
        refresh_compaction_debt(&db.shared);
        maybe_schedule_auto_compaction(&db.shared);
        maybe_schedule_primary_compaction(&db.shared);
        maybe_schedule_secondary_compaction(&db.shared);
        maybe_schedule_text_compaction(&db.shared);
        Ok(db)
    }

    pub fn open_with(path: impl AsRef<Path>, opts: DbOptions) -> Result<Db> {
        opts.auto_compaction.validate()?;
        opts.memory.validate()?;
        let memory_governor = MemoryGovernor::new(opts.memory.limits());
        let dir = path.as_ref().to_path_buf();
        let ro = opts.read_only;
        if !dir.join(MARKER_FILE).exists() {
            return Err(Error::InvalidArgument(format!(
                "not a elitesql database: {}",
                dir.display()
            )));
        }
        if !ro {
            let _ = fs::remove_file(dir.join(CREATION_FILE));
        }
        let lock_file = acquire_lock(&dir, ro)?;
        let (mut manifest, used_prev) = Manifest::load(&dir)?;
        let catalog = match manifest.catalog.clone() {
            Some(catalog) => catalog,
            None => Catalog::load(&dir.join(CATALOG_FILE))?,
        };
        if used_prev && !ro {
            // The primary manifest was unreadable; re-establish it from the
            // fallback without ever rotating the corrupt file over the good one.
            if let PublishOutcome::SyncFailed(error) = manifest.heal(&dir)? {
                return Err(Error::CommitUnknown(format!(
                    "fallback manifest was restored, but syncing its directory failed: {error}"
                )));
            }
        }
        if manifest.catalog.is_none() && !ro {
            // Upgrade legacy manifests before serving writes. From this point
            // onward both primary and fallback generations carry the schema
            // that describes their segment set.
            manifest.catalog = Some(catalog.clone());
            if let PublishOutcome::SyncFailed(error) = manifest.publish(&dir)? {
                return Err(Error::CommitUnknown(format!(
                    "legacy manifest was upgraded, but syncing its directory failed: {error}"
                )));
            }
        }
        if !ro
            && Manifest::previous(&dir)
                .is_none_or(|previous| previous.encode() != manifest.encode())
        {
            if let PublishOutcome::SyncFailed(error) = manifest.refresh_previous(&dir)? {
                return Err(Error::CommitUnknown(format!(
                    "manifest fallback refresh was incomplete: {error}"
                )));
            }
        }
        if !ro {
            fs::create_dir_all(dir.join(VECTORS_DIR))?;
            fs::create_dir_all(dir.join(INDEXES_DIR))?;
            fs::create_dir_all(dir.join(BLOBS_DIR))?;
            cleanup_blob_staging(&dir);
            cleanup_wal_bridge_staging(&dir);
            cleanup_orphans(&dir, &manifest)?;
            cleanup_orphan_vidx(&dir, &catalog);
            cleanup_orphan_sidx(&dir, &catalog);
            cleanup_orphan_tidx(&dir, &catalog);
        }
        let pending_ddl = if ro { None } else { DdlIntent::load(&dir)? };
        let expected_primary_generation =
            primary_generation(manifest.committed_version, &manifest.segments, &catalog);
        let preloaded_primary = load_primary_runs(&dir, expected_primary_generation)
            .ok()
            .or_else(|| {
                let path = primary_index_path(&dir);
                let bytes = fs::metadata(&path).ok()?.len();
                PagedIndex::open(&path)
                    .ok()
                    .filter(|index| index.dump_version() == expected_primary_generation)
                    .map(|index| {
                        PrimaryIdx::paged_named(
                            index,
                            "primary.pidx".into(),
                            PRIMARY_BASE_LEVEL,
                            bytes,
                            expected_primary_generation,
                        )
                    })
            });
        let mut preloaded_primary_valid = preloaded_primary.is_some();

        // Load segments listed in the manifest. Read-only mode exposes the
        // valid prefix of a damaged segment instead of refusing to open.
        let mut index: HashMap<String, BTreeMap<String, Vec<VersionEntry>>> = HashMap::new();
        let primary_path = primary_index_path(&dir);
        let primary_tmp = primary_path.with_extension("pidx.tmp");
        let temp_dir = opts
            .memory
            .spill_directory
            .clone()
            .unwrap_or_else(|| dir.join(INDEXES_DIR).join("tmp"));
        let mut primary_writer = if preloaded_primary.is_none() && !ro {
            Some(ExternalPagedWriter::new(
                &primary_tmp,
                &temp_dir,
                expected_primary_generation,
                opts.memory.maintenance_pool_bytes,
            )?)
        } else {
            None
        };
        let mut read_only_index_bytes = 0usize;
        let mut readers = HashMap::new();
        for meta in &manifest.segments {
            let seg_path = dir.join(SEGMENTS_DIR).join(segment_file_name(meta.id));
            let file = match File::open(&seg_path) {
                Ok(file) => file,
                Err(e) if ro => {
                    let _ = e;
                    preloaded_primary_valid = false;
                    continue; // missing segment: expose what the rest holds
                }
                Err(e) => return Err(e.into()),
            };
            let file_len = file.metadata()?.len();
            if file_len < meta.len && !ro {
                return Err(Error::Corrupt(format!(
                    "segment {} shorter than manifest ({} < {})",
                    segment_file_name(meta.id),
                    file_len,
                    meta.len
                )));
            }
            // SAFETY: segments are immutable while referenced by the manifest;
            // compaction publishes new inodes and drops these mappings first.
            let data = unsafe { MmapOptions::new().map(&file) }?;
            let _ = data.advise(Advice::Sequential);
            let valid = &data[..meta.len.min(file_len) as usize];
            let outcome = if preloaded_primary.is_some() {
                validate_segment(valid)
            } else {
                visit_segment(valid, |entry| {
                    if pending_ddl.is_none() {
                        let Some(schema) = catalog.table(&entry.table) else {
                            return Ok(());
                        };
                        if schema.epoch != 0 && entry.version <= schema.epoch {
                            return Ok(());
                        }
                    }
                    let version = VersionEntry {
                        version: entry.version,
                        kind: if entry.tombstone {
                            VKind::SegTombstone
                        } else {
                            VKind::SegPut {
                                segment: meta.id,
                                payload_offset: entry.payload_offset,
                                payload_len: entry.payload_len,
                            }
                        },
                    };
                    if let Some(writer) = primary_writer.as_mut() {
                        writer.add(
                            &primary_key(&entry.table, &entry.id),
                            &encode_primary_entry(&version)?,
                        )?;
                    } else {
                        read_only_index_bytes = read_only_index_bytes
                            .saturating_add(entry.table.len())
                            .saturating_add(entry.id.len())
                            .saturating_add(160);
                        if read_only_index_bytes > opts.memory.index_delta_pool_bytes {
                            return Err(Error::MemoryLimit(format!(
                                "read-only recovery needs more than {} bytes for a missing/stale primary index; open writable once to rebuild its mmap index or raise memory.index_delta_pool_bytes",
                                opts.memory.index_delta_pool_bytes
                            )));
                        }
                        index
                            .entry(entry.table)
                            .or_default()
                            .entry(entry.id)
                            .or_default()
                            .push(version);
                    }
                    Ok(())
                })?
            };
            if (!outcome.clean || outcome.valid_len != meta.len) && !ro {
                return Err(Error::Corrupt(format!(
                    "segment {} failed validation",
                    segment_file_name(meta.id)
                )));
            }
            if !outcome.clean || outcome.valid_len != meta.len {
                preloaded_primary_valid = false;
            }
            readers.insert(meta.id, Arc::new(file));
        }
        if ro && preloaded_primary.is_some() && !preloaded_primary_valid {
            // The persisted directory may reference bytes beyond a damaged
            // segment's valid prefix. Reconstruct only that validated prefix;
            // healthy read-only opens keep the zero-copy mmap fast path.
            index.clear();
            read_only_index_bytes = 0;
            for meta in &manifest.segments {
                let Some(file) = readers.get(&meta.id) else {
                    continue;
                };
                let file_len = file.metadata()?.len();
                let data = unsafe { MmapOptions::new().map(file) }?;
                let valid = &data[..meta.len.min(file_len) as usize];
                visit_segment(valid, |entry| {
                    let Some(schema) = catalog.table(&entry.table) else {
                        return Ok(());
                    };
                    if schema.epoch != 0 && entry.version <= schema.epoch {
                        return Ok(());
                    }
                    read_only_index_bytes = read_only_index_bytes
                        .saturating_add(entry.table.len())
                        .saturating_add(entry.id.len())
                        .saturating_add(160);
                    if read_only_index_bytes > opts.memory.index_delta_pool_bytes {
                        return Err(Error::MemoryLimit(format!(
                            "read-only recovery needs more than {} bytes for a damaged primary index; open writable from a repaired copy or raise memory.index_delta_pool_bytes",
                            opts.memory.index_delta_pool_bytes
                        )));
                    }
                    index
                        .entry(entry.table)
                        .or_default()
                        .entry(entry.id)
                        .or_default()
                        .push(VersionEntry {
                            version: entry.version,
                            kind: if entry.tombstone {
                                VKind::SegTombstone
                            } else {
                                VKind::SegPut {
                                    segment: meta.id,
                                    payload_offset: entry.payload_offset,
                                    payload_len: entry.payload_len,
                                }
                            },
                        });
                    Ok(())
                })?;
            }
        }
        let mut primary = if preloaded_primary_valid {
            preloaded_primary.expect("validated preloaded primary")
        } else if ro {
            PrimaryIdx::resident(index)
        } else {
            let writer = primary_writer
                .take()
                .expect("writable rebuild has a primary writer");
            if let Err(error) = writer.finish() {
                let _ = fs::remove_file(&primary_tmp);
                return Err(error);
            }
            fs::rename(&primary_tmp, &primary_path)?;
            fsync_dir(&dir.join(INDEXES_DIR))?;
            let rebuilt = PrimaryIdx::paged_named(
                PagedIndex::open(&primary_path)?,
                "primary.pidx".into(),
                PRIMARY_BASE_LEVEL,
                fs::metadata(&primary_path)?.len(),
                expected_primary_generation,
            );
            publish_primary_run_manifest(&dir, expected_primary_generation, &rebuilt)?;
            rebuilt
        };

        if !ro
            && PrimaryRunManifest::load(&dir.join(INDEXES_DIR), expected_primary_generation)
                .is_err()
        {
            publish_primary_run_manifest(&dir, expected_primary_generation, &primary)?;
        }
        if !ro {
            cleanup_primary_run_orphans(&dir);
        }

        // Replay the WAL chain idempotently. Checkpoint publication may rotate
        // writers before its manifest becomes visible, so consecutive WALs
        // after the manifest's anchor are part of the same recoverable log.
        // Bridge WALs may duplicate records already present in the anchor;
        // versions at/below the recovered watermark are skipped.
        let mut committed_version = manifest.committed_version;
        let mut identity_high_water: HashMap<String, i64> = manifest
            .identity_high_water
            .iter()
            .map(|(table, value)| (table.clone(), *value))
            .collect();
        let mut memtable_bytes = 0u64;
        let anchor_wal = wal_path(&dir, manifest.wal_id);
        if !anchor_wal.exists() {
            return Err(Error::Corrupt(format!(
                "active WAL {} is missing; it may have contained commits above manifest version {}",
                manifest.wal_id, manifest.committed_version
            )));
        }
        let mut active_wal_id = manifest.wal_id;
        let mut replay_wal_id = manifest.wal_id;
        loop {
            let wal_file = wal_path(&dir, replay_wal_id);
            if !wal_file.exists() {
                break;
            }
            let data = match fs::read(&wal_file) {
                Ok(data) => data,
                Err(_) if ro => Vec::new(),
                Err(error) => return Err(error.into()),
            };
            let scan = scan_wal(&data);
            let clean = scan.clean;
            if !clean && !ro {
                let file = OpenOptions::new().write(true).open(&wal_file)?;
                file.set_len(scan.valid_len)?;
                file.sync_all()?;
            }
            for rec in scan.records {
                if rec.version <= committed_version {
                    continue;
                }
                let expected = committed_version.checked_add(1).ok_or_else(|| {
                    Error::Corrupt("commit version exhausted while replaying WAL".into())
                })?;
                if rec.version != expected {
                    return Err(Error::Corrupt(format!(
                        "WAL commit version gap: expected {expected}, found {}",
                        rec.version
                    )));
                }
                for (table, value) in rec.identity_high_water {
                    identity_high_water
                        .entry(table)
                        .and_modify(|high| *high = (*high).max(value))
                        .or_insert(value);
                }
                for ch in rec.changes {
                    let kind = match ch.payload {
                        Some(payload) => {
                            memtable_bytes += payload.len() as u64;
                            VKind::MemPut(Arc::new(payload))
                        }
                        None => VKind::MemTombstone,
                    };
                    primary.push(
                        &ch.table,
                        ch.id,
                        VersionEntry {
                            version: rec.version,
                            kind,
                        },
                    );
                }
                committed_version = rec.version;
            }
            active_wal_id = replay_wal_id;
            if !clean {
                break;
            }
            let Some(next) = replay_wal_id.checked_add(1) else {
                break;
            };
            replay_wal_id = next;
        }

        // A DDL operation interrupted by a crash is finished below, once the
        // handle exists. Until then nothing may be pruned: the half that did
        // land may be data written under a name the catalog does not know yet.
        let blobs_dir_open = dir.join(BLOBS_DIR);
        let (secondary, vector, text) = if pending_ddl.is_some() {
            // Derived indexes are rebuilt by the DDL replay; building them
            // twice would only waste the open.
            (HashMap::new(), HashMap::new(), HashMap::new())
        } else {
            (
                load_or_build_secondary_indexes(
                    &dir,
                    &blobs_dir_open,
                    &catalog,
                    &primary,
                    &readers,
                    committed_version,
                    ro,
                    &opts.memory,
                )?,
                load_or_build_vector_indexes(
                    &dir,
                    &blobs_dir_open,
                    &catalog,
                    &primary,
                    &readers,
                    committed_version,
                    ro,
                    &opts.memory,
                )?,
                load_or_build_text_indexes(
                    &dir,
                    &blobs_dir_open,
                    &catalog,
                    &primary,
                    &readers,
                    committed_version,
                    ro,
                    &opts.memory,
                )?,
            )
        };
        if !ro {
            cleanup_orphan_sidx(&dir, &catalog);
            cleanup_orphan_tidx(&dir, &catalog);
        }
        let retained_index_bytes = primary
            .delta_memory_bytes()
            .saturating_add(
                secondary
                    .values()
                    .map(SecIdx::delta_memory_bytes)
                    .sum::<usize>(),
            )
            .saturating_add(
                vector
                    .values()
                    .map(VecIdx::delta_memory_bytes)
                    .sum::<usize>(),
            )
            .saturating_add(
                text.values()
                    .map(TextIdx::delta_memory_bytes)
                    .sum::<usize>(),
            );
        if retained_index_bytes > opts.memory.index_delta_pool_bytes {
            return Err(Error::MemoryLimit(format!(
                "opening retained an estimated {retained_index_bytes} index-delta bytes, exceeding memory.index_delta_pool_bytes ({}); open writable with a larger pool to consolidate recovery state",
                opts.memory.index_delta_pool_bytes
            )));
        }
        let next_segment_id = manifest.segments.iter().map(|s| s.id).max().unwrap_or(0) + 1;
        let wal = if ro {
            None
        } else {
            Some(WalWriter::open(&dir, active_wal_id)?)
        };
        let blobs_dir = dir.join(BLOBS_DIR);
        let mut last_generated_id = Ulid::nil();
        let mut table_high_ids = HashMap::new();
        let mut superseded_segments = HashSet::new();
        for table in &catalog.tables {
            primary.visit_table(&table.name, None, |id, versions| {
                let latest = versions
                    .iter()
                    .rev()
                    .find(|entry| entry.version > table.epoch);
                if let Some(latest) = latest {
                    table_high_ids.insert(table.name.clone(), id.to_owned());
                    for entry in versions.iter().filter(|entry| entry.version > table.epoch) {
                        if entry.version != latest.version {
                            if let VKind::SegPut { segment, .. } = &entry.kind {
                                superseded_segments.insert(*segment);
                            }
                        }
                    }
                }
                if let Ok(parsed) = Ulid::from_string(id) {
                    last_generated_id = last_generated_id.max(parsed);
                }
                Ok(true)
            })?;
        }
        let db = finish_db(Arc::new(Shared {
            dir,
            opts: opts.clone(),
            memory_governor,
            _lock_file: lock_file,
            state: RwLock::new(State {
                catalog,
                committed_version,
                identity_high_water,
                index: primary,
                table_high_ids,
                superseded_segments,
                secondary,
                vector,
                text,
                blobs: blobs_dir,
                readers,
                segments: manifest.segments,
                next_segment_id,
            }),
            commit: CommitMutex::new(
                CommitState {
                    wal,
                    memtable_bytes,
                    wal_sync_group: None,
                },
                opts.durability,
            ),
            commit_coordinator: Mutex::new(CommitCoordinatorState::default()),
            canonical_error: Mutex::new(None),
            pending_blob_publications: Mutex::new(HashSet::new()),
            ddl: Mutex::new(()),
            commit_count: AtomicU64::new(0),
            commit_nanos: AtomicU64::new(0),
            commit_lock_wait_nanos: AtomicU64::new(0),
            commit_lock_hold_nanos: AtomicU64::new(0),
            commit_prepare_nanos: AtomicU64::new(0),
            commit_locked_prepare_nanos: AtomicU64::new(0),
            commit_wal_nanos: AtomicU64::new(0),
            commit_wal_append_nanos: AtomicU64::new(0),
            commit_apply_nanos: AtomicU64::new(0),
            commit_waiters: AtomicU64::new(0),
            wal_sync_count: AtomicU64::new(0),
            wal_sync_nanos: AtomicU64::new(0),
            wal_synced_bytes: AtomicU64::new(0),
            wal_sync_max_group_commits: AtomicU64::new(0),
            wal_sync_max_group_bytes: AtomicU64::new(0),
            wal_group_coalesce_nanos: AtomicU64::new(0),
            wal_group_leader_lock_wait_nanos: AtomicU64::new(0),
            grouped_commit_count: AtomicU64::new(0),
            coordinated_batch_count: AtomicU64::new(0),
            coordinated_commit_count: AtomicU64::new(0),
            point_read_parallelism: std::thread::available_parallelism()
                .map_or(1, std::num::NonZeroUsize::get),
            active_state_writers: AtomicU64::new(0),
            safe_coalesce_budget: AtomicU64::new(0),
            admitted_point_reads: AtomicU64::new(0),
            point_read_throttle_count: AtomicU64::new(0),
            snapshots: Mutex::new(BTreeMap::new()),
            last_generated_id: Mutex::new(last_generated_id),
            vector_tx: Mutex::new(None),
            vector_backlog: AtomicU64::new(0),
            vector_worker_alive: AtomicBool::new(true),
            vector_worker_error: Mutex::new(None),
            maintenance_tx: Mutex::new(None),
            maintenance_worker_alive: AtomicBool::new(true),
            maintenance_worker_error: Mutex::new(None),
            index_maintenance_failures: AtomicU64::new(0),
            checkpoint_tx: Mutex::new(None),
            derived_tx: Mutex::new(None),
            background_checkpoint: Mutex::new(BackgroundCheckpointState::default()),
            background_checkpoint_done: Condvar::new(),
            background_derived: Mutex::new(BackgroundDerivedState::default()),
            background_derived_done: Condvar::new(),
            derived_manifest_publication: Mutex::new(()),
            derived_publication_count: AtomicU64::new(0),
            derived_publication_nanos: AtomicU64::new(0),
            #[cfg(test)]
            derived_test_pause_before_publish: AtomicBool::new(false),
            #[cfg(test)]
            derived_test_reached_publish: AtomicBool::new(false),
            #[cfg(test)]
            derived_test_fail_before_manifest_rename: AtomicBool::new(false),
            #[cfg(test)]
            derived_test_fail_after_manifest_rename: AtomicBool::new(false),
            #[cfg(test)]
            blob_test_pause_after_publish: AtomicBool::new(false),
            #[cfg(test)]
            blob_test_reached_publish: AtomicBool::new(false),
            #[cfg(test)]
            primary_test_pause_before_manifest: AtomicBool::new(false),
            #[cfg(test)]
            primary_test_reached_manifest: AtomicBool::new(false),
            #[cfg(test)]
            primary_test_fail_before_manifest_rename: AtomicBool::new(false),
            #[cfg(test)]
            primary_test_fail_after_manifest_rename: AtomicBool::new(false),
            primary_compaction_scheduled: AtomicBool::new(false),
            primary_manifest_publication: Mutex::new(()),
            primary_run_compaction_count: AtomicU64::new(0),
            primary_run_compaction_nanos: AtomicU64::new(0),
            primary_run_compaction_bytes_read: AtomicU64::new(0),
            primary_run_compaction_bytes_written: AtomicU64::new(0),
            primary_checkpoint_bytes_written: AtomicU64::new(0),
            secondary_compaction_scheduled: AtomicBool::new(false),
            secondary_run_compaction_count: AtomicU64::new(0),
            secondary_run_compaction_nanos: AtomicU64::new(0),
            secondary_run_compaction_bytes_read: AtomicU64::new(0),
            secondary_run_compaction_bytes_written: AtomicU64::new(0),
            secondary_checkpoint_bytes_written: AtomicU64::new(0),
            text_compaction_scheduled: AtomicBool::new(false),
            text_run_compaction_count: AtomicU64::new(0),
            text_run_compaction_nanos: AtomicU64::new(0),
            text_run_compaction_bytes_read: AtomicU64::new(0),
            text_run_compaction_bytes_written: AtomicU64::new(0),
            text_checkpoint_bytes_written: AtomicU64::new(0),
            auto_compaction_state: Mutex::new(AutoCompactionState::default()),
            checkpoint_count: AtomicU64::new(0),
            checkpoint_nanos: AtomicU64::new(0),
            automatic_compaction_count: AtomicU64::new(0),
            automatic_compaction_nanos: AtomicU64::new(0),
            automatic_compaction_failures: AtomicU64::new(0),
            automatic_compaction_bytes_reclaimed: AtomicU64::new(0),
            compaction_retained_snapshots: Mutex::new(HashSet::new()),
            compaction_refresh_needed: AtomicBool::new(false),
            query_spill_files: AtomicU64::new(0),
            query_spilled_bytes: AtomicU64::new(0),
            query_peak_buffer_bytes: AtomicU64::new(0),
        }));
        if let Some(intent) = pending_ddl {
            db.apply_ddl(&intent)?;
            DdlIntent::clear(&db.shared.dir)?;
            db.finish_ddl_recovery()?;
        }
        refresh_compaction_debt(&db.shared);
        maybe_schedule_auto_compaction(&db.shared);
        maybe_schedule_primary_compaction(&db.shared);
        maybe_schedule_secondary_compaction(&db.shared);
        maybe_schedule_text_compaction(&db.shared);
        Ok(db)
    }

    // --- schema ------------------------------------------------------------

    pub fn create_table(&self, mut schema: TableSchema) -> Result<()> {
        if self.shared.opts.read_only {
            return Err(Error::ReadOnly);
        }
        let _ddl = self.shared.ddl.lock().unwrap();
        schema.validate()?;
        if let Some(identity_name) = schema
            .columns
            .iter()
            .find(|column| column.identity)
            .map(|column| column.name.clone())
        {
            match schema
                .indexes
                .iter_mut()
                .find(|index| index.column == identity_name)
            {
                Some(index) => index.unique = true,
                None => schema.indexes.push(IndexDef {
                    column: identity_name,
                    unique: true,
                }),
            }
        }
        // A local equality index is part of the FK contract. It bounds parent
        // delete checks and cascade discovery without requiring callers to
        // remember a second DDL statement.
        for foreign_key in &schema.foreign_keys {
            if !schema
                .indexes
                .iter()
                .any(|index| index.column == foreign_key.column)
            {
                schema.indexes.push(IndexDef {
                    column: foreign_key.column.clone(),
                    unique: false,
                });
            }
        }
        let mut cs = lock_commit_for_maintenance(&self.shared)?;
        let mut st = self.shared.state.write().unwrap();
        // This incarnation of the table only owns data committed from now on.
        // Anything older under the same name belongs to a table that was
        // dropped and is filtered while rebuilding the primary mmap index.
        schema.epoch = st.committed_version;
        if st.catalog.table(&schema.name).is_some() {
            return Err(Error::TableExists(schema.name));
        }
        for foreign_key in &schema.foreign_keys {
            let target = if foreign_key.referenced_table == schema.name {
                &schema
            } else {
                st.catalog
                    .table(&foreign_key.referenced_table)
                    .ok_or_else(|| {
                        Error::SchemaViolation(format!(
                            "foreign key {}.{} references unknown table '{}'",
                            schema.name, foreign_key.column, foreign_key.referenced_table
                        ))
                    })?
            };
            let local_type = schema
                .column(&foreign_key.column)
                .expect("schema validation checked the local column")
                .ty;
            let referenced_type =
                if foreign_key.referenced_column == ID_COLUMN && target.has_implicit_id() {
                    ColumnType::Text
                } else {
                    target
                        .column(&foreign_key.referenced_column)
                        .ok_or_else(|| {
                            Error::SchemaViolation(format!(
                                "foreign key {}.{} references unknown column {}.{}",
                                schema.name,
                                foreign_key.column,
                                foreign_key.referenced_table,
                                foreign_key.referenced_column
                            ))
                        })?
                        .ty
                };
            if local_type != referenced_type {
                return Err(Error::SchemaViolation(format!(
                    "foreign key {}.{} ({local_type}) does not match {}.{} ({referenced_type})",
                    schema.name,
                    foreign_key.column,
                    foreign_key.referenced_table,
                    foreign_key.referenced_column
                )));
            }
            let implicit_primary =
                foreign_key.referenced_column == ID_COLUMN && target.has_implicit_id();
            let unique_index = target
                .indexes
                .iter()
                .any(|index| index.column == foreign_key.referenced_column && index.unique);
            if !implicit_primary && !unique_index {
                return Err(Error::SchemaViolation(format!(
                    "foreign key target {}.{} must have a unique index",
                    foreign_key.referenced_table, foreign_key.referenced_column
                )));
            }
        }
        for def in &schema.indexes {
            if schema.column(&def.column).is_none() {
                return Err(Error::SchemaViolation(format!(
                    "index over unknown column '{}'",
                    def.column
                )));
            }
        }
        for def in &schema.vector_indexes {
            match schema.column(&def.column) {
                Some(col) if col.ty == ColumnType::Vector => {}
                _ => {
                    return Err(Error::SchemaViolation(format!(
                        "vector index over non-vector column '{}'",
                        def.column
                    )))
                }
            }
        }
        for def in &schema.text_indexes {
            match schema.column(&def.column) {
                Some(col) if col.ty == ColumnType::Text => {}
                _ => {
                    return Err(Error::SchemaViolation(format!(
                        "text index over non-text column '{}'",
                        def.column
                    )))
                }
            }
        }
        let mut next_catalog = st.catalog.clone();
        next_catalog.tables.push(schema.clone());
        let publication_error =
            publish_catalog_generation_locked(&self.shared, &mut cs, &mut st, next_catalog)?;
        for def in &schema.indexes {
            st.secondary.insert(
                (schema.name.clone(), def.column.clone()),
                SecIdx::resident(HashMap::new()),
            );
        }
        for def in &schema.vector_indexes {
            st.vector.insert(
                (schema.name.clone(), def.column.clone()),
                VecIdx::new(def.clone()),
            );
        }
        for def in &schema.text_indexes {
            st.text
                .insert((schema.name.clone(), def.column.clone()), TextIdx::new());
        }
        publication_error.map_or(Ok(()), Err)
    }

    /// Create a secondary (optionally unique) equality index over a column,
    /// built from the current committed state.
    pub fn create_index(&self, table: &str, column: &str, unique: bool) -> Result<()> {
        if self.shared.opts.read_only {
            return Err(Error::ReadOnly);
        }
        let _ddl = self.shared.ddl.lock().unwrap();
        let mut cs = lock_commit_for_maintenance(&self.shared)?;
        let _memory = Self::acquire_maintenance_memory(&self.shared);
        let mut st = self.shared.state.write().unwrap();
        {
            let schema = st
                .catalog
                .table(table)
                .ok_or_else(|| Error::TableNotFound(table.into()))?;
            let Some(col) = schema.column(column) else {
                return Err(Error::SchemaViolation(format!("unknown column '{column}'")));
            };
            if col.ty == ColumnType::Vector {
                return Err(Error::SchemaViolation(format!(
                    "{table}.{column} is a vector column; use create_vector_index"
                )));
            }
            if schema.indexes.iter().any(|d| d.column == column) {
                return Err(Error::InvalidArgument(format!(
                    "index on {table}.{column} already exists"
                )));
            }
        }
        let path = sidx_path(&self.shared.dir, table, column);
        let tmp = path.with_extension("sidx.tmp");
        let temp_dir = self
            .shared
            .opts
            .memory
            .spill_directory
            .clone()
            .unwrap_or_else(|| self.shared.dir.join(INDEXES_DIR).join("tmp"));
        if let Err(error) = write_secondary_from_canonical(
            &tmp,
            &temp_dir,
            st.committed_version,
            self.shared.opts.memory.maintenance_pool_bytes,
            &st.blobs,
            table,
            column,
            &st.index,
            &st.readers,
        ) {
            let _ = fs::remove_file(&tmp);
            return Err(error);
        }
        if unique {
            let built = PagedIndex::open(&tmp)?;
            let mut previous_key: Option<Vec<u8>> = None;
            let unique_result = built.scan(|pair, _operation| {
                if pair == SECONDARY_FORMAT_KEY {
                    return Ok(());
                }
                let (key, _id) = secondary_pair_parts(pair)?;
                if previous_key.as_deref() == Some(key) {
                    return Err(Error::UniqueViolation {
                        table: table.into(),
                        column: column.into(),
                    });
                }
                previous_key = Some(key.to_vec());
                Ok(())
            });
            drop(built);
            if let Err(error) = unique_result {
                let _ = fs::remove_file(&tmp);
                return Err(error);
            }
        }
        let mut next_catalog = st.catalog.clone();
        next_catalog
            .table_mut(table)
            .expect("checked above")
            .indexes
            .push(IndexDef {
                column: column.into(),
                unique,
            });
        let publication_error =
            match publish_catalog_generation_locked(&self.shared, &mut cs, &mut st, next_catalog) {
                Ok(error) => error,
                Err(error) => {
                    let _ = fs::remove_file(&tmp);
                    return Err(error);
                }
            };
        let post_publish = (|| -> Result<()> {
            fs::rename(&tmp, &path)?;
            fsync_dir(&self.shared.dir.join(INDEXES_DIR))?;
            let meta = DerivedRunMeta {
                file: path
                    .file_name()
                    .and_then(|name| name.to_str())
                    .expect("secondary path has utf8 filename")
                    .to_owned(),
                level: DERIVED_BASE_LEVEL,
                bytes: fs::metadata(&path)?.len(),
                generation: st.committed_version,
            };
            DerivedRunManifest::new(
                DerivedRunKind::Secondary,
                table,
                column,
                st.committed_version,
                vec![meta.clone()],
                [0, 0],
            )
            .publish(&sidx_manifest_path(&self.shared.dir, table, column))?;
            let version = st.committed_version;
            st.secondary.insert(
                (table.into(), column.into()),
                SecIdx::paged_runs(
                    version,
                    vec![SecRun {
                        meta,
                        index: Arc::new(PagedIndex::open(&path)?),
                    }],
                )?,
            );
            Ok(())
        })();
        if let Err(error) = post_publish {
            fence_writes(
                &self.shared,
                format!("catalog published for {table}.{column}, but secondary index setup failed: {error}"),
            );
            return Err(error);
        }
        publication_error.map_or(Ok(()), Err)
    }

    /// Create an ANN (HNSW) index over a vector column, built from the
    /// current committed state. The index is a derived structure: it is
    /// rebuilt from canonical data on open and on compaction.
    pub fn create_vector_index(
        &self,
        table: &str,
        column: &str,
        opts: VectorIndexOptions,
    ) -> Result<()> {
        if self.shared.opts.read_only {
            return Err(Error::ReadOnly);
        }
        let _ddl = self.shared.ddl.lock().unwrap();
        let mut cs = lock_commit_for_maintenance(&self.shared)?;
        let _memory = Self::acquire_maintenance_memory(&self.shared);
        let mut st = self.shared.state.write().unwrap();
        {
            let schema = st
                .catalog
                .table(table)
                .ok_or_else(|| Error::TableNotFound(table.into()))?;
            let Some(col) = schema.column(column) else {
                return Err(Error::SchemaViolation(format!("unknown column '{column}'")));
            };
            if col.ty != ColumnType::Vector {
                return Err(Error::SchemaViolation(format!(
                    "{table}.{column} is not a vector column"
                )));
            }
            if schema.vector_indexes.iter().any(|d| d.column == column) {
                return Err(Error::InvalidArgument(format!(
                    "vector index on {table}.{column} already exists"
                )));
            }
            if opts.m == 0 || opts.m > 256 {
                return Err(Error::InvalidArgument("m must be between 1 and 256".into()));
            }
        }
        let def = VectorIndexDef {
            column: column.into(),
            metric: opts.metric,
            m: opts.m,
            ef_construction: opts.ef_construction,
            mode: opts.mode,
            quantized: opts.quantized,
        };
        let mut vidx = VecIdx::new(def.clone());
        st.index.visit_table(table, None, |id, versions| {
            if let Some(last) = versions.last() {
                if !last.is_tombstone() {
                    let rec = read_record_kind(&st.blobs, &st.readers, &last.kind)?;
                    if let Some(Value::Vector(v)) = rec.get(column) {
                        vidx.insert(id, v);
                        if vidx.delta_memory_bytes()
                            > self.shared.opts.memory.maintenance_pool_bytes
                        {
                            return Err(Error::MemoryLimit(format!(
                                "building vector index {table}.{column} exceeds memory.maintenance_pool_bytes"
                            )));
                        }
                    }
                }
            }
            Ok(true)
        })?;
        let mut next_catalog = st.catalog.clone();
        next_catalog
            .table_mut(table)
            .expect("checked above")
            .vector_indexes
            .push(def);
        let publication_error =
            publish_catalog_generation_locked(&self.shared, &mut cs, &mut st, next_catalog)?;
        let def = st
            .catalog
            .table(table)
            .and_then(|schema| {
                schema
                    .vector_indexes
                    .iter()
                    .find(|def| def.column == column)
            })
            .expect("vector definition just inserted")
            .clone();
        if vidx.total_len() == 0 {
            // Keep a brand-new empty graph mutable so subsequent inserts are
            // included in its first durable base at close/consolidation.
            st.vector.insert((table.into(), column.into()), vidx);
            return publication_error.map_or(Ok(()), Err);
        }
        let post_publish = (|| -> Result<()> {
            let path = vidx_path(&self.shared.dir, table, column);
            let tmp = path.with_extension("vidx.tmp");
            vidx.dump_file(&tmp, table, column, &def, st.committed_version)?;
            fs::rename(&tmp, &path)?;
            fsync_dir(&self.shared.dir.join(VECTORS_DIR))?;
            let file = File::open(&path)?;
            let mmap = unsafe { MmapOptions::new().map(&file) }?;
            let (mapped, _) = VecIdx::load_mmap(mmap, table, column, &def)?;
            st.vector.insert((table.into(), column.into()), mapped);
            Ok(())
        })();
        if let Err(error) = post_publish {
            fence_writes(
                &self.shared,
                format!("catalog published for {table}.{column}, but vector index setup failed: {error}"),
            );
            return Err(error);
        }
        publication_error.map_or(Ok(()), Err)
    }

    /// Create a basic full-text index (inverted index + BM25) over a text
    /// column, built from the current committed state.
    pub fn create_text_index(&self, table: &str, column: &str) -> Result<()> {
        if self.shared.opts.read_only {
            return Err(Error::ReadOnly);
        }
        let _ddl = self.shared.ddl.lock().unwrap();
        let mut cs = lock_commit_for_maintenance(&self.shared)?;
        let _memory = Self::acquire_maintenance_memory(&self.shared);
        let mut st = self.shared.state.write().unwrap();
        {
            let schema = st
                .catalog
                .table(table)
                .ok_or_else(|| Error::TableNotFound(table.into()))?;
            let Some(col) = schema.column(column) else {
                return Err(Error::SchemaViolation(format!("unknown column '{column}'")));
            };
            if col.ty != ColumnType::Text {
                return Err(Error::SchemaViolation(format!(
                    "{table}.{column} is not a text column"
                )));
            }
            if schema.text_indexes.iter().any(|d| d.column == column) {
                return Err(Error::InvalidArgument(format!(
                    "text index on {table}.{column} already exists"
                )));
            }
        }
        let path = tidx_path(&self.shared.dir, table, column);
        let tmp = path.with_extension("tidx.tmp");
        let temp_dir = self
            .shared
            .opts
            .memory
            .spill_directory
            .clone()
            .unwrap_or_else(|| self.shared.dir.join(INDEXES_DIR).join("tmp"));
        let version = st.committed_version;
        let (doc_count, total_len) = match write_text_from_canonical(
            &tmp,
            &temp_dir,
            version,
            self.shared.opts.memory.maintenance_pool_bytes,
            &st.blobs,
            table,
            column,
            &st.index,
            &st.readers,
        ) {
            Ok(stats) => stats,
            Err(error) => {
                let _ = fs::remove_file(&tmp);
                return Err(error);
            }
        };
        let mut next_catalog = st.catalog.clone();
        next_catalog
            .table_mut(table)
            .expect("checked above")
            .text_indexes
            .push(TextIndexDef {
                column: column.into(),
            });
        let publication_error =
            match publish_catalog_generation_locked(&self.shared, &mut cs, &mut st, next_catalog) {
                Ok(error) => error,
                Err(error) => {
                    let _ = fs::remove_file(&tmp);
                    return Err(error);
                }
            };
        let post_publish = (|| -> Result<()> {
            fs::rename(&tmp, &path)?;
            fsync_dir(&self.shared.dir.join(INDEXES_DIR))?;
            let meta = DerivedRunMeta {
                file: path
                    .file_name()
                    .and_then(|name| name.to_str())
                    .expect("text path has utf8 filename")
                    .to_owned(),
                level: DERIVED_BASE_LEVEL,
                bytes: fs::metadata(&path)?.len(),
                generation: version,
            };
            let text = TextIdx::paged_runs(
                version,
                vec![TextRun {
                    meta,
                    index: Arc::new(PagedIndex::open(&path)?),
                }],
                doc_count,
                total_len,
            )?;
            publish_text_manifest(&self.shared.dir, table, column, version, &text)?;
            st.text.insert((table.into(), column.into()), text);
            Ok(())
        })();
        if let Err(error) = post_publish {
            fence_writes(
                &self.shared,
                format!(
                    "catalog published for {table}.{column}, but text index setup failed: {error}"
                ),
            );
            return Err(error);
        }
        publication_error.map_or(Ok(()), Err)
    }

    // --- DDL: DROP and ALTER -------------------------------------------------
    //
    // The catalog is the authority on what exists. Unlinking a table or an
    // index from it is therefore a complete, durable operation on its own, and
    // costs the same whether the table holds one record or a billion: the
    // records left in existing segments become unreachable (`open` ignores
    // entries whose table is not in the catalog, or that predate the current
    // incarnation of a re-created one) and their space is reclaimed by the
    // next compaction.
    //
    // Changes that must also touch data — renaming a table or column, dropping
    // a column, backfilling a new column's default — rewrite it, and are made
    // crash-safe by a `ddl.json` record replayed on the next open (see
    // `crate::ddl`).

    /// Drop a table with every index and record in it. Immediate and durable;
    /// the disk space is returned by the next [`Db::compact`].
    ///
    /// DDL is not transactional: the drop is visible at once to every reader,
    /// including open snapshots and in-flight transactions (whose commits then
    /// fail with [`Error::TableNotFound`]).
    pub fn drop_table(&self, table: &str) -> Result<()> {
        if self.shared.opts.read_only {
            return Err(Error::ReadOnly);
        }
        let _ddl = self.shared.ddl.lock().unwrap();
        let mut cs = lock_commit_for_maintenance(&self.shared)?;
        let mut st = self.shared.state.write().unwrap();
        let schema = st
            .catalog
            .table(table)
            .ok_or_else(|| Error::TableNotFound(table.into()))?;
        if let Some((child, foreign_key)) = st.catalog.tables.iter().find_map(|child| {
            if child.name == table {
                return None;
            }
            child
                .foreign_keys
                .iter()
                .find(|foreign_key| foreign_key.referenced_table == table)
                .map(|foreign_key| (child, foreign_key))
        }) {
            return Err(Error::SchemaViolation(format!(
                "cannot drop table '{table}': referenced by {}.{}",
                child.name, foreign_key.column
            )));
        }
        let vector_columns: Vec<String> = schema
            .vector_indexes
            .iter()
            .map(|d| d.column.clone())
            .collect();
        let mut dropped_rows = 0u64;
        st.index.visit_table(table, None, |_id, versions| {
            if versions.last().is_some_and(|entry| !entry.is_tombstone()) {
                dropped_rows = dropped_rows.saturating_add(1);
            }
            Ok(true)
        })?;
        let mut next = st.catalog.clone();
        next.tables.retain(|t| t.name != table);
        let publication_error =
            publish_catalog_generation_locked(&self.shared, &mut cs, &mut st, next)?;
        st.identity_high_water.remove(table);
        // Immutable primary pages may keep unreachable keys until compaction;
        // dropping only the bounded mutable delta avoids materializing the
        // complete mmap-backed primary index in RAM.
        st.index.remove_delta_table(table);
        st.secondary.retain(|key, _| key.0 != table);
        st.vector.retain(|key, _| key.0 != table);
        st.text.retain(|key, _| key.0 != table);
        let cleanup_catalog = st.catalog.clone();
        let retained_delta_bytes = st.index_delta_memory_bytes();
        drop(st);
        self.shared
            .memory_governor
            .set_index_delta_bytes(retained_delta_bytes);
        drop(cs);
        for column in &vector_columns {
            let _ = fs::remove_file(vidx_path(&self.shared.dir, table, column));
        }
        cleanup_orphan_sidx(&self.shared.dir, &cleanup_catalog);
        cleanup_orphan_tidx(&self.shared.dir, &cleanup_catalog);
        // The catalog changed without advancing the commit version. Its
        // generation no longer matches `primary.runs`, so reopen will rebuild
        // if no later checkpoint republishes the still-immutable run set.
        // Keep the base inode: a later delta publication may safely reference
        // it while table epochs filter the now-unreachable keys.
        {
            let mut auto = self.shared.auto_compaction_state.lock().unwrap();
            auto.debt_operations = auto.debt_operations.saturating_add(dropped_rows);
        }
        refresh_compaction_debt(&self.shared);
        maybe_schedule_auto_compaction(&self.shared);
        publication_error.map_or(Ok(()), Err)
    }

    /// Drop the secondary (equality) index on a column. The column and its
    /// data are untouched; queries fall back to a scan.
    pub fn drop_index(&self, table: &str, column: &str) -> Result<()> {
        self.drop_index_of_kind(table, column, IndexKind::Secondary)
    }

    /// Drop the ANN (HNSW) index on a vector column, including its persisted
    /// graph.
    pub fn drop_vector_index(&self, table: &str, column: &str) -> Result<()> {
        self.drop_index_of_kind(table, column, IndexKind::Vector)
    }

    /// Drop the full-text (BM25) index on a text column.
    pub fn drop_text_index(&self, table: &str, column: &str) -> Result<()> {
        self.drop_index_of_kind(table, column, IndexKind::Text)
    }

    fn drop_index_of_kind(&self, table: &str, column: &str, kind: IndexKind) -> Result<()> {
        if self.shared.opts.read_only {
            return Err(Error::ReadOnly);
        }
        let _ddl = self.shared.ddl.lock().unwrap();
        let mut cs = lock_commit_for_maintenance(&self.shared)?;
        let mut st = self.shared.state.write().unwrap();
        let schema = st
            .catalog
            .table(table)
            .ok_or_else(|| Error::TableNotFound(table.into()))?;
        let present = match kind {
            IndexKind::Secondary => schema.indexes.iter().any(|d| d.column == column),
            IndexKind::Vector => schema.vector_indexes.iter().any(|d| d.column == column),
            IndexKind::Text => schema.text_indexes.iter().any(|d| d.column == column),
        };
        if !present {
            return Err(Error::IndexNotFound {
                table: table.into(),
                column: column.into(),
            });
        }
        if kind == IndexKind::Secondary {
            if schema
                .foreign_keys
                .iter()
                .any(|foreign_key| foreign_key.column == column)
            {
                return Err(Error::SchemaViolation(format!(
                    "cannot drop index on {table}.{column}: required by a foreign key"
                )));
            }
            if st.catalog.tables.iter().any(|child| {
                child.foreign_keys.iter().any(|foreign_key| {
                    foreign_key.referenced_table == table && foreign_key.referenced_column == column
                })
            }) {
                return Err(Error::SchemaViolation(format!(
                    "cannot drop unique index on {table}.{column}: it is a foreign key target"
                )));
            }
        }
        let mut next = st.catalog.clone();
        let schema = next.table_mut(table).expect("checked above");
        match kind {
            IndexKind::Secondary => schema.indexes.retain(|d| d.column != column),
            IndexKind::Vector => schema.vector_indexes.retain(|d| d.column != column),
            IndexKind::Text => schema.text_indexes.retain(|d| d.column != column),
        }
        let publication_error =
            publish_catalog_generation_locked(&self.shared, &mut cs, &mut st, next)?;
        let key = (table.to_owned(), column.to_owned());
        match kind {
            IndexKind::Secondary => {
                st.secondary.remove(&key);
            }
            IndexKind::Vector => {
                st.vector.remove(&key);
            }
            IndexKind::Text => {
                st.text.remove(&key);
            }
        }
        let retained_delta_bytes = st.index_delta_memory_bytes();
        let cleanup_catalog = st.catalog.clone();
        drop(st);
        self.shared
            .memory_governor
            .set_index_delta_bytes(retained_delta_bytes);
        match kind {
            IndexKind::Secondary => {
                cleanup_orphan_sidx(&self.shared.dir, &cleanup_catalog);
            }
            IndexKind::Vector => {
                let _ = fs::remove_file(vidx_path(&self.shared.dir, table, column));
            }
            IndexKind::Text => {
                cleanup_orphan_tidx(&self.shared.dir, &cleanup_catalog);
            }
        }
        publication_error.map_or(Ok(()), Err)
    }

    /// Add a column.
    ///
    /// Without a default this is a catalog-only change that takes effect
    /// immediately, whatever the size of the table: records written earlier
    /// simply read the new column as NULL. With a default, those records are
    /// backfilled with it in bounded batches, and `NOT NULL` is applied only
    /// once every one of them has a value — so an interrupted `ADD COLUMN`
    /// leaves a nullable column, never a schema its own data violates.
    pub fn add_column(&self, table: &str, column: Column) -> Result<()> {
        if self.shared.opts.read_only {
            return Err(Error::ReadOnly);
        }
        let _ddl = self.shared.ddl.lock().unwrap();
        column.validate()?;
        {
            let st = self.shared.state.read().unwrap();
            let schema = st
                .catalog
                .table(table)
                .ok_or_else(|| Error::TableNotFound(table.into()))?;
            if schema.column(&column.name).is_some() {
                return Err(Error::InvalidArgument(format!(
                    "column {table}.{} already exists",
                    column.name
                )));
            }
        }
        if column.default.is_none() {
            // Keep writers excluded from the emptiness check through catalog
            // publication; otherwise an insert using the old schema could
            // land between them and violate a new NOT NULL column.
            let mut cs = lock_commit_for_maintenance(&self.shared)?;
            if !column.nullable && !self.table_is_empty(table) {
                return Err(Error::SchemaViolation(format!(
                    "column {table}.{} is NOT NULL: give it a DEFAULT so the records already \
                     stored can be backfilled",
                    column.name
                )));
            }
            // One durable catalog write, atomic on its own: no intent needed.
            let mut st = self.shared.state.write().unwrap();
            let mut next = st.catalog.clone();
            let schema = next
                .table_mut(table)
                .ok_or_else(|| Error::TableNotFound(table.into()))?;
            if schema.column(&column.name).is_some() {
                return Err(Error::InvalidArgument(format!(
                    "column {table}.{} already exists",
                    column.name
                )));
            }
            schema.columns.push(column);
            let publication_error =
                publish_catalog_generation_locked(&self.shared, &mut cs, &mut st, next)?;
            return publication_error.map_or(Ok(()), Err);
        }
        let not_null = !column.nullable;
        let intent = DdlIntent::AddColumn {
            table: table.to_owned(),
            column,
            not_null,
        };
        self.run_ddl_intent(&intent)
    }

    /// Drop a column, its data and any index over it. The table's records are
    /// rewritten to reclaim the space, so this costs a compaction.
    pub fn drop_column(&self, table: &str, column: &str) -> Result<()> {
        if self.shared.opts.read_only {
            return Err(Error::ReadOnly);
        }
        let _ddl = self.shared.ddl.lock().unwrap();
        {
            let st = self.shared.state.read().unwrap();
            let schema = st
                .catalog
                .table(table)
                .ok_or_else(|| Error::TableNotFound(table.into()))?;
            if schema.column(column).is_none() {
                return Err(Error::ColumnNotFound {
                    table: table.into(),
                    column: column.into(),
                });
            }
            if schema
                .foreign_keys
                .iter()
                .any(|foreign_key| foreign_key.column == column)
                || st.catalog.tables.iter().any(|child| {
                    child.foreign_keys.iter().any(|foreign_key| {
                        foreign_key.referenced_table == table
                            && foreign_key.referenced_column == column
                    })
                })
            {
                return Err(Error::SchemaViolation(format!(
                    "cannot drop {table}.{column}: it participates in a foreign key"
                )));
            }
            if schema.columns.len() == 1 {
                return Err(Error::InvalidArgument(format!(
                    "{column} is the only column of {table}; drop the table instead"
                )));
            }
        }
        let intent = DdlIntent::DropColumn {
            table: table.to_owned(),
            column: column.to_owned(),
        };
        self.run_ddl_intent(&intent)
    }

    /// Rename a table. Records are keyed by table name on disk, so this
    /// rewrites them under the new name: it costs a compaction.
    pub fn rename_table(&self, table: &str, new_name: &str) -> Result<()> {
        if self.shared.opts.read_only {
            return Err(Error::ReadOnly);
        }
        let _ddl = self.shared.ddl.lock().unwrap();
        {
            let st = self.shared.state.read().unwrap();
            if st.catalog.table(table).is_none() {
                return Err(Error::TableNotFound(table.into()));
            }
            if new_name.is_empty() {
                return Err(Error::InvalidArgument(
                    "table name must not be empty".into(),
                ));
            }
            validate_short_string("table name", new_name)?;
            if new_name == table {
                return Ok(());
            }
            if st.catalog.table(new_name).is_some() {
                return Err(Error::TableExists(new_name.into()));
            }
            if st.catalog.tables.iter().any(|schema| {
                (schema.name == table && !schema.foreign_keys.is_empty())
                    || schema
                        .foreign_keys
                        .iter()
                        .any(|foreign_key| foreign_key.referenced_table == table)
            }) {
                return Err(Error::SchemaViolation(format!(
                    "cannot rename table '{table}' while it participates in a foreign key"
                )));
            }
        }
        let intent = DdlIntent::RenameTable {
            table: table.to_owned(),
            to: new_name.to_owned(),
        };
        self.run_ddl_intent(&intent)
    }

    /// Rename a column, carrying any index over it. Column names live in every
    /// record payload, so this rewrites them: it costs a compaction.
    pub fn rename_column(&self, table: &str, column: &str, new_name: &str) -> Result<()> {
        if self.shared.opts.read_only {
            return Err(Error::ReadOnly);
        }
        let _ddl = self.shared.ddl.lock().unwrap();
        {
            let st = self.shared.state.read().unwrap();
            let schema = st
                .catalog
                .table(table)
                .ok_or_else(|| Error::TableNotFound(table.into()))?;
            if schema.column(column).is_none() {
                return Err(Error::ColumnNotFound {
                    table: table.into(),
                    column: column.into(),
                });
            }
            if new_name == column {
                return Ok(());
            }
            if new_name.is_empty() {
                return Err(Error::InvalidArgument(
                    "column name must not be empty".into(),
                ));
            }
            validate_short_string("column name", new_name)?;
            if new_name == ID_COLUMN {
                return Err(Error::InvalidArgument(format!(
                    "column name '{ID_COLUMN}' is reserved for the implicit primary key"
                )));
            }
            if schema.column(new_name).is_some() {
                return Err(Error::InvalidArgument(format!(
                    "column {table}.{new_name} already exists"
                )));
            }
            if schema
                .foreign_keys
                .iter()
                .any(|foreign_key| foreign_key.column == column)
                || st.catalog.tables.iter().any(|child| {
                    child.foreign_keys.iter().any(|foreign_key| {
                        foreign_key.referenced_table == table
                            && foreign_key.referenced_column == column
                    })
                })
            {
                return Err(Error::SchemaViolation(format!(
                    "cannot rename {table}.{column} while it participates in a foreign key"
                )));
            }
        }
        let intent = DdlIntent::RenameColumn {
            table: table.to_owned(),
            column: column.to_owned(),
            to: new_name.to_owned(),
        };
        self.run_ddl_intent(&intent)
    }

    /// Run one DDL operation to completion. Every step is idempotent, so this
    /// is both the normal path and the crash-recovery path replayed by `open`.
    fn apply_ddl(&self, intent: &DdlIntent) -> Result<()> {
        match intent {
            DdlIntent::RenameTable { table, to } => {
                self.rewrite_segments(&Rewrite::RenameTable { from: table, to })
            }
            DdlIntent::RenameColumn { table, column, to } => {
                self.rewrite_segments(&Rewrite::RenameColumn {
                    table,
                    from: column,
                    to,
                })
            }
            DdlIntent::DropColumn { table, column } => {
                self.rewrite_segments(&Rewrite::DropColumn { table, column })
            }
            DdlIntent::AddColumn {
                table,
                column,
                not_null,
            } => self.apply_add_column(table, column, *not_null),
        }
    }

    fn run_ddl_intent(&self, intent: &DdlIntent) -> Result<()> {
        ensure_canonical_writable(&self.shared)?;
        if let Some(pending) = DdlIntent::load(&self.shared.dir)? {
            fence_writes(
                &self.shared,
                format!(
                    "pending DDL must be recovered before another schema change: {}",
                    pending.describe()
                ),
            );
            return Err(Error::CommitUnknown(
                "a previous DDL intent is still pending; reopen the database to recover it".into(),
            ));
        }
        if let Err(error) = intent.write(&self.shared.dir) {
            if self.shared.dir.join(crate::ddl::DDL_FILE).exists() {
                fence_writes(
                    &self.shared,
                    format!(
                        "DDL intent may have been published despite a sync failure; reopen to recover: {error}"
                    ),
                );
            }
            return Err(error);
        }
        if let Err(error) = self.apply_ddl(intent) {
            fence_writes(
                &self.shared,
                format!("DDL application failed with a durable pending intent: {error}"),
            );
            return Err(error);
        }
        if let Err(error) = DdlIntent::clear(&self.shared.dir) {
            fence_writes(
                &self.shared,
                format!("DDL completed but intent cleanup was uncertain: {error}"),
            );
            return Err(error);
        }
        Ok(())
    }

    /// Three durable steps, each one leaving a consistent database behind:
    /// publish the column as nullable, backfill it, then enforce `NOT NULL`.
    fn apply_add_column(&self, table: &str, column: &Column, not_null: bool) -> Result<()> {
        {
            let mut cs = lock_commit_for_maintenance(&self.shared)?;
            let mut st = self.shared.state.write().unwrap();
            let mut next = st.catalog.clone();
            let schema = next
                .table_mut(table)
                .ok_or_else(|| Error::TableNotFound(table.into()))?;
            let mut staged = column.clone();
            staged.nullable = true;
            match schema.columns.iter_mut().find(|c| c.name == column.name) {
                Some(existing) => *existing = staged,
                None => schema.columns.push(staged),
            }
            if let Some(error) =
                publish_catalog_generation_locked(&self.shared, &mut cs, &mut st, next)?
            {
                return Err(error);
            }
        }
        self.backfill_column(table, &column.name, &column.default_value()?)?;
        if not_null {
            let mut cs = lock_commit_for_maintenance(&self.shared)?;
            let mut st = self.shared.state.write().unwrap();
            let mut next = st.catalog.clone();
            let schema = next
                .table_mut(table)
                .ok_or_else(|| Error::TableNotFound(table.into()))?;
            if let Some(existing) = schema.columns.iter_mut().find(|c| c.name == column.name) {
                existing.nullable = false;
            }
            if let Some(error) =
                publish_catalog_generation_locked(&self.shared, &mut cs, &mut st, next)?
            {
                return Err(error);
            }
        }
        Ok(())
    }

    /// Write `fill` into every record that predates the column or holds NULL
    /// in it, through ordinary commits so indexes and durability follow. Runs
    /// in bounded batches: memory stays flat whatever the size of the table.
    fn backfill_column(&self, table: &str, column: &str, fill: &Value) -> Result<()> {
        if fill.is_null() {
            return Ok(());
        }
        // Leave conservative room for the old/new record and every derived
        // index entry charged by `Txn::stage`; a single unusually large row
        // still receives the normal explicit `MemoryLimit` error.
        let batch_rows = (self.shared.opts.memory.index_delta_pool_bytes / 1024).clamp(1, 512);
        let mut patch = Record::new();
        patch.insert(column.to_owned(), fill.clone());
        let mut cursor: Option<String> = None;
        loop {
            let batch = self.ids_needing_fill(table, column, batch_rows, cursor.as_deref())?;
            let Some(last) = batch.last() else {
                return Ok(());
            };
            cursor = Some(last.clone());
            let mut attempts = 0;
            loop {
                let mut txn = self.begin();
                for id in &batch {
                    match txn.update(table, id, patch.clone()) {
                        Ok(()) => {}
                        // Deleted while the backfill was running: nothing to fill.
                        Err(Error::RecordNotFound { .. }) => {}
                        Err(e) => return Err(e),
                    }
                }
                match txn.commit() {
                    Ok(_) => break,
                    Err(Error::Conflict(_)) if attempts < BACKFILL_RETRIES => attempts += 1,
                    Err(e) => return Err(e),
                }
            }
        }
    }

    /// The next ids (in key order, after `cursor`) whose stored record has no
    /// value for `column`. Only the payload's field names and value tags are
    /// inspected, so nothing large is decoded.
    fn ids_needing_fill(
        &self,
        table: &str,
        column: &str,
        limit: usize,
        cursor: Option<&str>,
    ) -> Result<Vec<String>> {
        let st = self.shared.state.read().unwrap();
        let mut out = Vec::new();
        st.index.visit_table(table, cursor, |id, versions| {
            let Some(last) = versions.last() else {
                return Ok(true);
            };
            if last.is_tombstone() {
                return Ok(true);
            }
            let payload = payload_bytes(&st.readers, &last.kind)?.expect("put has payload");
            if encoded_column_needs_fill(&payload, column)? {
                out.push(id.to_owned());
                if out.len() == limit {
                    return Ok(false);
                }
            }
            Ok(true)
        })?;
        Ok(out)
    }

    fn table_is_empty(&self, table: &str) -> bool {
        let st = self.shared.state.read().unwrap();
        let mut empty = true;
        let _ = st.index.visit_table(table, None, |_, versions| {
            if versions.last().is_some_and(|entry| !entry.is_tombstone()) {
                empty = false;
                return Ok(false);
            }
            Ok(true)
        });
        empty
    }

    /// After replaying a pending DDL record on open: drop what the published
    /// catalog no longer owns and rebuild every derived index from canonical
    /// data.
    fn finish_ddl_recovery(&self) -> Result<()> {
        let mut cs = lock_commit_for_maintenance(&self.shared)?;
        let _memory = Self::acquire_maintenance_memory(&self.shared);
        // `apply_ddl` has already completed the idempotent data/catalog step.
        // Drain any batched backfill writes, then publish/remap their derived
        // deltas instead of materializing the complete primary directory.
        checkpoint_measured(&self.shared, &mut cs)?;
        let retained = self.shared.state.read().unwrap().index_delta_memory_bytes();
        self.shared.memory_governor.set_index_delta_bytes(retained);
        let catalog = self.shared.state.read().unwrap().catalog.clone();
        cleanup_orphan_sidx(&self.shared.dir, &catalog);
        cleanup_orphan_tidx(&self.shared.dir, &catalog);
        cleanup_orphan_vidx(&self.shared.dir, &catalog);
        Ok(())
    }

    /// BM25 full-text search over an indexed text column. Results are the
    /// latest committed records, best score first. `filter` applies equality
    /// constraints on other columns.
    pub fn search_text(
        &self,
        table: &str,
        column: &str,
        query: &str,
        top_k: usize,
        filter: Option<&Record>,
    ) -> Result<Vec<TextHit>> {
        let _memory = self.acquire_query_memory();
        self.search_text_inner(table, column, query, top_k, filter)
    }

    fn search_text_inner(
        &self,
        table: &str,
        column: &str,
        query: &str,
        top_k: usize,
        filter: Option<&Record>,
    ) -> Result<Vec<TextHit>> {
        if top_k == 0 {
            return Ok(Vec::new());
        }
        let candidate_cap = (self.shared.opts.memory.query_working_bytes / 64).max(1);
        if top_k > candidate_cap || query.len() > self.shared.opts.memory.query_working_bytes {
            return Err(Error::MemoryLimit(format!(
                "text search request exceeds its per-query budget (top_k capacity {candidate_cap})"
            )));
        }
        let st = self.shared.state.read().unwrap();
        let schema = st
            .catalog
            .table(table)
            .ok_or_else(|| Error::TableNotFound(table.into()))?;
        if let Some(f) = filter {
            for key in f.keys() {
                if key != ID_COLUMN && schema.column(key).is_none() {
                    return Err(Error::SchemaViolation(format!(
                        "unknown filter column '{key}'"
                    )));
                }
            }
        }
        let tidx = st
            .text
            .get(&(table.to_owned(), column.to_owned()))
            .ok_or_else(|| {
                Error::InvalidArgument(format!(
                    "no text index on {table}.{column}; create one with create_text_index"
                ))
            })?;
        let ranked = tidx.search_top_k(query, top_k, |id| {
            let entry = st.latest_owned(table, id)?;
            let Some(entry) = entry else { return Ok(false) };
            if entry.is_tombstone() {
                return Ok(false);
            }
            if let Some(f) = filter {
                let rec = read_record_kind(&st.blobs, &st.readers, &entry.kind)?;
                let ok = f.iter().all(|(k, want)| {
                    if k == ID_COLUMN {
                        matches!(want, Value::Text(t) if t == id)
                    } else {
                        rec.get(k).unwrap_or(&Value::Null) == want
                    }
                });
                if !ok {
                    return Ok(false);
                }
            }
            Ok(true)
        })?;
        let mut hits = Vec::with_capacity(ranked.len());
        for (id, score) in ranked {
            let entry = st
                .index
                .latest(table, &id)?
                .expect("ranked ids were validated above");
            let mut rec = read_record_kind(&st.blobs, &st.readers, &entry.kind)?;
            rec.insert(ID_COLUMN.into(), Value::Text(id.clone()));
            hits.push(TextHit {
                id,
                score,
                record: rec,
            });
        }
        Ok(hits)
    }

    /// Hybrid search: fuse BM25 text ranking and ANN vector ranking with
    /// Reciprocal Rank Fusion (RRF, k=60). At least one modality is
    /// required; both columns must be indexed.
    pub fn search_hybrid(&self, table: &str, query: &HybridQuery<'_>) -> Result<Vec<HybridHit>> {
        let _memory = self.acquire_query_memory();
        if query.text.is_none() && query.vector.is_none() {
            return Err(Error::InvalidArgument(
                "hybrid search needs a text query, a vector, or both".into(),
            ));
        }
        if query.top_k == 0 {
            return Ok(Vec::new());
        }
        let candidate_cap = (self.shared.opts.memory.query_working_bytes / 128).max(1);
        if query.top_k > candidate_cap {
            return Err(Error::MemoryLimit(format!(
                "hybrid top_k={} exceeds its per-query capacity {candidate_cap}",
                query.top_k
            )));
        }
        let fetch_k = query.top_k.saturating_mul(4).max(50).min(candidate_cap);
        const RRF_K: f32 = 60.0;

        let mut fused: BTreeMap<String, (f32, Option<Record>)> = BTreeMap::new();
        if let Some((column, text)) = query.text {
            let hits =
                self.search_text_inner(table, column, text, fetch_k, query.filter.as_ref())?;
            for (rank, hit) in hits.into_iter().enumerate() {
                let entry = fused.entry(hit.id).or_insert((0.0, None));
                entry.0 += 1.0 / (RRF_K + rank as f32 + 1.0);
                entry.1.get_or_insert(hit.record);
            }
        }
        if let Some((column, vector)) = query.vector {
            let opts = VectorSearchOptions {
                ef_search: query.ef_search,
                filter: query.filter.clone(),
            };
            let hits = self.search_vector_inner(table, column, vector, fetch_k, &opts)?;
            for (rank, hit) in hits.into_iter().enumerate() {
                let entry = fused.entry(hit.id).or_insert((0.0, None));
                entry.0 += 1.0 / (RRF_K + rank as f32 + 1.0);
                entry.1.get_or_insert(hit.record);
            }
        }
        let mut out: Vec<HybridHit> = fused
            .into_iter()
            .map(|(id, (score, record))| HybridHit {
                id,
                score,
                record: record.expect("record captured from at least one modality"),
            })
            .collect();
        out.sort_by(|a, b| b.score.total_cmp(&a.score).then_with(|| a.id.cmp(&b.id)));
        out.truncate(query.top_k);
        Ok(out)
    }

    /// Approximate nearest-neighbour search over an indexed vector column.
    /// Results are the latest committed records, closest first (lower
    /// distance = closer). Deleted records never appear; with `Async`
    /// indexing, very recent vectors may not be searchable yet (see
    /// [`Db::wait_vector_indexing`]).
    pub fn search_vector(
        &self,
        table: &str,
        column: &str,
        query: &[f32],
        top_k: usize,
        opts: &VectorSearchOptions,
    ) -> Result<Vec<VectorHit>> {
        let _memory = self.acquire_query_memory();
        self.search_vector_inner(table, column, query, top_k, opts)
    }

    fn search_vector_inner(
        &self,
        table: &str,
        column: &str,
        query: &[f32],
        top_k: usize,
        opts: &VectorSearchOptions,
    ) -> Result<Vec<VectorHit>> {
        if query.iter().any(|component| !component.is_finite()) {
            return Err(Error::InvalidArgument(
                "vector query components must be finite float32 values".into(),
            ));
        }
        if top_k == 0 {
            return Ok(Vec::new());
        }
        let st = self.shared.state.read().unwrap();
        let schema = st
            .catalog
            .table(table)
            .ok_or_else(|| Error::TableNotFound(table.into()))?;
        let Some(col) = schema.column(column) else {
            return Err(Error::SchemaViolation(format!("unknown column '{column}'")));
        };
        if col.ty != ColumnType::Vector {
            return Err(Error::SchemaViolation(format!(
                "{table}.{column} is not a vector column"
            )));
        }
        if let Some(dim) = col.dim {
            if query.len() != dim {
                return Err(Error::InvalidArgument(format!(
                    "query has dimension {}, column expects {dim}",
                    query.len()
                )));
            }
        }
        if let Some(filter) = &opts.filter {
            for key in filter.keys() {
                if key != ID_COLUMN && schema.column(key).is_none() {
                    return Err(Error::SchemaViolation(format!(
                        "unknown filter column '{key}'"
                    )));
                }
            }
        }
        let vidx = st
            .vector
            .get(&(table.to_owned(), column.to_owned()))
            .ok_or_else(|| {
                Error::InvalidArgument(format!(
                    "no vector index on {table}.{column}; create one with create_vector_index"
                ))
            })?;

        // Candidate ids, distance heaps and the visited set are private query
        // memory. Metadata filtering may require over-fetching, but it must
        // stop at the admitted query reservation instead of escalating to the
        // complete index.
        const ESTIMATED_VECTOR_CANDIDATE_BYTES: usize = 128;
        let candidate_cap =
            (self.shared.opts.memory.query_working_bytes / ESTIMATED_VECTOR_CANDIDATE_BYTES).max(1);
        if top_k > candidate_cap {
            return Err(Error::MemoryLimit(format!(
                "vector top_k={top_k} exceeds the per-query candidate capacity {candidate_cap}"
            )));
        }
        let requested_ef = opts
            .ef_search
            .unwrap_or_else(|| top_k.saturating_mul(2).max(64));
        // Larger memory defaults reduced the number of independently searched
        // HNSW generations and silently cut aggregate graph exploration at the
        // same public ef_search. Calibrate the internal beam so the public
        // quality curve remains at least as strong as the historical default,
        // while keeping construction cost and persisted graph size unchanged.
        let ef_base = requested_ef.saturating_mul(2).min(candidate_cap);
        if vidx.live_len() == 0 {
            return Ok(Vec::new());
        }
        // Over-fetch to survive tombstones and metadata filtering, escalating
        // until enough hits pass or every backend label has been considered.
        let total = vidx.total_len();
        let search_limit = total.min(candidate_cap);
        let mut fetch = top_k.saturating_mul(4).max(32).min(search_limit);
        loop {
            let raw = vidx.search_raw(query, fetch, ef_base.max(fetch));
            let mut hits: Vec<VectorHit> = Vec::with_capacity(top_k);
            for (id, distance) in &raw {
                let entry = st.latest_owned(table, id)?;
                let Some(entry) = entry else { continue };
                if entry.is_tombstone() {
                    continue;
                }
                let mut rec = read_record_kind(&st.blobs, &st.readers, &entry.kind)?;
                if let Some(filter) = &opts.filter {
                    let ok = filter.iter().all(|(k, want)| {
                        if k == ID_COLUMN {
                            matches!(want, Value::Text(t) if t == id)
                        } else {
                            rec.get(k).unwrap_or(&Value::Null) == want
                        }
                    });
                    if !ok {
                        continue;
                    }
                }
                rec.insert(ID_COLUMN.into(), Value::Text(id.clone()));
                hits.push(VectorHit {
                    id: id.clone(),
                    distance: *distance,
                    record: rec,
                });
                if hits.len() == top_k {
                    break;
                }
            }
            if hits.len() >= top_k || fetch >= search_limit {
                return Ok(hits);
            }
            fetch = fetch.saturating_mul(4).min(search_limit);
        }
    }

    /// Vectors committed in `Async` mode that are not yet searchable.
    pub fn vector_indexing_backlog(&self) -> u64 {
        self.shared.vector_backlog.load(AtomicOrdering::SeqCst)
    }

    /// Block until every async-committed vector is searchable.
    pub fn wait_vector_indexing(&self) -> Result<()> {
        wait_vector_indexing_shared(&self.shared)
    }

    pub fn tables(&self) -> Vec<String> {
        let st = self.shared.state.read().unwrap();
        st.catalog.tables.iter().map(|t| t.name.clone()).collect()
    }

    /// The schema of a table, if it exists.
    pub fn table_schema(&self, table: &str) -> Option<TableSchema> {
        let st = self.shared.state.read().unwrap();
        st.catalog.table(table).cloned()
    }

    /// Execute one SQL statement from the deliberately small V1 dialect.
    ///
    /// SELECT reads the latest committed state (read committed); for
    /// snapshot-consistent reads use [`Db::scan_at`]/[`Db::get_at`].
    /// UPDATE/DELETE apply their write set through a transaction, retrying a
    /// bounded number of times on optimistic conflict. Multi-row INSERTs are
    /// a single atomic commit.
    pub fn query(&self, sql: &str) -> Result<crate::sql::QueryOutput> {
        let _memory = self.acquire_query_memory();
        crate::sql::execute(self, sql)
    }

    /// Execute SQL with positional `?` or `%s` placeholders. Values remain
    /// typed and are bound after parsing; they are never interpolated into the
    /// SQL string.
    pub fn query_params(&self, sql: &str, params: &[Value]) -> Result<crate::sql::QueryOutput> {
        let _memory = self.acquire_query_memory();
        crate::sql::execute_positional(self, sql, params)
    }

    /// Execute SQL with DB-API-style `%(name)s` placeholders.
    pub fn query_named_params(
        &self,
        sql: &str,
        params: &Record,
    ) -> Result<crate::sql::QueryOutput> {
        let _memory = self.acquire_query_memory();
        crate::sql::execute_named(self, sql, params)
    }

    pub(crate) fn query_params_bounded(
        &self,
        sql: &str,
        params: &[Value],
        max_rows: usize,
    ) -> Result<crate::sql::QueryOutput> {
        let _memory = self.acquire_query_memory();
        crate::sql::execute_positional_bounded(self, sql, params, max_rows)
    }

    pub(crate) fn query_named_params_bounded(
        &self,
        sql: &str,
        params: &Record,
        max_rows: usize,
    ) -> Result<crate::sql::QueryOutput> {
        let _memory = self.acquire_query_memory();
        crate::sql::execute_named_bounded(self, sql, params, max_rows)
    }

    /// Stream a snapshot-consistent single-table SELECT without materializing
    /// the complete result. WHERE, projection, OFFSET and LIMIT are supported;
    /// blocking operators currently use [`Db::query`] and its spill path.
    pub fn query_cursor<'db>(&'db self, sql: &str) -> Result<crate::sql::QueryCursor<'db>> {
        crate::sql::execute_cursor(self, sql)
    }

    /// Streaming SELECT with positional parameters.
    pub fn query_cursor_params<'db>(
        &'db self,
        sql: &str,
        params: &[Value],
    ) -> Result<crate::sql::QueryCursor<'db>> {
        crate::sql::execute_cursor_positional(self, sql, params)
    }

    /// Streaming SELECT with named parameters.
    pub fn query_cursor_named_params<'db>(
        &'db self,
        sql: &str,
        params: &Record,
    ) -> Result<crate::sql::QueryCursor<'db>> {
        crate::sql::execute_cursor_named(self, sql, params)
    }

    // --- transactions --------------------------------------------------------

    /// Begin a transaction reading from a stable snapshot of the current
    /// committed state. Multiple transactions stage writes in parallel; they
    /// only meet at commit.
    pub fn begin(&self) -> Txn {
        Txn {
            shared: self.shared.clone(),
            snapshot: self.snapshot(),
            started_at: Instant::now(),
            staged: Vec::new(),
            staged_bytes: 0,
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

    /// Stream a strictly primary-key-ordered append directly into one
    /// canonical segment and one immutable primary run.
    ///
    /// This is the bounded-memory path for initial imports. Every record must
    /// carry an explicit text `id`; ids must be strictly increasing and above
    /// the table's existing high watermark. The complete input becomes
    /// visible atomically at one commit version. Build secondary, text or
    /// vector indexes after loading; a table with existing derived indexes is
    /// rejected so their queryable state can never lag canonical data.
    pub fn bulk_insert_sorted<I>(&self, table: &str, records: I) -> Result<usize>
    where
        I: IntoIterator<Item = Record>,
    {
        if self.shared.opts.read_only {
            return Err(Error::ReadOnly);
        }
        let mut cs = lock_commit_for_maintenance(&self.shared)?;
        let _memory = Self::acquire_maintenance_memory(&self.shared);

        // Start from an empty mutable generation. This preserves earlier WAL
        // commits before the direct-manifest publication below.
        checkpoint_measured(&self.shared, &mut cs)?;
        let mut records = records.into_iter().peekable();

        let (
            schema,
            committed_version,
            next_segment_id,
            old_segments,
            old_generation,
            old_run_metas,
            first_run,
            existing_high,
        ) = {
            let st = self.shared.state.read().unwrap();
            let schema = st
                .catalog
                .table(table)
                .ok_or_else(|| Error::TableNotFound(table.into()))?
                .clone();
            if !schema.indexes.is_empty()
                || !schema.text_indexes.is_empty()
                || !schema.vector_indexes.is_empty()
            {
                return Err(Error::InvalidArgument(format!(
                    "bulk_insert_sorted requires {table} to have no derived indexes; create them after the load"
                )));
            }
            (
                schema,
                st.committed_version,
                st.next_segment_id,
                st.segments.clone(),
                st.index.generation,
                st.index.run_metas(),
                st.index.runs.is_empty(),
                st.table_high_ids.get(table).cloned(),
            )
        };
        if records.peek().is_none() {
            return Ok(0);
        }

        let version = committed_version.saturating_add(1);
        let segment_name = segment_file_name(next_segment_id);
        let segment_path = self.shared.dir.join(SEGMENTS_DIR).join(&segment_name);
        let segment_tmp = self
            .shared
            .dir
            .join(SEGMENTS_DIR)
            .join(format!("{segment_name}.bulk.tmp"));
        let run_file = if first_run {
            "primary.pidx".to_owned()
        } else {
            format!("primary-L0-{}.pidx.run", Ulid::new())
        };
        let run_level = if first_run { PRIMARY_BASE_LEVEL } else { 0 };
        let run_path = self.shared.dir.join(INDEXES_DIR).join(&run_file);
        let run_tmp = run_path.with_extension("bulk.tmp");

        let load_result = (|| -> Result<(usize, u64, String, PagedIndex)> {
            let segment_file = File::create(&segment_tmp)?;
            let mut segment = BufWriter::with_capacity(
                self.shared
                    .opts
                    .memory
                    .maintenance_pool_bytes
                    .clamp(1, 1024 * 1024),
                segment_file,
            );
            let mut primary = PagedWriter::create(&run_tmp, 0, None)?;
            let mut blob_sink =
                BlobSink::new(&self.shared.dir, self.shared.opts.external_blob_threshold);
            let mut previous_id = existing_high;
            let mut segment_entry = Vec::new();
            let mut primary_key_buf = Vec::new();
            let mut primary_value = Vec::with_capacity(25);
            let mut position = 0u64;
            let mut rows = 0usize;

            for record in records {
                let id = match record.get(ID_COLUMN) {
                    Some(Value::Text(id)) if !id.is_empty() => id.clone(),
                    Some(Value::Text(_)) => {
                        return Err(Error::InvalidArgument("id must not be empty".into()))
                    }
                    Some(_) => {
                        return Err(Error::SchemaViolation(
                            "bulk_insert_sorted requires a text id".into(),
                        ))
                    }
                    None => {
                        return Err(Error::InvalidArgument(
                            "bulk_insert_sorted requires an explicit id".into(),
                        ))
                    }
                };
                validate_short_string("record id", &id)?;
                if previous_id
                    .as_deref()
                    .is_some_and(|previous| id.as_str() <= previous)
                {
                    return Err(Error::InvalidArgument(format!(
                        "bulk_insert_sorted ids must be strictly increasing and above existing ids: '{id}' follows '{}'; no rows were published",
                        previous_id.as_deref().unwrap_or("")
                    )));
                }
                let normalized = normalize_record(&schema, record)?;
                let payload = encode_record_ordered(&schema, &normalized, Some(&mut blob_sink))?;
                let payload_len = u32::try_from(payload.len()).map_err(|_| {
                    Error::InvalidArgument("bulk record payload exceeds 4 GiB".into())
                })?;
                let payload_rel =
                    encode_entry_into(&mut segment_entry, version, table, &id, Some(&payload))?;
                encode_primary_key_into(table, &id, &mut primary_key_buf);
                encode_primary_entry_into(
                    &VersionEntry {
                        version,
                        kind: VKind::SegPut {
                            segment: next_segment_id,
                            payload_offset: position + payload_rel,
                            payload_len,
                        },
                    },
                    &mut primary_value,
                )?;
                primary.add(&primary_key_buf, &primary_value)?;
                segment.write_all(&segment_entry)?;
                position = position.saturating_add(segment_entry.len() as u64);
                previous_id = Some(id);
                rows = rows.saturating_add(1);
            }
            segment.flush()?;
            let segment_file = segment
                .into_inner()
                .map_err(|error| Error::Io(error.into_error()))?;
            segment_file.sync_all()?;
            blob_sink.publish()?;

            let mut new_segments = old_segments.clone();
            new_segments.push(SegmentMeta {
                id: next_segment_id,
                len: position,
            });
            let generation = primary_generation(version, &new_segments, &{
                let st = self.shared.state.read().unwrap();
                st.catalog.clone()
            });
            primary.set_dump_version(generation);
            primary.finish()?;
            fs::rename(&segment_tmp, &segment_path)?;
            fs::rename(&run_tmp, &run_path)?;
            fsync_dir(&self.shared.dir.join(SEGMENTS_DIR))?;
            fsync_dir(&self.shared.dir.join(INDEXES_DIR))?;
            let index = PagedIndex::open(&run_path)?;
            Ok((
                rows,
                generation,
                previous_id.expect("non-empty bulk load has a final id"),
                index,
            ))
        })();

        let (rows, generation, final_id, index) = match load_result {
            Ok(result) => result,
            Err(error) => {
                let _ = fs::remove_file(&segment_tmp);
                let _ = fs::remove_file(&run_tmp);
                let _ = fs::remove_file(&segment_path);
                let _ = fs::remove_file(&run_path);
                return Err(error);
            }
        };
        let (segment_bytes, run_bytes, segment_reader) = match (|| -> Result<(u64, u64, File)> {
            Ok((
                fs::metadata(&segment_path)?.len(),
                fs::metadata(&run_path)?.len(),
                File::open(&segment_path)?,
            ))
        })() {
            Ok(files) => files,
            Err(error) => {
                let _ = fs::remove_file(&segment_path);
                let _ = fs::remove_file(&run_path);
                return Err(error);
            }
        };
        let run_meta = PrimaryRunMeta {
            file: run_file,
            level: run_level,
            bytes: run_bytes,
            generation,
        };
        let mut new_run_metas = old_run_metas.clone();
        new_run_metas.push(run_meta.clone());
        let indexes_dir = self.shared.dir.join(INDEXES_DIR);
        if let Err(error) = PrimaryRunManifest::new(generation, new_run_metas).publish(&indexes_dir)
        {
            let _ = fs::remove_file(&segment_path);
            let _ = fs::remove_file(&run_path);
            return Err(error);
        }
        let mut new_segments = old_segments;
        new_segments.push(SegmentMeta {
            id: next_segment_id,
            len: segment_bytes,
        });
        let identity_high_water = {
            let st = self.shared.state.read().unwrap();
            identity_manifest(&st)
        };
        let manifest_outcome = match (Manifest {
            format_version: FORMAT_VERSION,
            committed_version: version,
            segments: new_segments.clone(),
            wal_id: cs.wal().id,
            identity_high_water,
            catalog: Some(self.shared.state.read().unwrap().catalog.clone()),
        })
        .publish(&self.shared.dir)
        {
            Ok(outcome) => outcome,
            Err(error) => {
                let _ =
                    PrimaryRunManifest::new(old_generation, old_run_metas).publish(&indexes_dir);
                let _ = fs::remove_file(&segment_path);
                let _ = fs::remove_file(&run_path);
                return Err(error);
            }
        };

        let final_ulid = Ulid::from_string(&final_id).ok();
        {
            let mut st = self.shared.state.write().unwrap();
            st.committed_version = version;
            st.segments = new_segments;
            st.readers.insert(next_segment_id, Arc::new(segment_reader));
            st.next_segment_id = next_segment_id.saturating_add(1);
            st.index.generation = generation;
            st.index.runs.push(PrimaryRun {
                meta: run_meta,
                index: Arc::new(index),
            });
            st.table_high_ids.insert(table.to_owned(), final_id);
        }
        if let Some(final_ulid) = final_ulid {
            let mut previous = self.shared.last_generated_id.lock().unwrap();
            *previous = (*previous).max(final_ulid);
        }
        let publication_error =
            publication_sync_error(&self.shared, "bulk insert manifest", manifest_outcome);
        self.shared
            .primary_checkpoint_bytes_written
            .fetch_add(run_bytes, AtomicOrdering::Relaxed);
        cleanup_primary_run_orphans(&self.shared.dir);
        maybe_schedule_primary_compaction(&self.shared);
        if publication_error.is_none() {
            cleanup_orphans(
                &self.shared.dir,
                &Manifest {
                    format_version: FORMAT_VERSION,
                    committed_version: version,
                    segments: self.shared.state.read().unwrap().segments.clone(),
                    wal_id: cs.wal().id,
                    identity_high_water: identity_manifest(&self.shared.state.read().unwrap()),
                    catalog: Some(self.shared.state.read().unwrap().catalog.clone()),
                },
            )?;
        }
        publication_error.map_or(Ok(rows), Err)
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
        // A point lookup has no query operator or candidate buffer: its only
        // material allocation is the Record returned to the caller, which is
        // explicitly outside the working-set budget. Reserving a full
        // query_working_bytes slot here needlessly capped default concurrency
        // at four readers.
        self.get_unbudgeted(table, id)
    }

    pub(crate) fn get_unbudgeted(&self, table: &str, id: &str) -> Result<Option<Record>> {
        shared_get_at(&self.shared, table, id, u64::MAX)
    }

    /// Read a record as of a snapshot.
    pub fn get_at(&self, snapshot: &Snapshot, table: &str, id: &str) -> Result<Option<Record>> {
        self.get_at_unbudgeted(snapshot, table, id)
    }

    pub(crate) fn get_at_unbudgeted(
        &self,
        snapshot: &Snapshot,
        table: &str,
        id: &str,
    ) -> Result<Option<Record>> {
        self.validate_snapshot_owner(snapshot)?;
        shared_get_at(&self.shared, table, id, snapshot.version)
    }

    /// All visible records of a table, ordered by id.
    pub fn scan(&self, table: &str) -> Result<Vec<(String, Record)>> {
        let _memory = self.acquire_query_memory();
        self.scan_unbudgeted(table)
    }

    pub(crate) fn scan_unbudgeted(&self, table: &str) -> Result<Vec<(String, Record)>> {
        let snapshot = self.snapshot();
        shared_scan_at(&self.shared, table, snapshot.version)
    }

    /// At most `limit` visible records, ordered by id and strictly after
    /// `after_id`. This is the bounded primitive used by streaming SQL scans.
    /// Passing `None` starts at the beginning of the table.
    pub fn scan_batch(
        &self,
        table: &str,
        after_id: Option<&str>,
        limit: usize,
    ) -> Result<Vec<(String, Record)>> {
        let _memory = self.acquire_query_memory();
        let snapshot = self.snapshot();
        shared_scan_batch_at(&self.shared, table, snapshot.version, after_id, limit)
    }

    /// All records of a table as of a snapshot, ordered by id.
    pub fn scan_at(&self, snapshot: &Snapshot, table: &str) -> Result<Vec<(String, Record)>> {
        let _memory = self.acquire_query_memory();
        self.validate_snapshot_owner(snapshot)?;
        shared_scan_at(&self.shared, table, snapshot.version)
    }

    /// Snapshot-consistent bounded scan. Repeated calls with the same snapshot
    /// and the last returned id form a stable cursor without retaining all rows.
    pub fn scan_batch_at(
        &self,
        snapshot: &Snapshot,
        table: &str,
        after_id: Option<&str>,
        limit: usize,
    ) -> Result<Vec<(String, Record)>> {
        let _memory = self.acquire_query_memory();
        self.scan_batch_at_unbudgeted(snapshot, table, after_id, limit)
    }

    pub(crate) fn scan_batch_at_unbudgeted(
        &self,
        snapshot: &Snapshot,
        table: &str,
        after_id: Option<&str>,
        limit: usize,
    ) -> Result<Vec<(String, Record)>> {
        self.validate_snapshot_owner(snapshot)?;
        shared_scan_batch_at(&self.shared, table, snapshot.version, after_id, limit)
    }

    fn validate_snapshot_owner(&self, snapshot: &Snapshot) -> Result<()> {
        if Arc::ptr_eq(&self.shared, &snapshot.shared) {
            Ok(())
        } else {
            Err(Error::InvalidArgument(
                "snapshot belongs to a different database handle".into(),
            ))
        }
    }

    /// Equality lookup over the latest committed state. Uses the secondary
    /// index when one exists on the column; falls back to a full scan.
    pub fn find_eq(
        &self,
        table: &str,
        column: &str,
        value: &Value,
    ) -> Result<Vec<(String, Record)>> {
        let _memory = self.acquire_query_memory();
        self.find_eq_unbudgeted(table, column, value)
    }

    pub(crate) fn find_eq_unbudgeted(
        &self,
        table: &str,
        column: &str,
        value: &Value,
    ) -> Result<Vec<(String, Record)>> {
        // Register the exact generation while holding locks in the same order
        // as compaction (snapshots -> state). This lets record decoding continue
        // after the state lock is released without blob/segment GC invalidating
        // the captured sources.
        let mut snapshots = self.shared.snapshots.lock().unwrap();
        let st = self.shared.state.read().unwrap();
        let version = st.committed_version;
        *snapshots.entry(version).or_insert(0) += 1;
        drop(snapshots);
        let _snapshot_guard = Snapshot {
            version,
            shared: self.shared.clone(),
        };
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
        if let Some(idx) = st.secondary.get(&(table.to_owned(), column.to_owned())) {
            let ids = idx.ids(&index_key(value))?;
            let blobs = st.blobs.clone();
            let mut readers = SegmentReaders::new();
            let mut prepared = Vec::with_capacity(ids.len());
            for id in ids {
                if let Some(last) = st.visible_owned(table, &id, version)? {
                    if !last.is_tombstone() {
                        if let VKind::SegPut { segment, .. } = &last.kind {
                            let reader = st.readers.get(segment).ok_or_else(|| {
                                Error::Corrupt(format!("missing segment {segment}"))
                            })?;
                            readers.entry(*segment).or_insert_with(|| reader.clone());
                        }
                        prepared.push((id, last.kind));
                    }
                }
            }
            drop(st);
            let mut out = Vec::with_capacity(prepared.len());
            for (id, kind) in prepared {
                let mut record = read_record_kind(&blobs, &readers, &kind)?;
                record
                    .entry(ID_COLUMN.into())
                    .or_insert_with(|| Value::Text(id.clone()));
                out.push((id, record));
            }
            out.sort_unstable_by(|a, b| a.0.cmp(&b.0));
            return Ok(out);
        }

        if self.shared.opts.read_only {
            let mut out = Vec::new();
            find_eq_via_primary_index(&st, table, column, value, &mut out)?;
            out.sort_unstable_by(|a, b| a.0.cmp(&b.0));
            return Ok(out);
        }

        // Walk immutable segments in physical order and decode only matching
        // records. Holding the shared state view keeps its segment set and
        // latest-version directory stable for the duration; the scan is
        // read-only and therefore remains concurrent with other readers.
        let mut out = Vec::new();
        find_eq_streaming(&self.shared, &st, table, column, value, &mut out)?;
        out.sort_unstable_by(|a, b| a.0.cmp(&b.0));
        Ok(out)
    }

    /// Bounded equality lookup ordered by id. A secondary index is used when
    /// present; otherwise the primary directory is scanned without
    /// materializing the complete match set.
    pub fn find_eq_batch(
        &self,
        table: &str,
        column: &str,
        value: &Value,
        after_id: Option<&str>,
        limit: usize,
    ) -> Result<Vec<(String, Record)>> {
        let _memory = self.acquire_query_memory();
        self.find_eq_batch_unbudgeted(table, column, value, after_id, limit)
    }

    pub(crate) fn find_eq_batch_unbudgeted(
        &self,
        table: &str,
        column: &str,
        value: &Value,
        after_id: Option<&str>,
        limit: usize,
    ) -> Result<Vec<(String, Record)>> {
        let st = self.shared.state.read().unwrap();
        let schema = st
            .catalog
            .table(table)
            .ok_or_else(|| Error::TableNotFound(table.into()))?;
        if schema.column(column).is_none() {
            return Err(Error::SchemaViolation(format!("unknown column '{column}'")));
        }
        if value.is_null() || limit == 0 {
            return Ok(Vec::new());
        }
        let mut out = Vec::with_capacity(limit);
        if let Some(index) = st.secondary.get(&(table.to_owned(), column.to_owned())) {
            let ids = index.ids_batch(&index_key(value), after_id, limit)?;
            if ids.is_empty() {
                return Ok(out);
            }
            for id in ids {
                let Some(entry) = st.latest_owned(table, &id)? else {
                    continue;
                };
                if entry.is_tombstone() {
                    continue;
                }
                let mut record = read_record_kind(&st.blobs, &st.readers, &entry.kind)?;
                record
                    .entry(ID_COLUMN.into())
                    .or_insert_with(|| Value::Text(id.clone()));
                out.push((id, record));
            }
            return Ok(out);
        }

        st.index.visit_table(table, after_id, |id, versions| {
            let Some(entry) = versions
                .iter()
                .rev()
                .find(|entry| entry.version > schema.epoch)
            else {
                return Ok(true);
            };
            if entry.is_tombstone() {
                return Ok(true);
            }
            let mut record = read_record_kind(&st.blobs, &st.readers, &entry.kind)?;
            if record.get(column).unwrap_or(&Value::Null) != value {
                return Ok(true);
            }
            record
                .entry(ID_COLUMN.into())
                .or_insert_with(|| Value::Text(id.to_owned()));
            out.push((id.to_owned(), record));
            if out.len() == limit {
                return Ok(false);
            }
            Ok(true)
        })?;
        Ok(out)
    }

    // --- maintenance -----------------------------------------------------------

    /// Drain committed in-memory data into a new immutable segment, publish a
    /// new manifest, and rotate the WAL.
    pub fn checkpoint(&self) -> Result<()> {
        if self.shared.opts.read_only {
            return Err(Error::ReadOnly);
        }
        // Freeze each canonical generation while holding the commit mutex,
        // then perform segment/WAL/manifest I/O on the checkpoint worker. New
        // commits may proceed against the active generation while this method
        // remains a barrier for everything committed before its freeze.
        let primary_scheduled = {
            let mut cs = lock_commit_for_maintenance(&self.shared)?;
            wait_vector_indexing_shared(&self.shared)?;
            let memory = Self::acquire_maintenance_memory(&self.shared);
            schedule_frozen_checkpoint(&self.shared, &mut cs, memory)?
        };
        if primary_scheduled {
            wait_for_background_checkpoint(&self.shared)?;
        }

        let derived_scheduled = {
            let cs = lock_commit_for_maintenance(&self.shared)?;
            wait_vector_indexing_shared(&self.shared)?;
            let memory = Self::acquire_maintenance_memory(&self.shared);
            let scheduled = schedule_frozen_derived(&self.shared, memory)?;
            drop(cs);
            scheduled
        };
        if derived_scheduled {
            wait_for_background_derived(&self.shared)?;
        }
        refresh_compaction_debt_if_needed(&self.shared);
        maybe_schedule_auto_compaction(&self.shared);
        Ok(())
    }

    /// Cumulative checkpoint count and wall time for this open handle.
    pub fn maintenance_stats(&self) -> MaintenanceStats {
        let (compaction_debt_operations, estimated_reclaimable_bytes) = {
            let auto = self.shared.auto_compaction_state.lock().unwrap();
            (auto.debt_operations, auto.estimated_reclaimable_bytes)
        };
        let (segments, primary_runs, secondary_runs, text_runs) = {
            let state = self.shared.state.read().unwrap();
            (
                state.segments.len(),
                state.index.runs.len(),
                state.secondary.values().map(|index| index.runs.len()).sum(),
                state.text.values().map(|index| index.runs.len()).sum(),
            )
        };
        MaintenanceStats {
            commits: self.shared.commit_count.load(AtomicOrdering::Relaxed),
            commit_time: Duration::from_nanos(
                self.shared.commit_nanos.load(AtomicOrdering::Relaxed),
            ),
            commit_lock_wait_time: Duration::from_nanos(
                self.shared
                    .commit_lock_wait_nanos
                    .load(AtomicOrdering::Relaxed),
            ),
            commit_lock_hold_time: Duration::from_nanos(
                self.shared
                    .commit_lock_hold_nanos
                    .load(AtomicOrdering::Relaxed),
            ),
            commit_prepare_time: Duration::from_nanos(
                self.shared
                    .commit_prepare_nanos
                    .load(AtomicOrdering::Relaxed),
            ),
            commit_locked_prepare_time: Duration::from_nanos(
                self.shared
                    .commit_locked_prepare_nanos
                    .load(AtomicOrdering::Relaxed),
            ),
            commit_wal_time: Duration::from_nanos(
                self.shared.commit_wal_nanos.load(AtomicOrdering::Relaxed),
            ),
            commit_wal_append_time: Duration::from_nanos(
                self.shared
                    .commit_wal_append_nanos
                    .load(AtomicOrdering::Relaxed),
            ),
            wal_syncs: self.shared.wal_sync_count.load(AtomicOrdering::Relaxed),
            wal_sync_time: Duration::from_nanos(
                self.shared.wal_sync_nanos.load(AtomicOrdering::Relaxed),
            ),
            wal_synced_bytes: self.shared.wal_synced_bytes.load(AtomicOrdering::Relaxed),
            wal_sync_max_group_commits: self
                .shared
                .wal_sync_max_group_commits
                .load(AtomicOrdering::Relaxed),
            wal_sync_max_group_bytes: self
                .shared
                .wal_sync_max_group_bytes
                .load(AtomicOrdering::Relaxed),
            wal_group_coalesce_time: Duration::from_nanos(
                self.shared
                    .wal_group_coalesce_nanos
                    .load(AtomicOrdering::Relaxed),
            ),
            wal_group_leader_lock_wait_time: Duration::from_nanos(
                self.shared
                    .wal_group_leader_lock_wait_nanos
                    .load(AtomicOrdering::Relaxed),
            ),
            grouped_commits: self
                .shared
                .grouped_commit_count
                .load(AtomicOrdering::Relaxed),
            coordinated_batches: self
                .shared
                .coordinated_batch_count
                .load(AtomicOrdering::Relaxed),
            coordinated_commits: self
                .shared
                .coordinated_commit_count
                .load(AtomicOrdering::Relaxed),
            point_read_throttles: self
                .shared
                .point_read_throttle_count
                .load(AtomicOrdering::Relaxed),
            commit_apply_time: Duration::from_nanos(
                self.shared.commit_apply_nanos.load(AtomicOrdering::Relaxed),
            ),
            checkpoints: self.shared.checkpoint_count.load(AtomicOrdering::Relaxed),
            checkpoint_time: Duration::from_nanos(
                self.shared.checkpoint_nanos.load(AtomicOrdering::Relaxed),
            ),
            automatic_compactions: self
                .shared
                .automatic_compaction_count
                .load(AtomicOrdering::Relaxed),
            automatic_compaction_time: Duration::from_nanos(
                self.shared
                    .automatic_compaction_nanos
                    .load(AtomicOrdering::Relaxed),
            ),
            automatic_compaction_failures: self
                .shared
                .automatic_compaction_failures
                .load(AtomicOrdering::Relaxed),
            automatic_compaction_bytes_reclaimed: self
                .shared
                .automatic_compaction_bytes_reclaimed
                .load(AtomicOrdering::Relaxed),
            compaction_debt_operations,
            estimated_reclaimable_bytes,
            segments,
            primary_runs,
            primary_run_compactions: self
                .shared
                .primary_run_compaction_count
                .load(AtomicOrdering::Relaxed),
            primary_run_compaction_time: Duration::from_nanos(
                self.shared
                    .primary_run_compaction_nanos
                    .load(AtomicOrdering::Relaxed),
            ),
            primary_run_compaction_bytes_read: self
                .shared
                .primary_run_compaction_bytes_read
                .load(AtomicOrdering::Relaxed),
            primary_run_compaction_bytes_written: self
                .shared
                .primary_run_compaction_bytes_written
                .load(AtomicOrdering::Relaxed),
            primary_checkpoint_bytes_written: self
                .shared
                .primary_checkpoint_bytes_written
                .load(AtomicOrdering::Relaxed),
            secondary_runs,
            secondary_run_compactions: self
                .shared
                .secondary_run_compaction_count
                .load(AtomicOrdering::Relaxed),
            secondary_run_compaction_time: Duration::from_nanos(
                self.shared
                    .secondary_run_compaction_nanos
                    .load(AtomicOrdering::Relaxed),
            ),
            secondary_run_compaction_bytes_read: self
                .shared
                .secondary_run_compaction_bytes_read
                .load(AtomicOrdering::Relaxed),
            secondary_run_compaction_bytes_written: self
                .shared
                .secondary_run_compaction_bytes_written
                .load(AtomicOrdering::Relaxed),
            secondary_checkpoint_bytes_written: self
                .shared
                .secondary_checkpoint_bytes_written
                .load(AtomicOrdering::Relaxed),
            text_runs,
            text_run_compactions: self
                .shared
                .text_run_compaction_count
                .load(AtomicOrdering::Relaxed),
            text_run_compaction_time: Duration::from_nanos(
                self.shared
                    .text_run_compaction_nanos
                    .load(AtomicOrdering::Relaxed),
            ),
            text_run_compaction_bytes_read: self
                .shared
                .text_run_compaction_bytes_read
                .load(AtomicOrdering::Relaxed),
            text_run_compaction_bytes_written: self
                .shared
                .text_run_compaction_bytes_written
                .load(AtomicOrdering::Relaxed),
            text_checkpoint_bytes_written: self
                .shared
                .text_checkpoint_bytes_written
                .load(AtomicOrdering::Relaxed),
            derived_publications: self
                .shared
                .derived_publication_count
                .load(AtomicOrdering::Relaxed),
            derived_publication_time: Duration::from_nanos(
                self.shared
                    .derived_publication_nanos
                    .load(AtomicOrdering::Relaxed),
            ),
        }
    }

    /// Wait until the current primary-index level promotion has published.
    /// Checkpoints remain independently durable while this maintenance runs.
    pub fn wait_for_primary_compaction(&self) -> Result<()> {
        wait_for_background_checkpoint(&self.shared)?;
        let initial_failures = self
            .shared
            .index_maintenance_failures
            .load(AtomicOrdering::Relaxed);
        loop {
            if !self
                .shared
                .maintenance_worker_alive
                .load(AtomicOrdering::Acquire)
                || self
                    .shared
                    .index_maintenance_failures
                    .load(AtomicOrdering::Relaxed)
                    .saturating_sub(initial_failures)
                    >= MAX_MAINTENANCE_WAIT_RETRIES
            {
                return Err(maintenance_wait_error(&self.shared, "primary compaction"));
            }
            maybe_schedule_primary_compaction(&self.shared);
            let scheduled = self
                .shared
                .primary_compaction_scheduled
                .load(AtomicOrdering::Acquire);
            if !scheduled && !primary_compaction_needed(&self.shared) {
                return Ok(());
            }
            std::thread::sleep(Duration::from_millis(1));
        }
    }

    /// Wait until every currently eligible secondary-index level promotion
    /// has published. Checkpoints remain independently durable.
    pub fn wait_for_secondary_compaction(&self) -> Result<()> {
        let initial_failures = self
            .shared
            .index_maintenance_failures
            .load(AtomicOrdering::Relaxed);
        loop {
            if !self
                .shared
                .maintenance_worker_alive
                .load(AtomicOrdering::Acquire)
                || self
                    .shared
                    .index_maintenance_failures
                    .load(AtomicOrdering::Relaxed)
                    .saturating_sub(initial_failures)
                    >= MAX_MAINTENANCE_WAIT_RETRIES
            {
                return Err(maintenance_wait_error(&self.shared, "secondary compaction"));
            }
            maybe_schedule_secondary_compaction(&self.shared);
            let scheduled = self
                .shared
                .secondary_compaction_scheduled
                .load(AtomicOrdering::Acquire);
            if !scheduled && !secondary_compaction_needed(&self.shared) {
                return Ok(());
            }
            std::thread::sleep(Duration::from_millis(1));
        }
    }

    pub fn wait_for_text_compaction(&self) -> Result<()> {
        let initial_failures = self
            .shared
            .index_maintenance_failures
            .load(AtomicOrdering::Relaxed);
        loop {
            if !self
                .shared
                .maintenance_worker_alive
                .load(AtomicOrdering::Acquire)
                || self
                    .shared
                    .index_maintenance_failures
                    .load(AtomicOrdering::Relaxed)
                    .saturating_sub(initial_failures)
                    >= MAX_MAINTENANCE_WAIT_RETRIES
            {
                return Err(maintenance_wait_error(&self.shared, "text compaction"));
            }
            maybe_schedule_text_compaction(&self.shared);
            let scheduled = self
                .shared
                .text_compaction_scheduled
                .load(AtomicOrdering::Acquire);
            if !scheduled && !text_compaction_needed(&self.shared) {
                return Ok(());
            }
            std::thread::sleep(Duration::from_millis(1));
        }
    }

    /// Cumulative query spill and peak-buffer statistics for this open handle.
    pub fn query_memory_stats(&self) -> QueryMemoryStats {
        QueryMemoryStats {
            spill_files: self.shared.query_spill_files.load(AtomicOrdering::Relaxed),
            spilled_bytes: self
                .shared
                .query_spilled_bytes
                .load(AtomicOrdering::Relaxed),
            peak_buffer_bytes: self
                .shared
                .query_peak_buffer_bytes
                .load(AtomicOrdering::Relaxed),
        }
    }

    /// Current database-wide admission and retained-delta accounting.
    pub fn global_memory_stats(&self) -> GlobalMemoryStats {
        self.shared.memory_governor.stats()
    }

    pub(crate) fn acquire_query_memory(&self) -> MemoryPermit {
        self.shared.memory_governor.acquire(
            MemoryPool::Query,
            self.shared.opts.memory.query_working_bytes,
        )
    }

    fn acquire_maintenance_memory(shared: &Arc<Shared>) -> MemoryPermit {
        shared.memory_governor.acquire(
            MemoryPool::Maintenance,
            shared.opts.memory.maintenance_pool_bytes,
        )
    }

    pub(crate) fn memory_options(&self) -> MemoryOptions {
        self.shared.opts.memory.clone()
    }

    pub(crate) fn record_query_buffer(&self, bytes: usize) {
        self.shared
            .query_peak_buffer_bytes
            .fetch_max(bytes as u64, AtomicOrdering::Relaxed);
    }

    pub(crate) fn record_query_spill(&self, bytes: u64) {
        self.shared
            .query_spill_files
            .fetch_add(1, AtomicOrdering::Relaxed);
        self.shared
            .query_spilled_bytes
            .fetch_add(bytes, AtomicOrdering::Relaxed);
    }

    /// Wait until a queued/running automatic compaction finishes. Primarily
    /// useful for graceful shutdown, tests, and maintenance observability.
    pub fn wait_for_automatic_compaction(&self) -> Result<()> {
        while self.shared.auto_compaction_state.lock().unwrap().scheduled {
            if !self
                .shared
                .maintenance_worker_alive
                .load(AtomicOrdering::Acquire)
            {
                return Err(maintenance_wait_error(&self.shared, "automatic compaction"));
            }
            std::thread::sleep(Duration::from_millis(1));
        }
        Ok(())
    }

    /// Rewrite segments keeping only versions visible to the latest state or
    /// to a live snapshot. Blocks writers for the duration (Phase 1).
    pub fn compact(&self) -> Result<()> {
        self.rewrite_segments(&Rewrite::None)
    }

    /// The single data-rewriting primitive, shared by compaction and by the DDL
    /// statements that must touch data. It rebuilds every segment from the
    /// versions still needed (the latest state plus every live snapshot),
    /// applying `rw` on the way, then publishes the manifest, the catalog and
    /// the in-memory state. Readers are blocked only for that final swap, so
    /// they never observe a half-applied schema change; a crash before it is
    /// finished by replaying the `ddl.json` record on the next open.
    fn rewrite_segments(&self, rw: &Rewrite<'_>) -> Result<()> {
        Self::rewrite_segments_shared(&self.shared, rw)
    }

    fn rewrite_segments_shared(shared: &Arc<Shared>, rw: &Rewrite<'_>) -> Result<()> {
        if shared.opts.read_only {
            return Err(Error::ReadOnly);
        }
        let mut cs = lock_commit_for_maintenance(shared)?;
        let _memory = Self::acquire_maintenance_memory(shared);
        // Drain the WAL first so every record lives in a segment: the rewrite
        // below is the only thing that then has to be transformed.
        checkpoint_measured(shared, &mut cs)?;

        let watermarks: Vec<u64> = {
            let snaps = shared.snapshots.lock().unwrap();
            let st = shared.state.read().unwrap();
            let mut w: Vec<u64> = snaps.keys().copied().collect();
            w.push(st.committed_version);
            w.sort_unstable();
            w.dedup();
            w
        };

        let (committed_version, seg_id, mut source_tables) = {
            let st = shared.state.read().unwrap();
            let mut tables: BTreeSet<String> = st
                .catalog
                .tables
                .iter()
                .map(|table| table.name.clone())
                .collect();
            if let Rewrite::RenameTable { to, .. } = *rw {
                // Crash replay may find that the data half already uses the
                // target name while the catalog still uses the source name.
                tables.insert(to.to_owned());
            }
            (st.committed_version, st.next_segment_id, tables)
        };
        let work_budget = (shared.opts.memory.maintenance_pool_bytes / 3).max(1);
        let seg_path = shared
            .dir
            .join(SEGMENTS_DIR)
            .join(segment_file_name(seg_id));
        let raw_segment = File::create(&seg_path)?;
        let mut segment = BufWriter::with_capacity(work_budget.min(1024 * 1024), raw_segment);
        let primary_path = primary_index_path(&shared.dir);
        let primary_tmp = primary_path.with_extension("pidx.tmp");
        let temp_dir = shared
            .opts
            .memory
            .spill_directory
            .clone()
            .unwrap_or_else(|| shared.dir.join(INDEXES_DIR).join("tmp"));
        let mut primary_writer = ExternalPagedWriter::new(&primary_tmp, &temp_dir, 0, work_budget)?;
        let blob_refs_path = shared.dir.join(INDEXES_DIR).join("blob-gc.refs.tmp");
        let mut blob_refs_writer =
            ExternalPagedWriter::new(&blob_refs_path, &temp_dir, committed_version, work_budget)?;
        let mut segment_position = 0u64;
        let mut blob_refs = Vec::new();
        let mut encoded = Vec::new();
        let mut new_high_ids = HashMap::<String, String>::new();
        let mut new_segment_superseded = false;

        {
            let st = shared.state.read().unwrap();
            for table in std::mem::take(&mut source_tables) {
                let owner = st.catalog.table(&table).or(match *rw {
                    Rewrite::RenameTable { from, to } if to == table => st.catalog.table(from),
                    _ => None,
                });
                let Some(schema) = owner else { continue };
                let epoch = schema.epoch;
                let out_table = rw.output_table(&table);
                st.index.visit_table(&table, None, |id, versions| {
                    let mut keep_versions = BTreeSet::new();
                    for &watermark in &watermarks {
                        if let Some(version) = versions
                            .iter()
                            .rev()
                            .find(|entry| entry.version <= watermark && entry.version > epoch)
                        {
                            keep_versions.insert(version.version);
                        }
                    }
                    let mut have_put = false;
                    let latest_kept = keep_versions.iter().next_back().copied();
                    for version in versions {
                        if !keep_versions.contains(&version.version) {
                            continue;
                        }
                        if Some(version.version) != latest_kept && !version.is_tombstone() {
                            new_segment_superseded = true;
                        }
                        new_high_ids
                            .entry(out_table.clone())
                            .and_modify(|high| {
                                if id > high.as_str() {
                                    *high = id.to_owned();
                                }
                            })
                            .or_insert_with(|| id.to_owned());
                        let payload = if version.is_tombstone() {
                            if !have_put {
                                continue;
                            }
                            None
                        } else {
                            have_put = true;
                            let bytes = payload_bytes(&st.readers, &version.kind)?
                                .expect("put has payload");
                            Some(transform_payload(rw, &table, bytes)?)
                        };
                        if let Some(payload) = &payload {
                            blob_refs.clear();
                            scan_payload_blob_refs(payload, &mut blob_refs)?;
                            for blob in &blob_refs {
                                blob_refs_writer.add(blob.name.as_bytes(), b"")?;
                            }
                        }
                        let payload_rel = encode_entry_into(
                            &mut encoded,
                            version.version,
                            &out_table,
                            id,
                            payload.as_deref(),
                        )?;
                        let kind = match &payload {
                            Some(payload) => VKind::SegPut {
                                segment: seg_id,
                                payload_offset: segment_position + payload_rel,
                                payload_len: payload.len() as u32,
                            },
                            None => VKind::SegTombstone,
                        };
                        segment.write_all(&encoded)?;
                        segment_position = segment_position.saturating_add(encoded.len() as u64);
                        primary_writer.add(
                            &primary_key(&out_table, id),
                            &encode_primary_entry(&VersionEntry {
                                version: version.version,
                                kind,
                            })?,
                        )?;
                    }
                    Ok(true)
                })?;
            }
        }
        segment.flush()?;
        let segment_file = segment
            .into_inner()
            .map_err(|error| Error::Io(error.into_error()))?;
        segment_file.sync_all()?;
        let mut new_segments = Vec::new();
        let mut new_readers = HashMap::new();
        if segment_position == 0 {
            fs::remove_file(&seg_path)?;
        } else {
            new_segments.push(SegmentMeta {
                id: seg_id,
                len: segment_position,
            });
            new_readers.insert(seg_id, Arc::new(File::open(&seg_path)?));
        }
        fsync_dir(&shared.dir.join(SEGMENTS_DIR))?;

        let new_catalog = {
            let st = shared.state.read().unwrap();
            let mut next = st.catalog.clone();
            rw.apply_to_catalog(&mut next);
            next
        };
        let identity_high_water: BTreeMap<String, i64> = {
            let st = shared.state.read().unwrap();
            st.identity_high_water
                .iter()
                .map(|(table, value)| (rw.output_table(table), *value))
                .collect()
        };
        let generation = primary_generation(committed_version, &new_segments, &new_catalog);
        primary_writer.set_dump_version(generation);
        primary_writer.finish()?;
        fs::rename(&primary_tmp, &primary_path)?;
        let new_primary = PrimaryIdx::paged_named(
            PagedIndex::open(&primary_path)?,
            "primary.pidx".into(),
            PRIMARY_BASE_LEVEL,
            fs::metadata(&primary_path)?.len(),
            generation,
        );
        blob_refs_writer.finish()?;
        let blob_refs_index = PagedIndex::open(&blob_refs_path)?;

        // Keep the human/tooling catalog mirror ahead of canonical publication.
        // Current engines pair schema and data through the embedded catalog in
        // the manifest, so a crash here still opens the old complete generation.
        if rw.is_ddl() {
            new_catalog.save(&shared.dir.join(CATALOG_FILE))?;
        }

        // The manifest atomically publishes both transformed data and schema.
        let manifest_outcome = Manifest {
            format_version: FORMAT_VERSION,
            committed_version,
            segments: new_segments.clone(),
            wal_id: cs.wal().id,
            identity_high_water: identity_high_water.clone(),
            catalog: Some(new_catalog.clone()),
        }
        .publish(&shared.dir)?;

        let primary_publish_error =
            publish_primary_run_manifest(&shared.dir, generation, &new_primary).err();
        // Canonical publication has completed. Adopt it in memory before
        // propagating any disposable-index error.
        {
            let mut st = shared.state.write().unwrap();
            st.catalog = new_catalog;
            st.identity_high_water = identity_high_water.into_iter().collect();
            st.index = new_primary;
            st.table_high_ids = new_high_ids;
            st.superseded_segments = if new_segment_superseded && segment_position > 0 {
                HashSet::from([seg_id])
            } else {
                HashSet::new()
            };
            st.readers = new_readers;
            st.segments = new_segments;
            st.next_segment_id = seg_id + 1;
            if rw.is_ddl() {
                st.secondary.clear();
            }
            st.vector.clear();
            st.text.clear();
        }
        let publication_error =
            publication_sync_error(shared, "compaction manifest", manifest_outcome);
        if let Some(error) = primary_publish_error {
            if publication_error.is_none() {
                fence_writes(
                    shared,
                    format!(
                        "canonical rewrite published but primary index publication failed: {error}"
                    ),
                );
            }
            return Err(publication_error.unwrap_or(error));
        }
        if let Err(error) = rebuild_derived_indexes_after_rewrite(shared, rw.is_ddl()) {
            if publication_error.is_none() {
                fence_writes(
                    shared,
                    format!(
                        "canonical rewrite published but derived index rebuild failed: {error}"
                    ),
                );
            }
            return Err(publication_error.unwrap_or(error));
        }
        cleanup_primary_run_orphans(&shared.dir);
        let retained = shared.state.read().unwrap().index_delta_memory_bytes();
        shared.memory_governor.set_index_delta_bytes(retained);
        let catalog = shared.state.read().unwrap().catalog.clone();
        cleanup_orphan_sidx(&shared.dir, &catalog);
        cleanup_orphan_tidx(&shared.dir, &catalog);
        cleanup_orphan_vidx(&shared.dir, &catalog);
        if publication_error.is_none() {
            cleanup_orphans(
                &shared.dir,
                &Manifest {
                    format_version: FORMAT_VERSION,
                    committed_version,
                    segments: shared.state.read().unwrap().segments.clone(),
                    wal_id: cs.wal().id,
                    identity_high_water: identity_manifest(&shared.state.read().unwrap()),
                    catalog: Some(shared.state.read().unwrap().catalog.clone()),
                },
            )?;
        }

        // Blob references were externally sorted while the segment streamed;
        // no HashSet proportional to the number of chunks is retained.
        let blobs_dir = shared.dir.join(BLOBS_DIR);
        if publication_error.is_none() {
            let pending = shared.pending_blob_publications.lock().unwrap();
            gc_blob_files(&blobs_dir, |stem| {
                if pending.contains(stem) {
                    return true;
                }
                let mut found = false;
                blob_refs_index
                    .visit_key(stem.as_bytes(), |_| {
                        found = true;
                        Ok(false)
                    })
                    .map(|()| found)
                    .unwrap_or(false)
            });
        }
        drop(blob_refs_index);
        let _ = fs::remove_file(&blob_refs_path);
        reset_compaction_debt(shared);
        publication_error.map_or(Ok(()), Err)
    }
}

impl Txn {
    fn stage(&mut self, table: &str, id: String, operation: Option<Record>) -> Result<()> {
        let table_index = self
            .staged
            .iter()
            .position(|(name, _)| name == table)
            .expect("schema is cached before staging");
        let old_bytes = self.staged[table_index]
            .1
            .operations
            .get(&id)
            .map_or(0, |old| staged_operation_bytes(table, &id, &old.operation));
        let new_bytes = staged_operation_bytes(table, &id, &operation);
        let projected = self
            .staged_bytes
            .saturating_sub(old_bytes)
            .saturating_add(new_bytes);
        let limit = self.shared.opts.memory.index_delta_pool_bytes;
        if projected > limit {
            return Err(Error::MemoryLimit(format!(
                "transaction staging needs an estimated {projected} bytes, but memory.index_delta_pool_bytes is {limit}; commit smaller batches"
            )));
        }
        let staged_table = &mut self.staged[table_index].1;
        if let Some(existing) = staged_table.operations.get_mut(&id) {
            existing.operation = operation;
            existing.deleted_record = None;
        } else {
            let position = staged_table.next_position;
            staged_table.next_position = staged_table.next_position.saturating_add(1);
            staged_table.operations.insert(
                id,
                StagedOperation {
                    position,
                    operation,
                    deleted_record: None,
                },
            );
        }
        self.staged_bytes = projected;
        Ok(())
    }

    pub fn snapshot_version(&self) -> u64 {
        self.snapshot.version
    }

    pub fn elapsed(&self) -> Duration {
        self.started_at.elapsed()
    }

    /// Cached schema lookup: clones from the catalog once per table.
    fn schema(&mut self, table: &str) -> Result<&TableSchema> {
        let table_index =
            if let Some(index) = self.staged.iter().position(|(name, _)| name == table) {
                index
            } else {
                let st = self.shared.state.read().unwrap();
                let schema = st
                    .catalog
                    .table(table)
                    .ok_or_else(|| Error::TableNotFound(table.into()))?
                    .clone();
                let snapshot_high_id = st.table_high_ids.get(table).cloned();
                self.staged.push((
                    table.to_owned(),
                    StagedTable {
                        schema,
                        snapshot_high_id,
                        operations: HashMap::new(),
                        next_position: 0,
                    },
                ));
                self.staged.len() - 1
            };
        Ok(&self.staged[table_index].1.schema)
    }

    pub(crate) fn table_schema(&mut self, table: &str) -> Result<TableSchema> {
        Ok(self.schema(table)?.clone())
    }

    /// Execute one SQL statement against this transaction's snapshot and
    /// staged writes. DDL and multi-table/aggregate SELECT are deliberately
    /// excluded from the transactional SQL surface.
    pub fn query(&mut self, sql: &str) -> Result<crate::sql::QueryOutput> {
        crate::sql::execute_txn(self, sql)
    }

    pub fn query_params(&mut self, sql: &str, params: &[Value]) -> Result<crate::sql::QueryOutput> {
        crate::sql::execute_txn_positional(self, sql, params)
    }

    pub fn query_named(&mut self, sql: &str, params: &Record) -> Result<crate::sql::QueryOutput> {
        crate::sql::execute_txn_named(self, sql, params)
    }

    pub(crate) fn query_params_bounded(
        &mut self,
        sql: &str,
        params: &[Value],
        max_rows: usize,
    ) -> Result<crate::sql::QueryOutput> {
        crate::sql::execute_txn_positional_bounded(self, sql, params, max_rows)
    }

    pub(crate) fn query_named_bounded(
        &mut self,
        sql: &str,
        params: &Record,
        max_rows: usize,
    ) -> Result<crate::sql::QueryOutput> {
        crate::sql::execute_txn_named_bounded(self, sql, params, max_rows)
    }

    /// Read through this transaction: staged writes first, then the snapshot.
    pub fn get(&self, table: &str, id: &str) -> Result<Option<Record>> {
        let has_implicit_id = self
            .staged
            .iter()
            .find(|(name, _)| name == table)
            .map(|(_, staged)| staged.schema.has_implicit_id())
            .or_else(|| {
                self.shared
                    .state
                    .read()
                    .unwrap()
                    .catalog
                    .table(table)
                    .map(TableSchema::has_implicit_id)
            })
            .ok_or_else(|| Error::TableNotFound(table.into()))?;
        match self
            .staged
            .iter()
            .find(|(name, _)| name == table)
            .and_then(|(_, staged)| staged.operations.get(id))
            .map(|staged| &staged.operation)
        {
            Some(None) => Ok(None),
            Some(Some(rec)) => {
                let mut rec = rec.clone();
                if has_implicit_id {
                    rec.insert(ID_COLUMN.into(), Value::Text(id.to_owned()));
                }
                Ok(Some(rec))
            }
            None => shared_get_at(&self.shared, table, id, self.snapshot.version),
        }
    }

    /// Scan through the transaction's stable snapshot with staged writes
    /// overlaid. This is also the view used to discover cascade targets.
    pub fn scan(&self, table: &str) -> Result<Vec<(String, Record)>> {
        let mut rows: BTreeMap<String, Record> =
            shared_scan_at(&self.shared, table, self.snapshot.version)?
                .into_iter()
                .collect();
        if let Some((_, staged)) = self.staged.iter().find(|(name, _)| name == table) {
            for (id, operation) in &staged.operations {
                match &operation.operation {
                    Some(record) => {
                        let mut record = record.clone();
                        if staged.schema.has_implicit_id() {
                            record.insert(ID_COLUMN.into(), Value::Text(id.clone()));
                        }
                        rows.insert(id.clone(), record);
                    }
                    None => {
                        rows.remove(id);
                    }
                }
            }
        }
        Ok(rows.into_iter().collect())
    }

    pub fn insert(&mut self, table: &str, record: Record) -> Result<String> {
        let mut record = record;
        self.schema(table)?;
        let table_index = self
            .staged
            .iter()
            .position(|(name, _)| name == table)
            .expect("schema was cached above");
        let schema = &self.staged[table_index].1.schema;
        let id = match (schema.has_implicit_id(), record.get(ID_COLUMN)) {
            (false, _) | (true, None) => {
                let mut previous = self.shared.last_generated_id.lock().unwrap();
                let candidate = Ulid::new();
                let next = if candidate > *previous {
                    candidate
                } else {
                    previous.increment().ok_or_else(|| {
                        Error::InvalidArgument("cannot generate id: ULID overflow".into())
                    })?
                };
                *previous = next;
                next.to_string()
            }
            (true, Some(Value::Text(s))) if !s.is_empty() => {
                validate_short_string("record id", s)?;
                if let Ok(explicit) = Ulid::from_string(s) {
                    let mut previous = self.shared.last_generated_id.lock().unwrap();
                    if explicit > *previous {
                        *previous = explicit;
                    }
                }
                s.clone()
            }
            (true, Some(Value::Text(_))) => {
                return Err(Error::InvalidArgument("id must not be empty".into()))
            }
            (true, Some(_)) => {
                return Err(Error::SchemaViolation("id must be a text value".into()))
            }
        };
        let identity_name = schema
            .columns
            .iter()
            .find(|column| column.identity)
            .map(|column| column.name.clone());
        if let Some(identity_name) = identity_name {
            let explicit = match record.get(&identity_name) {
                None => None,
                Some(Value::Int64(value)) if *value >= 1 => Some(*value),
                Some(Value::Int64(_)) => {
                    return Err(Error::SchemaViolation(format!(
                        "identity column '{identity_name}' requires a positive int"
                    )))
                }
                Some(_) => {
                    return Err(Error::SchemaViolation(format!(
                        "identity column '{identity_name}' requires an int"
                    )))
                }
            };
            let generated = reserve_identity(&self.shared, table, explicit)?;
            record.insert(identity_name, Value::Int64(generated));
        }
        let normalized = normalize_record(schema, record)?;
        let staged_table = &self.staged[table_index].1;
        let exists = match staged_table
            .operations
            .get(&id)
            .map(|staged| &staged.operation)
        {
            Some(Some(_)) => true,
            Some(None) => false,
            None => {
                let above_snapshot_high = staged_table
                    .snapshot_high_id
                    .as_deref()
                    .is_none_or(|high| id.as_str() > high);
                if above_snapshot_high {
                    false
                } else {
                    self.shared
                        .state
                        .read()
                        .unwrap()
                        .visible_owned(table, &id, self.snapshot.version)
                        .ok()
                        .flatten()
                        .is_some_and(|entry| !entry.is_tombstone())
                }
            }
        };
        if exists {
            return Err(Error::DuplicateId {
                table: table.into(),
                id,
            });
        }
        self.stage(table, id.clone(), Some(normalized))?;
        Ok(id)
    }

    pub fn update(&mut self, table: &str, id: &str, patch: Record) -> Result<()> {
        self.schema(table)?; // cache + existence check
        let has_implicit_id = self
            .staged
            .iter()
            .find(|(name, _)| name == table)
            .expect("schema was cached above")
            .1
            .schema
            .has_implicit_id();
        if has_implicit_id && patch.contains_key(ID_COLUMN) {
            return Err(Error::InvalidArgument(
                "the primary key cannot be updated".into(),
            ));
        }
        {
            let st = self.shared.state.read().unwrap();
            for schema in &st.catalog.tables {
                for foreign_key in &schema.foreign_keys {
                    if foreign_key.referenced_table == table
                        && patch.contains_key(&foreign_key.referenced_column)
                    {
                        return Err(Error::SchemaViolation(format!(
                            "cannot update referenced key {table}.{}; ON UPDATE is not supported",
                            foreign_key.referenced_column
                        )));
                    }
                }
            }
        }
        let mut current = self.get(table, id)?.ok_or_else(|| Error::RecordNotFound {
            table: table.into(),
            id: id.into(),
        })?;
        if has_implicit_id {
            current.remove(ID_COLUMN);
        }
        let schema = &self
            .staged
            .iter()
            .find(|(name, _)| name == table)
            .expect("cached above")
            .1
            .schema;
        for (name, value) in patch {
            let col = schema
                .column(&name)
                .ok_or_else(|| Error::SchemaViolation(format!("unknown column '{name}'")))?;
            if col.identity {
                return Err(Error::InvalidArgument(format!(
                    "identity column '{}' cannot be updated",
                    col.name
                )));
            }
            check_value(col, &value)?;
            current.insert(name, value);
        }
        self.stage(table, id.to_owned(), Some(current))
    }

    pub fn delete(&mut self, table: &str, id: &str) -> Result<bool> {
        self.schema(table)?;
        let Some(current) = self.get(table, id)? else {
            return Ok(false);
        };
        self.stage(table, id.to_owned(), None)?;
        let staged = self
            .staged
            .iter_mut()
            .find(|(name, _)| name == table)
            .and_then(|(_, staged)| staged.operations.get_mut(id))
            .expect("delete was just staged");
        staged.deleted_record = Some(current);
        Ok(true)
    }

    /// Validate optimistically and publish all staged writes as one atomic
    /// commit. Returns the commit version, or `Error::Conflict` if any
    /// touched record changed after this transaction began.
    pub fn commit(mut self) -> Result<u64> {
        self.expand_delete_cascades()?;
        commit_staged(&self.shared, self.snapshot.version, self.staged)
    }

    /// Discard all staged writes.
    pub fn rollback(self) {}

    fn expand_delete_cascades(&mut self) -> Result<()> {
        const MAX_CASCADE_DEPTH: usize = 64;
        const MAX_CASCADE_ROWS: usize = 100_000;

        let catalog = self.shared.state.read().unwrap().catalog.clone();
        let mut queue = VecDeque::new();
        let mut visited = HashSet::new();
        for (table, staged) in &self.staged {
            for (id, operation) in &staged.operations {
                if operation.operation.is_none() {
                    if let Some(record) = &operation.deleted_record {
                        queue.push_back((table.clone(), id.clone(), record.clone(), 0usize));
                        visited.insert((table.clone(), id.clone()));
                    }
                }
            }
        }

        while let Some((parent_table, _parent_id, parent_record, depth)) = queue.pop_front() {
            if depth >= MAX_CASCADE_DEPTH {
                return Err(Error::SchemaViolation(format!(
                    "ON DELETE CASCADE exceeded maximum depth {MAX_CASCADE_DEPTH}"
                )));
            }
            for child_schema in &catalog.tables {
                for foreign_key in child_schema.foreign_keys.iter().filter(|foreign_key| {
                    foreign_key.referenced_table == parent_table
                        && foreign_key.on_delete == ReferentialAction::Cascade
                }) {
                    let Some(parent_value) = parent_record.get(&foreign_key.referenced_column)
                    else {
                        continue;
                    };
                    if parent_value.is_null() {
                        continue;
                    }
                    let matches: Vec<_> = self
                        .scan(&child_schema.name)?
                        .into_iter()
                        .filter(|(_, record)| record.get(&foreign_key.column) == Some(parent_value))
                        .collect();
                    for (child_id, child_record) in matches {
                        let key = (child_schema.name.clone(), child_id.clone());
                        if !visited.insert(key) {
                            continue;
                        }
                        if visited.len() > MAX_CASCADE_ROWS {
                            return Err(Error::MemoryLimit(format!(
                                "ON DELETE CASCADE exceeds {MAX_CASCADE_ROWS} rows"
                            )));
                        }
                        self.schema(&child_schema.name)?;
                        self.stage(&child_schema.name, child_id.clone(), None)?;
                        let operation = self
                            .staged
                            .iter_mut()
                            .find(|(name, _)| name == &child_schema.name)
                            .and_then(|(_, staged)| staged.operations.get_mut(&child_id))
                            .expect("cascade delete was just staged");
                        operation.deleted_record = Some(child_record.clone());
                        queue.push_back((
                            child_schema.name.clone(),
                            child_id,
                            child_record,
                            depth + 1,
                        ));
                    }
                }
            }
        }
        Ok(())
    }
}

fn reserve_identity(shared: &Shared, table: &str, explicit: Option<i64>) -> Result<i64> {
    // Identity allocation mutates publication state before commit staging.
    // Announce that brief writer section so a saturated point-read workload
    // cannot repeatedly reacquire the state lock ahead of the allocator.
    let _active_state_writer = ActiveStateWriter::enter(shared);
    let mut st = shared.state.write().unwrap();
    let high = st.identity_high_water.entry(table.to_owned()).or_insert(0);
    match explicit {
        Some(value) => {
            *high = (*high).max(value);
            Ok(value)
        }
        None => {
            let next = high.checked_add(1).ok_or_else(|| {
                Error::SchemaViolation(format!("identity sequence for '{table}' is exhausted"))
            })?;
            *high = next;
            Ok(next)
        }
    }
}

fn staged_operation_bytes(table: &str, id: &str, operation: &Option<Record>) -> usize {
    let record_bytes = operation.as_ref().map_or(0, |record| {
        record
            .iter()
            .map(|(name, value)| {
                name.len()
                    + 40
                    + match value {
                        Value::Text(text) => text.len() + 24,
                        Value::Blob(blob) => blob.len() + 24,
                        Value::Vector(vector) => vector.len() * std::mem::size_of::<f32>() + 24,
                        _ => std::mem::size_of::<Value>(),
                    }
            })
            .sum::<usize>()
    });
    table
        .len()
        .saturating_add(id.len())
        .saturating_add(record_bytes)
        .saturating_add(128)
}

// --- internals ----------------------------------------------------------------

fn segment_bytes(shared: &Shared) -> u64 {
    shared
        .state
        .read()
        .unwrap()
        .segments
        .iter()
        .map(|segment| segment.len)
        .sum()
}

fn segment_entry_bytes(table: &str, id: &str, entry: &VersionEntry) -> u64 {
    let payload = match entry.kind {
        VKind::SegPut { payload_len, .. } => payload_len as u64,
        VKind::SegTombstone => 0,
        VKind::MemPut(_) | VKind::MemTombstone => return 0,
    };
    // kind + version + table framing + id framing + payload framing + crc
    1 + 8 + 2 + table.len() as u64 + 2 + id.len() as u64 + 4 + payload + 4
}

/// Approximate the immutable bytes that a superseded in-memory or segment
/// entry contributes after its next checkpoint. This is deliberately an
/// upper-bound trigger estimate: compaction still computes the exact
/// snapshot-safe reclaimable set before publishing anything.
fn obsolete_entry_bytes_estimate(table: &str, id: &str, entry: &VersionEntry) -> u64 {
    let payload = match &entry.kind {
        VKind::MemPut(payload) => payload.len() as u64,
        VKind::SegPut { payload_len, .. } => *payload_len as u64,
        VKind::MemTombstone | VKind::SegTombstone => 0,
    };
    1 + 8 + 2 + table.len() as u64 + 2 + id.len() as u64 + 4 + payload + 4
}

/// Estimate what a plain compaction could remove while preserving every live
/// snapshot. Segment lengths also account for catalog-unreachable entries
/// (for example a dropped table already pruned from the in-memory index).
fn compaction_debt_estimate(shared: &Shared, track_retained_snapshots: bool) -> (u64, u64) {
    let snapshots = shared.snapshots.lock().unwrap();
    let st = shared.state.read().unwrap();
    let mut watermarks: Vec<u64> = snapshots.keys().copied().collect();
    if track_retained_snapshots {
        *shared.compaction_retained_snapshots.lock().unwrap() = snapshots.keys().copied().collect();
    }
    watermarks.push(st.committed_version);
    watermarks.sort_unstable();
    watermarks.dedup();

    let total_segment_bytes: u64 = st.segments.iter().map(|segment| segment.len).sum();
    let mut kept_segment_bytes = 0u64;
    let mut obsolete_entries = 0u64;
    for schema in &st.catalog.tables {
        let table = schema.name.as_str();
        let _ = st.index.visit_table(table, None, |id, versions| {
            let mut keep_versions = BTreeSet::new();
            for &watermark in &watermarks {
                if let Some(entry) = versions
                    .iter()
                    .rev()
                    .find(|entry| entry.version <= watermark && entry.version > schema.epoch)
                {
                    keep_versions.insert(entry.version);
                }
            }
            let mut have_put = false;
            for entry in versions {
                let selected = keep_versions.contains(&entry.version);
                let kept = if !selected {
                    false
                } else if entry.is_tombstone() {
                    have_put
                } else {
                    have_put = true;
                    true
                };
                if kept {
                    kept_segment_bytes =
                        kept_segment_bytes.saturating_add(segment_entry_bytes(table, id, entry));
                } else {
                    obsolete_entries = obsolete_entries.saturating_add(1);
                }
            }
            Ok(true)
        });
    }
    (
        obsolete_entries,
        total_segment_bytes.saturating_sub(kept_segment_bytes),
    )
}

fn refresh_compaction_debt(shared: &Shared) {
    let (operations, bytes) = compaction_debt_estimate(shared, false);
    let mut auto = shared.auto_compaction_state.lock().unwrap();
    auto.debt_operations = auto.debt_operations.max(operations);
    auto.estimated_reclaimable_bytes = bytes;
}

fn refresh_compaction_debt_if_needed(shared: &Shared) {
    if shared
        .compaction_refresh_needed
        .swap(false, AtomicOrdering::AcqRel)
    {
        refresh_compaction_debt(shared);
    }
}

fn reset_compaction_debt(shared: &Shared) {
    let (operations, bytes) = compaction_debt_estimate(shared, true);
    let mut auto = shared.auto_compaction_state.lock().unwrap();
    auto.debt_operations = operations;
    auto.estimated_reclaimable_bytes = bytes;
}

fn auto_compaction_needed(shared: &Shared) -> bool {
    let options = &shared.opts.auto_compaction;
    if shared.opts.read_only || !options.enabled {
        return false;
    }
    let (segments, total_bytes) = {
        let st = shared.state.read().unwrap();
        (
            st.segments.len(),
            st.segments.iter().map(|segment| segment.len).sum::<u64>(),
        )
    };
    let auto = shared.auto_compaction_state.lock().unwrap();
    if auto
        .last_attempt
        .is_some_and(|last| last.elapsed() < Duration::from_millis(options.min_interval_ms))
    {
        return false;
    }
    if segments >= options.max_segments {
        return true;
    }
    if auto.estimated_reclaimable_bytes >= options.force_reclaim_bytes {
        return true;
    }
    if auto.debt_operations < options.min_obsolete_operations || total_bytes == 0 {
        return false;
    }
    let ratio = (auto.estimated_reclaimable_bytes as u128 * 100) / total_bytes as u128;
    ratio >= options.min_reclaim_ratio_percent as u128
}

fn maybe_schedule_auto_compaction(shared: &Shared) {
    if !auto_compaction_needed(shared) {
        return;
    }
    let sender = shared.maintenance_tx.lock().unwrap().as_ref().cloned();
    let Some(sender) = sender else { return };
    {
        let mut auto = shared.auto_compaction_state.lock().unwrap();
        if auto.scheduled {
            return;
        }
        auto.scheduled = true;
    }
    if sender.send(MaintenanceJob::Compact).is_err() {
        shared.auto_compaction_state.lock().unwrap().scheduled = false;
    }
}

pub(crate) fn acquire_lock(dir: &Path, shared: bool) -> Result<File> {
    let file = OpenOptions::new()
        .create(true)
        .truncate(false)
        .write(true)
        .open(dir.join(LOCK_FILE))?;
    let locked = if shared {
        file.try_lock_shared()
    } else {
        file.try_lock()
    };
    match locked {
        Ok(()) => Ok(file),
        Err(TryLockError::WouldBlock) => Err(Error::DatabaseLocked(dir.display().to_string())),
        Err(TryLockError::Error(e)) => Err(Error::Io(e)),
    }
}

fn lock_creation_destination(dir: &Path) -> Result<File> {
    let parent = dir
        .parent()
        .filter(|path| !path.as_os_str().is_empty())
        .unwrap_or(Path::new("."));
    fs::create_dir_all(parent)?;
    let name = dir.file_name().ok_or_else(|| {
        Error::InvalidArgument(format!("invalid database path: {}", dir.display()))
    })?;
    let path = parent.join(format!(".{}.elitesql-create-lock", name.to_string_lossy()));
    let file = OpenOptions::new()
        .create(true)
        .truncate(false)
        .write(true)
        .open(path)?;
    file.lock()?;
    Ok(file)
}

fn prepare_creation_directory(dir: &Path) -> Result<()> {
    if dir.join(MARKER_FILE).exists() {
        return Err(Error::InvalidArgument(format!(
            "database already exists at {}",
            dir.display()
        )));
    }
    if dir.exists() {
        if !dir.is_dir() {
            return Err(Error::InvalidArgument(format!(
                "database path is not a directory: {}",
                dir.display()
            )));
        }
        let entries = fs::read_dir(dir)?.collect::<std::io::Result<Vec<_>>>()?;
        if !entries.is_empty() {
            let owned_partial =
                fs::read(dir.join(CREATION_FILE)).is_ok_and(|bytes| bytes == CREATION_MAGIC);
            if !owned_partial {
                return Err(Error::InvalidArgument(format!(
                    "refusing to initialize non-empty directory without an EliteSQL creation journal: {}",
                    dir.display()
                )));
            }
            if let Some(entry) = entries
                .iter()
                .find(|entry| !is_creation_artifact(&entry.file_name()))
            {
                return Err(Error::InvalidArgument(format!(
                    "refusing to remove unknown file '{}' while recovering an incomplete database creation",
                    entry.path().display()
                )));
            }
            for entry in entries {
                if entry.file_type()?.is_dir() {
                    fs::remove_dir_all(entry.path())?;
                } else {
                    fs::remove_file(entry.path())?;
                }
            }
        }
    } else {
        fs::create_dir(dir)?;
    }
    let journal_path = dir.join(CREATION_FILE);
    let mut journal = File::create(journal_path)?;
    journal.write_all(CREATION_MAGIC)?;
    journal.sync_all()?;
    fsync_dir(dir)
}

fn is_creation_artifact(name: &std::ffi::OsStr) -> bool {
    matches!(
        name.to_str(),
        Some(
            CREATION_FILE
                | CATALOG_FILE
                | "catalog.json.tmp"
                | "manifest"
                | "manifest.prev"
                | "manifest.tmp"
                | "manifest.prev.tmp"
                | LOCK_FILE
                | SEGMENTS_DIR
                | WAL_DIR
                | VECTORS_DIR
                | INDEXES_DIR
                | BLOBS_DIR
                | ".ELITESQL.tmp"
        )
    )
}

fn cleanup_orphans(dir: &Path, manifest: &Manifest) -> Result<()> {
    let previous = Manifest::previous(dir);
    let mut listed: HashSet<u32> = manifest.segments.iter().map(|s| s.id).collect();
    if let Some(previous) = &previous {
        listed.extend(previous.segments.iter().map(|segment| segment.id));
    }
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
                let is_previous = previous
                    .as_ref()
                    .is_some_and(|previous| id == previous.wal_id);
                if id < manifest.wal_id && !is_previous {
                    let _ = fs::remove_file(dirent.path());
                }
            }
        }
    }
    Ok(())
}

fn cleanup_blob_staging(dir: &Path) {
    let Ok(entries) = fs::read_dir(dir.join(BLOBS_DIR)) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path
            .file_name()
            .and_then(|name| name.to_str())
            .is_some_and(|name| name.ends_with(".blob.tmp"))
        {
            let _ = fs::remove_file(path);
        }
    }
}

fn cleanup_wal_bridge_staging(dir: &Path) {
    let Ok(entries) = fs::read_dir(dir.join(WAL_DIR)) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path
            .file_name()
            .and_then(|name| name.to_str())
            .is_some_and(|name| name.ends_with(".wal.bridge.tmp"))
        {
            let _ = fs::remove_file(path);
        }
    }
}

fn gc_blob_files(blobs_dir: &Path, mut referenced: impl FnMut(&str) -> bool) {
    let Ok(entries) = fs::read_dir(blobs_dir) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        // In-flight commits prepare `<name>.blob.tmp` without the commit
        // mutex. Only final chunks participate in GC; stale temporaries are
        // removed on the next writable open.
        if path.extension().is_none_or(|extension| extension != "blob") {
            continue;
        }
        let keep = path
            .file_stem()
            .map(|stem| referenced(&stem.to_string_lossy()))
            .unwrap_or(false);
        if !keep {
            let _ = fs::remove_file(path);
        }
    }
}

/// Apply a DDL rewrite to one record payload. Values are copied byte for byte,
/// so out-of-line blob references keep pointing at the chunks they already own.
fn transform_payload(rw: &Rewrite<'_>, table: &str, payload: Vec<u8>) -> Result<Vec<u8>> {
    match *rw {
        Rewrite::RenameColumn { table: t, from, to } if t == table => {
            rewrite_payload_columns(&payload, Some((from, to)), None)
        }
        Rewrite::DropColumn { table: t, column } if t == table => {
            rewrite_payload_columns(&payload, None, Some(column))
        }
        _ => Ok(payload),
    }
}

/// Rebuild an encoded record with one field renamed and/or one field removed.
fn rewrite_payload_columns(
    payload: &[u8],
    rename: Option<(&str, &str)>,
    remove: Option<&str>,
) -> Result<Vec<u8>> {
    let mut pos = 0usize;
    let count = read_u16(payload, &mut pos)? as usize;
    let mut fields: Vec<(&str, &[u8])> = Vec::with_capacity(count);
    for _ in 0..count {
        let name = read_payload_name(payload, &mut pos)?;
        let value_start = pos;
        skip_value(payload, &mut pos, None)?;
        if remove == Some(name) {
            continue;
        }
        let out_name = match rename {
            Some((from, to)) if from == name => to,
            _ => name,
        };
        fields.push((out_name, &payload[value_start..pos]));
    }
    let mut buf = Vec::with_capacity(payload.len());
    let field_count = u16::try_from(fields.len())
        .map_err(|_| Error::InvalidArgument("record has more than 65535 stored columns".into()))?;
    buf.extend_from_slice(&field_count.to_le_bytes());
    for (name, value) in &fields {
        let name_len = u16::try_from(name.len()).map_err(|_| {
            Error::InvalidArgument("column name exceeds the 65535-byte storage limit".into())
        })?;
        buf.extend_from_slice(&name_len.to_le_bytes());
        buf.extend_from_slice(name.as_bytes());
        buf.extend_from_slice(value);
    }
    Ok(buf)
}

/// Whether an encoded record has no value for a column: either the field is
/// absent (written before the column existed) or it holds NULL. Only the value
/// tag is read, so nothing large is decoded.
fn encoded_column_needs_fill(payload: &[u8], column: &str) -> Result<bool> {
    let mut pos = 0usize;
    let count = read_u16(payload, &mut pos)? as usize;
    for _ in 0..count {
        let name = read_payload_name(payload, &mut pos)?;
        if name == column {
            let tag = *payload
                .get(pos)
                .ok_or_else(|| Error::Corrupt("unexpected end of record".into()))?;
            return Ok(tag == TAG_NULL);
        }
        skip_value(payload, &mut pos, None)?;
    }
    Ok(true)
}

/// Read one length-prefixed column name from a record payload.
fn read_payload_name<'p>(payload: &'p [u8], pos: &mut usize) -> Result<&'p str> {
    let name_len = read_u16(payload, pos)? as usize;
    let end = pos
        .checked_add(name_len)
        .filter(|end| *end <= payload.len())
        .ok_or_else(|| Error::Corrupt("unexpected end of record".into()))?;
    let name = std::str::from_utf8(&payload[*pos..end])
        .map_err(|_| Error::Corrupt("invalid utf8 in column name".into()))?;
    *pos = end;
    Ok(name)
}

fn shared_get_at(
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
    match st.visible_owned(table, id, max_version)? {
        Some(entry) if !entry.is_tombstone() => {
            let mut record = read_record_kind(&st.blobs, &st.readers, &entry.kind)?;
            if schema.has_implicit_id() {
                record.insert(ID_COLUMN.into(), Value::Text(id.to_owned()));
            }
            Ok(Some(record))
        }
        _ => Ok(None),
    }
}

fn shared_scan_at(shared: &Shared, table: &str, max_version: u64) -> Result<Vec<(String, Record)>> {
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

fn shared_scan_batch_at(
    shared: &Shared,
    table: &str,
    max_version: u64,
    after_id: Option<&str>,
    limit: usize,
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
    let mut prepared = Vec::with_capacity(limit);
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
    for (id, kind) in prepared {
        let mut record = read_record_kind(&blobs, &readers, &kind)?;
        if implicit_id {
            record.insert(ID_COLUMN.into(), Value::Text(id.clone()));
        }
        out.push((id, record));
    }
    Ok(out)
}

/// The optimistic commit path. Serialized by the commit mutex; readers are
/// only blocked during the short in-memory apply at the end.
fn finish_or_wait_wal_sync(
    shared: &Arc<Shared>,
    group: Arc<WalSyncGroup>,
    leader: bool,
) -> WalGroupResult {
    if !leader {
        return group.wait();
    }

    // Only a contended writer opens the coalescing window. Uncontended Safe
    // commits retain their original one-sync latency.
    if shared.commit_waiters.load(AtomicOrdering::Acquire) > 0
        && shared.opts.safe_group_commit_delay_us > 0
    {
        let coalesce_started = Instant::now();
        std::thread::sleep(Duration::from_micros(
            shared.opts.safe_group_commit_delay_us,
        ));
        shared.wal_group_coalesce_nanos.fetch_add(
            elapsed_nanos(coalesce_started.elapsed()),
            AtomicOrdering::Relaxed,
        );
    }

    let leader_lock_started = Instant::now();
    let mut cs = shared.commit.lock();
    shared.wal_group_leader_lock_wait_nanos.fetch_add(
        elapsed_nanos(leader_lock_started.elapsed()),
        AtomicOrdering::Relaxed,
    );
    debug_assert!(
        cs.wal_sync_group
            .as_ref()
            .is_some_and(|active| Arc::ptr_eq(active, &group)),
        "the elected WAL sync group stays active until its leader completes"
    );
    let sync_started = Instant::now();
    let outcome = cs.wal().sync_data();
    let sync_time = sync_started.elapsed();
    cs.wal_sync_group = None;
    let commits = group.commits.load(AtomicOrdering::Acquire);
    let bytes = group.bytes.load(AtomicOrdering::Acquire);
    record_wal_sync(shared, sync_time, commits, bytes);
    drop(cs);

    let result = match outcome {
        WalAppendOutcome::Complete => Ok(()),
        WalAppendOutcome::SyncFailed(error) => Err(error.to_string()),
    };
    group.complete(result.clone());
    result
}

fn elapsed_nanos(duration: Duration) -> u64 {
    duration.as_nanos().min(u64::MAX as u128) as u64
}

fn record_wal_sync(shared: &Shared, elapsed: Duration, commits: u64, bytes: u64) {
    shared.wal_sync_count.fetch_add(1, AtomicOrdering::Relaxed);
    shared
        .wal_sync_nanos
        .fetch_add(elapsed_nanos(elapsed), AtomicOrdering::Relaxed);
    shared
        .wal_synced_bytes
        .fetch_add(bytes, AtomicOrdering::Relaxed);
    shared
        .wal_sync_max_group_commits
        .fetch_max(commits, AtomicOrdering::Relaxed);
    shared
        .wal_sync_max_group_bytes
        .fetch_max(bytes, AtomicOrdering::Relaxed);
    if commits > 1 {
        shared
            .grouped_commit_count
            .fetch_add(commits, AtomicOrdering::Relaxed);
    }
}

fn lock_commit_for_transaction(shared: &Shared) -> TimedCommitGuard<'_> {
    TimedCommitGuard {
        guard: Some(shared.commit.lock()),
        started: Instant::now(),
        total_nanos: &shared.commit_lock_hold_nanos,
        waiters: &shared.commit_waiters,
        // Safe group commit benefits from allowing the current CPU to append
        // several queued records before the elected leader fsyncs them. Fast
        // and Balanced need fair handoff to bound mutex-tail latency instead.
        fair_handoff: shared.opts.durability != Durability::Safe,
    }
}

fn commit_staged(
    shared: &Arc<Shared>,
    snap_version: u64,
    staged_tables: Vec<(String, StagedTable)>,
) -> Result<u64> {
    if shared.opts.read_only {
        return Err(Error::ReadOnly);
    }
    ensure_canonical_writable(shared)?;
    if staged_tables
        .iter()
        .all(|(_, staged_table)| staged_table.operations.is_empty())
    {
        return Ok(shared.state.read().unwrap().committed_version);
    }
    let _active_state_writer = ActiveStateWriter::enter(shared);
    let prepared = prepare_commit(shared, snap_version, staged_tables);
    let prepared = prepared?;
    if is_coordinated_insert_candidate(&prepared) {
        coordinate_commit(shared, prepared)
    } else {
        finish_prepared_commit(shared, prepared)
    }
}

fn is_coordinated_insert_candidate(prepared: &PreparedCommit) -> bool {
    prepared.preencoded_wal.is_some()
        && prepared.staged.iter().all(|table| {
            table.schema.indexes.is_empty()
                && table.schema.text_indexes.is_empty()
                && table.schema.vector_indexes.is_empty()
                && table.schema.foreign_keys.is_empty()
                && !table.schema.columns.iter().any(|column| column.identity)
                && table
                    .changes
                    .iter()
                    .all(|change| change.operation.is_some())
        })
}

fn prepare_commit(
    shared: &Arc<Shared>,
    snap_version: u64,
    staged_tables: Vec<(String, StagedTable)>,
) -> Result<PreparedCommit> {
    let commit_started = Instant::now();
    let prepare_started = Instant::now();
    let mut blob_sink = BlobSink::new(&shared.dir, shared.opts.external_blob_threshold);
    let mut staged = Vec::with_capacity(staged_tables.len());
    for (name, staged_table) in staged_tables {
        if staged_table.operations.is_empty() {
            continue;
        }
        let operation_count = staged_table.operations.len();
        let mut ordered: Vec<Option<(String, Option<Record>)>> = std::iter::repeat_with(|| None)
            .take(operation_count)
            .collect();
        for (id, operation) in staged_table.operations {
            ordered[operation.position] = Some((id, operation.operation));
        }
        let mut ordered: Vec<_> = ordered
            .into_iter()
            .map(|entry| entry.expect("staged positions are contiguous"))
            .collect();
        if !ordered.windows(2).all(|pair| pair[0].0 < pair[1].0) {
            ordered.sort_unstable_by(|left, right| left.0.cmp(&right.0));
        }
        let mut changes = Vec::with_capacity(operation_count);
        for (id, operation) in ordered {
            let payload = match &operation {
                Some(record) => Some(Arc::new(encode_record_ordered(
                    &staged_table.schema,
                    record,
                    Some(&mut blob_sink),
                )?)),
                None => None,
            };
            changes.push(PreparedChange {
                id,
                operation,
                payload,
            });
        }
        staged.push(PreparedTable {
            name,
            schema: staged_table.schema,
            changes,
        });
    }
    staged.sort_unstable_by(|left, right| left.name.cmp(&right.name));
    // Most transactions do not advance an identity sequence. Encode their
    // potentially large payloads before taking the global commit mutex, then
    // patch the final version and CRC once conflict validation assigns it.
    // Identity high-water marks are read under the mutex and therefore keep
    // the legacy encoding path below.
    let preencoded_wal = if staged
        .iter()
        .all(|table| !table.schema.columns.iter().any(|column| column.identity))
    {
        let wal_changes: Vec<(&str, &str, Option<&[u8]>)> = staged
            .iter()
            .flat_map(|table| {
                table.changes.iter().map(|change| {
                    (
                        table.name.as_str(),
                        change.id.as_str(),
                        change.payload.as_deref().map(|payload| payload.as_slice()),
                    )
                })
            })
            .collect();
        Some(encode_commit(0, &wal_changes, &[])?)
    } else {
        None
    };
    // Blob files are already individually synced. Publish their names and
    // sync the blob directory before contending for the commit mutex; the
    // pending set makes concurrent compaction GC treat them as referenced
    // until either this commit applies or aborts.
    let pending_blobs = PendingBlobPublications::new(shared.clone(), &blob_sink);
    blob_sink.publish()?;
    #[cfg(test)]
    {
        shared
            .blob_test_reached_publish
            .store(true, AtomicOrdering::Release);
        while shared
            .blob_test_pause_after_publish
            .load(AtomicOrdering::Acquire)
        {
            std::thread::yield_now();
        }
    }
    // Decode prior indexed records optimistically while writers still prepare
    // in parallel. Commit validation below re-reads each latest version under
    // the serialization mutex; a cached record is used only when its version
    // still matches, so concurrent updates remain conflicts and checkpoint
    // representation changes remain safe.
    let has_derived_indexes = staged.iter().any(|table| {
        !table.schema.indexes.is_empty()
            || !table.schema.text_indexes.is_empty()
            || !table.schema.vector_indexes.is_empty()
    });
    let (incoming_index_bytes, optimistic_prior_records) = if has_derived_indexes {
        let state = shared.state.read().unwrap();
        let incoming_index_bytes = estimate_staged_index_bytes(&state, &staged)?;
        let records = staged
            .iter()
            .map(|table| {
                table
                    .changes
                    .iter()
                    .map(|change| {
                        if table.schema.indexes.is_empty() && table.schema.text_indexes.is_empty() {
                            return Ok(None);
                        }
                        let previous = if state.id_is_above_high_watermark(&table.name, &change.id)
                        {
                            None
                        } else {
                            state.latest_owned(&table.name, &change.id)?
                        };
                        match previous {
                            Some(entry) if !entry.is_tombstone() => Ok(Some((
                                entry.version,
                                read_record_kind(&state.blobs, &state.readers, &entry.kind)?,
                            ))),
                            _ => Ok(None),
                        }
                    })
                    .collect::<Result<Vec<_>>>()
            })
            .collect::<Result<Vec<_>>>()?;
        (incoming_index_bytes, records)
    } else {
        let bytes = staged
            .iter()
            .flat_map(|table| &table.changes)
            .map(|change| {
                change
                    .id
                    .len()
                    .saturating_add(change.payload.as_ref().map_or(0, |value| value.len()))
                    .saturating_add(144)
            })
            .fold(0usize, usize::saturating_add);
        let records = staged
            .iter()
            .map(|table| vec![None; table.changes.len()])
            .collect();
        (bytes, records)
    };
    if incoming_index_bytes > shared.memory_governor.index_capacity() {
        return Err(Error::MemoryLimit(format!(
            "transaction needs an estimated {incoming_index_bytes} index-delta bytes, but the pool is {} bytes; split the transaction or raise memory.index_delta_pool_bytes",
            shared.memory_governor.index_capacity()
        )));
    }
    let outside_prepare_time = prepare_started.elapsed();

    Ok(PreparedCommit {
        snap_version,
        staged,
        preencoded_wal,
        optimistic_prior_records,
        incoming_index_bytes,
        outside_prepare_time,
        commit_started,
        _pending_blobs: pending_blobs,
    })
}

fn coordinate_commit(shared: &Arc<Shared>, prepared: PreparedCommit) -> Result<u64> {
    let request = Arc::new(CoordinatedCommit::new(prepared));
    let mut leader = {
        let mut coordinator = shared.commit_coordinator.lock().unwrap();
        coordinator.queue.push_back(request.clone());
        if coordinator.active {
            false
        } else {
            coordinator.active = true;
            true
        }
    };

    loop {
        if leader {
            if should_coalesce_safe_batch(shared) {
                let coalesce_started = Instant::now();
                std::thread::sleep(Duration::from_micros(
                    shared.opts.safe_group_commit_delay_us,
                ));
                shared.wal_group_coalesce_nanos.fetch_add(
                    elapsed_nanos(coalesce_started.elapsed()),
                    AtomicOrdering::Relaxed,
                );
            }
            let batch = {
                let mut coordinator = shared.commit_coordinator.lock().unwrap();
                let take = coordinator.queue.len().min(COMMIT_COORDINATOR_MAX_BATCH);
                coordinator.queue.drain(..take).collect::<Vec<_>>()
            };
            process_coordinated_batch(shared, batch);
            let mut coordinator = shared.commit_coordinator.lock().unwrap();
            if let Some(next) = coordinator.queue.front() {
                next.promote_to_leader();
            } else {
                coordinator.active = false;
            }
        }
        if let Some(result) = request.take_result() {
            return result;
        }
        match request.wait_for_result_or_lead() {
            Some(result) => return result,
            None => leader = true,
        }
    }
}

fn should_coalesce_safe_batch(shared: &Shared) -> bool {
    if shared.opts.durability != Durability::Safe || shared.opts.safe_group_commit_delay_us == 0 {
        return false;
    }
    if shared.active_state_writers.load(AtomicOrdering::Acquire) > 1 {
        // Preserve a short contention memory across the tiny gap between two
        // writers finishing one batch and staging the next. Without it, a
        // two-writer workload can alternate just cleanly enough for each new
        // leader to misclassify the stream as single-writer.
        shared
            .safe_coalesce_budget
            .store(8, AtomicOrdering::Release);
        return true;
    }
    shared
        .safe_coalesce_budget
        .fetch_update(AtomicOrdering::AcqRel, AtomicOrdering::Acquire, |budget| {
            budget.checked_sub(1)
        })
        .is_ok()
}

fn process_coordinated_batch(shared: &Arc<Shared>, batch: Vec<Arc<CoordinatedCommit>>) {
    let mut prepared = batch
        .iter()
        .map(|request| {
            request
                .prepared
                .lock()
                .unwrap()
                .take()
                .expect("coordinated commit is processed once")
        })
        .collect::<Vec<_>>();
    let queue_wait = batch
        .iter()
        .map(|request| request.queued_at.elapsed())
        .fold(Duration::ZERO, Duration::saturating_add);
    if let Some(results) = finish_coordinated_insert_batch(shared, &mut prepared, queue_wait) {
        debug_assert_eq!(batch.len(), results.len());
        for (request, result) in batch.into_iter().zip(results) {
            request.complete(result);
        }
        return;
    }
    for (request, prepared) in batch.into_iter().zip(prepared) {
        request.complete(finish_prepared_commit(shared, prepared));
    }
}

/// Coordinated path for disjoint inserts into unconstrained tables.
/// Validation still happens under the serialization mutex, every transaction
/// receives its own version and WAL frame, and readers see the complete batch
/// only after the vectored append and one state publication finish. `Safe`
/// batches issue one strict durability barrier before any member returns.
fn finish_coordinated_insert_batch(
    shared: &Arc<Shared>,
    prepared: &mut [PreparedCommit],
    queue_wait: Duration,
) -> Option<Vec<Result<u64>>> {
    if prepared.len() < 2 {
        return None;
    }
    debug_assert!(prepared.iter().all(is_coordinated_insert_candidate));
    if shared
        .background_checkpoint
        .lock()
        .unwrap()
        .last_error
        .is_some()
    {
        return None;
    }
    let incoming_bytes = prepared.iter().try_fold(0usize, |total, commit| {
        total.checked_add(commit.incoming_index_bytes)
    })?;
    if shared.memory_governor.index_would_exceed(incoming_bytes) {
        return None;
    }

    let lock_started = Instant::now();
    shared.commit_waiters.fetch_add(1, AtomicOrdering::AcqRel);
    let mut cs = lock_commit_for_transaction(shared);
    shared.commit_waiters.fetch_sub(1, AtomicOrdering::AcqRel);
    let lock_wait = lock_started.elapsed().saturating_add(queue_wait);
    let locked_prepare_started = Instant::now();
    let mut ids = HashSet::new();
    let start_version = {
        let state = shared.state.read().unwrap();
        for commit in prepared.iter() {
            for table in &commit.staged {
                if state.catalog.table(&table.name) != Some(&table.schema) {
                    return None;
                }
                for change in &table.changes {
                    if !ids.insert((table.name.clone(), change.id.clone())) {
                        return None;
                    }
                    let previous = if state.id_is_above_high_watermark(&table.name, &change.id) {
                        None
                    } else {
                        match state.latest_owned(&table.name, &change.id) {
                            Ok(previous) => previous,
                            Err(_) => return None,
                        }
                    };
                    // Updates/reinserts need prior-record and derived-index
                    // handling from the general path.
                    if previous.is_some() {
                        return None;
                    }
                }
            }
        }
        state.committed_version
    };
    let batch_len = u64::try_from(prepared.len()).expect("coordinator batch length fits u64");
    let Some(end_version) = start_version.checked_add(batch_len) else {
        return Some(
            (0..prepared.len())
                .map(|_| {
                    Err(Error::InvalidArgument(
                        "commit version space is exhausted".into(),
                    ))
                })
                .collect(),
        );
    };
    let versions = ((start_version + 1)..=end_version).collect::<Vec<_>>();
    let locked_prepare_time = locked_prepare_started.elapsed();

    let wal_started = Instant::now();
    for (commit, version) in prepared.iter_mut().zip(&versions) {
        set_encoded_commit_version(
            commit
                .preencoded_wal
                .as_mut()
                .expect("insert batch has preencoded WAL"),
            *version,
        );
    }
    let wal_append_started = Instant::now();
    let records = prepared
        .iter()
        .map(|commit| {
            commit
                .preencoded_wal
                .as_deref()
                .expect("insert batch has preencoded WAL")
        })
        .collect::<Vec<_>>();
    let synced_bytes = records.iter().fold(0u64, |total, record| {
        total.saturating_add(record.len() as u64)
    });
    if let Err(error) = cs.wal().append_commits_unflushed(&records) {
        drop(cs);
        let message = error.to_string();
        return Some(
            versions
                .into_iter()
                .map(|_| {
                    Err(Error::Io(std::io::Error::other(format!(
                        "coordinated WAL append failed: {message}"
                    ))))
                })
                .collect(),
        );
    }
    let wal_append_time = wal_append_started.elapsed();
    let sync_due = cs.wal().sync_due(
        shared.opts.durability,
        shared.opts.balanced_sync_interval_ms,
    );
    let sync_outcome = sync_due.then(|| {
        let sync_started = Instant::now();
        let outcome = cs.wal().sync_data();
        record_wal_sync(shared, sync_started.elapsed(), batch_len, synced_bytes);
        outcome
    });

    let apply_started = Instant::now();
    let mut added = 0u64;
    {
        let mut state = shared.state.write().unwrap();
        for (commit, version) in prepared.iter_mut().zip(&versions) {
            for table in std::mem::take(&mut commit.staged) {
                let high_id = table
                    .changes
                    .last()
                    .map(|change| change.id.clone())
                    .expect("prepared table is non-empty");
                for change in table.changes {
                    let put = change.operation.as_ref().zip(change.payload.as_ref());
                    let put = put.map(|(record, payload)| (payload, record));
                    let mut jobs = Vec::new();
                    apply_one_owned(
                        &mut state,
                        *version,
                        &table.schema,
                        &table.name,
                        change.id,
                        ApplyRecordState { put, prior: None },
                        &mut jobs,
                    );
                    debug_assert!(jobs.is_empty());
                    added = added.saturating_add(
                        change
                            .payload
                            .as_deref()
                            .map_or(0, |payload| payload.len() as u64)
                            + 32,
                    );
                }
                state.record_high_id(&table.name, &high_id);
            }
            state.committed_version = *version;
        }
    }
    let apply_time = apply_started.elapsed();
    cs.memtable_bytes = cs.memtable_bytes.saturating_add(added);
    shared.memory_governor.add_index_delta_bytes(incoming_bytes);
    let maintenance_needed = cs.memtable_bytes >= shared.opts.memtable_max_bytes;
    drop(cs);
    let wal_time = wal_started.elapsed();

    if maintenance_needed {
        let result = (|| -> Result<()> {
            let mut cs = lock_commit_after_group_sync(shared);
            if cs.memtable_bytes >= shared.opts.memtable_max_bytes
                && shared.state.read().unwrap().index.frozen.is_none()
                && !shared.background_derived.lock().unwrap().running
            {
                let memory = Db::acquire_maintenance_memory(shared);
                let _ = schedule_frozen_checkpoint(shared, &mut cs, memory)?;
            }
            Ok(())
        })();
        if let Err(error) = result {
            let mut status = shared.background_checkpoint.lock().unwrap();
            status.commit_unknown = matches!(&error, Error::CommitUnknown(_));
            status.last_error = Some(format!("post-commit maintenance failed: {error}"));
        }
    }

    let elapsed_nanos = |duration: Duration| duration.as_nanos().min(u64::MAX as u128) as u64;
    let count = prepared.len() as u64;
    shared
        .commit_count
        .fetch_add(count, AtomicOrdering::Relaxed);
    let total_commit_nanos = prepared.iter().fold(0u64, |total, commit| {
        total.saturating_add(elapsed_nanos(commit.commit_started.elapsed()))
    });
    shared
        .commit_nanos
        .fetch_add(total_commit_nanos, AtomicOrdering::Relaxed);
    shared
        .commit_lock_wait_nanos
        .fetch_add(elapsed_nanos(lock_wait), AtomicOrdering::Relaxed);
    let outside_prepare = prepared
        .iter()
        .map(|commit| commit.outside_prepare_time)
        .fold(Duration::ZERO, Duration::saturating_add);
    shared.commit_prepare_nanos.fetch_add(
        elapsed_nanos(outside_prepare.saturating_add(locked_prepare_time)),
        AtomicOrdering::Relaxed,
    );
    shared
        .commit_locked_prepare_nanos
        .fetch_add(elapsed_nanos(locked_prepare_time), AtomicOrdering::Relaxed);
    shared.commit_wal_nanos.fetch_add(
        elapsed_nanos(wal_time).saturating_mul(count),
        AtomicOrdering::Relaxed,
    );
    shared
        .commit_wal_append_nanos
        .fetch_add(elapsed_nanos(wal_append_time), AtomicOrdering::Relaxed);
    shared
        .commit_apply_nanos
        .fetch_add(elapsed_nanos(apply_time), AtomicOrdering::Relaxed);
    shared
        .coordinated_batch_count
        .fetch_add(1, AtomicOrdering::Relaxed);
    shared
        .coordinated_commit_count
        .fetch_add(count, AtomicOrdering::Relaxed);

    let sync_error = match sync_outcome {
        Some(WalAppendOutcome::SyncFailed(error)) => Some(error.to_string()),
        _ => None,
    };
    Some(
        versions
            .into_iter()
            .map(|version| match &sync_error {
                Some(error) => Err(Error::CommitUnknown(format!(
                    "version {version} was published, but syncing its coordinated WAL batch failed: {error}"
                ))),
                None => Ok(version),
            })
            .collect(),
    )
}

fn finish_prepared_commit(shared: &Arc<Shared>, prepared: PreparedCommit) -> Result<u64> {
    let PreparedCommit {
        snap_version,
        staged,
        preencoded_wal,
        optimistic_prior_records,
        incoming_index_bytes,
        outside_prepare_time,
        commit_started,
        _pending_blobs,
    } = prepared;

    let mut lock_wait = Duration::ZERO;
    let mut locked_prepare_time = Duration::ZERO;
    let (mut cs, previous_entries, commit_version, incoming_index_bytes, identity_high_water) = loop {
        let lock_started = Instant::now();
        shared.commit_waiters.fetch_add(1, AtomicOrdering::AcqRel);
        let mut cs = lock_commit_for_transaction(shared);
        shared.commit_waiters.fetch_sub(1, AtomicOrdering::AcqRel);
        lock_wait = lock_wait.saturating_add(lock_started.elapsed());
        // Surface asynchronous I/O failure before this transaction reaches
        // its WAL durability point; reporting it after apply would make the
        // commit outcome ambiguous to the caller.
        take_background_checkpoint_error(shared)?;
        let locked_prepare_started = Instant::now();
        let mut previous_entries: Vec<Vec<(Option<VersionEntry>, Option<Record>)>> =
            Vec::with_capacity(staged.len());
        let (commit_version, identity_high_water) = {
            let st = shared.state.read().unwrap();
            for (table_index, table) in staged.iter().enumerate() {
                if st.catalog.table(&table.name) != Some(&table.schema) {
                    return Err(Error::Conflict(format!(
                        "schema for {} changed while the transaction was preparing",
                        table.name
                    )));
                }
                let mut table_previous = Vec::with_capacity(table.changes.len());
                for (change_index, change) in table.changes.iter().enumerate() {
                    // Write-write conflict: someone committed a change to this
                    // record after our snapshot.
                    let previous = if st.id_is_above_high_watermark(&table.name, &change.id) {
                        None
                    } else {
                        st.latest_owned(&table.name, &change.id)?
                    };
                    if let Some(last) = &previous {
                        if last.version > snap_version {
                            return Err(Error::Conflict(format!(
                                "{}/{} changed after this transaction began",
                                table.name, change.id
                            )));
                        }
                    }
                    // Complete every fallible read before the WAL durability
                    // point. Applying the already-committed change below must
                    // only mutate memory and cannot return a late error after
                    // partially publishing a transaction.
                    let prior_record = match &previous {
                        Some(entry)
                            if !entry.is_tombstone()
                                && (!table.schema.indexes.is_empty()
                                    || !table.schema.text_indexes.is_empty()) =>
                        {
                            match &optimistic_prior_records[table_index][change_index] {
                                Some((version, record)) if *version == entry.version => {
                                    Some(record.clone())
                                }
                                _ => Some(read_record_kind(&st.blobs, &st.readers, &entry.kind)?),
                            }
                        }
                        _ => None,
                    };
                    table_previous.push((previous, prior_record));
                }
                previous_entries.push(table_previous);
            }
            validate_unique(&st, &staged)?;
            validate_foreign_keys(&st, &staged)?;
            let identity_high_water: Vec<(String, i64)> = staged
                .iter()
                .filter(|table| table.schema.columns.iter().any(|column| column.identity))
                .filter_map(|table| {
                    st.identity_high_water
                        .get(&table.name)
                        .map(|value| (table.name.clone(), *value))
                })
                .collect();
            (st.committed_version + 1, identity_high_water)
        };
        locked_prepare_time = locked_prepare_time.saturating_add(locked_prepare_started.elapsed());
        if shared
            .memory_governor
            .index_would_exceed(incoming_index_bytes)
        {
            if let Some(group) = cs.wal_sync_group.clone() {
                drop(cs);
                let _ = group.wait();
                continue;
            }
            let (derived_bytes, has_frozen_derived) = {
                let state = shared.state.read().unwrap();
                (
                    state.derived_delta_memory_bytes(),
                    state.has_frozen_derived(),
                )
            };
            let derived_running = shared.background_derived.lock().unwrap().running;
            if derived_running {
                drop(cs);
                wait_for_background_derived(shared)?;
                continue;
            }
            if derived_bytes > 0 || has_frozen_derived {
                wait_vector_indexing_shared(shared)?;
                let memory = Db::acquire_maintenance_memory(shared);
                if schedule_frozen_derived(shared, memory)? {
                    drop(cs);
                    wait_for_background_derived(shared)?;
                    continue;
                }
            }
            if shared.state.read().unwrap().index.frozen.is_some() {
                drop(cs);
                wait_for_background_checkpoint(shared)?;
                continue;
            }
            let memory = Db::acquire_maintenance_memory(shared);
            if schedule_frozen_checkpoint(shared, &mut cs, memory)? {
                drop(cs);
                wait_for_background_checkpoint(shared)?;
                continue;
            }
            if shared
                .memory_governor
                .index_would_exceed(incoming_index_bytes)
            {
                return Err(Error::MemoryLimit(
                    "index tombstones still fill the delta pool after consolidation; compact the database or raise memory.index_delta_pool_bytes"
                        .into(),
                ));
            }
        }
        break (
            cs,
            previous_entries,
            commit_version,
            incoming_index_bytes,
            identity_high_water,
        );
    };
    let prepare_time = outside_prepare_time.saturating_add(locked_prepare_time);
    // Durability point: the WAL record is the commit.
    let wal_started = Instant::now();
    let mut bytes = match preencoded_wal {
        Some(bytes) => bytes,
        None => {
            let wal_changes: Vec<(&str, &str, Option<&[u8]>)> = staged
                .iter()
                .flat_map(|table| {
                    table.changes.iter().map(|change| {
                        (
                            table.name.as_str(),
                            change.id.as_str(),
                            change.payload.as_deref().map(|payload| payload.as_slice()),
                        )
                    })
                })
                .collect();
            let identity_wal: Vec<(&str, i64)> = identity_high_water
                .iter()
                .map(|(table, value)| (table.as_str(), *value))
                .collect();
            encode_commit(commit_version, &wal_changes, &identity_wal)?
        }
    };
    let wal_append_started = Instant::now();
    set_encoded_commit_version(&mut bytes, commit_version);
    cs.wal().append_commit_unflushed(&bytes)?;
    let sync_due = cs.wal().sync_due(
        shared.opts.durability,
        shared.opts.balanced_sync_interval_ms,
    );
    let sync_group = if sync_due {
        match cs.wal_sync_group.as_ref() {
            Some(group) => {
                group.join(bytes.len() as u64);
                Some((group.clone(), false))
            }
            None => {
                let group = Arc::new(WalSyncGroup::new(bytes.len() as u64));
                cs.wal_sync_group = Some(group.clone());
                Some((group, true))
            }
        }
    } else {
        None
    };
    let wal_append_time = wal_append_started.elapsed();

    // Publish atomically to readers.
    let apply_started = Instant::now();
    let mut added = 0u64;
    let mut obsolete_operations = 0u64;
    let mut obsolete_bytes = 0u64;
    let mut jobs: Vec<VecJob> = Vec::new();
    {
        let mut st = shared.state.write().unwrap();
        for (table, table_previous) in staged.into_iter().zip(previous_entries) {
            let high_id = table
                .changes
                .last()
                .map(|change| change.id.clone())
                .expect("prepared tables are non-empty");
            for (change, (previous, prior_record)) in table.changes.into_iter().zip(table_previous)
            {
                if let Some(previous) = &previous {
                    // An update, delete, or reinsert supersedes one previously
                    // visible record version. Count rows, not SQL statements.
                    obsolete_operations += 1;
                    obsolete_bytes = obsolete_bytes.saturating_add(obsolete_entry_bytes_estimate(
                        &table.name,
                        &change.id,
                        previous,
                    ));
                } else if change.operation.is_none() {
                    // Insert-then-delete inside one transaction leaves only a
                    // tombstone, which compaction can discard completely.
                    obsolete_operations += 1;
                }
                if let Some(VersionEntry {
                    kind: VKind::SegPut { segment, .. },
                    ..
                }) = &previous
                {
                    st.superseded_segments.insert(*segment);
                }
                let put = match (&change.operation, &change.payload) {
                    (Some(record), Some(payload)) => Some((payload, record)),
                    _ => None,
                };
                apply_one_owned(
                    &mut st,
                    commit_version,
                    &table.schema,
                    &table.name,
                    change.id,
                    ApplyRecordState {
                        put,
                        prior: prior_record,
                    },
                    &mut jobs,
                );
                added += change
                    .payload
                    .as_deref()
                    .map_or(0, |payload| payload.len() as u64)
                    + 32;
            }
            st.record_high_id(&table.name, &high_id);
        }
        st.committed_version = commit_version;
    }
    let apply_time = apply_started.elapsed();
    if obsolete_operations > 0 {
        let mut auto = shared.auto_compaction_state.lock().unwrap();
        auto.debt_operations = auto.debt_operations.saturating_add(obsolete_operations);
        auto.estimated_reclaimable_bytes = auto
            .estimated_reclaimable_bytes
            .saturating_add(obsolete_bytes);
    }
    if !jobs.is_empty() {
        let tx = shared.vector_tx.lock().unwrap();
        if let Some(tx) = tx.as_ref() {
            shared
                .vector_backlog
                .fetch_add(jobs.len() as u64, AtomicOrdering::SeqCst);
            for job in jobs {
                if tx.send(job).is_err() {
                    *shared
                        .vector_worker_error
                        .lock()
                        .unwrap_or_else(|poison| poison.into_inner()) = Some(
                        "vector indexing worker stopped before accepting committed work".into(),
                    );
                    // Keep the backlog charged: the vector is committed but
                    // not searchable. A waiter must report the dead worker,
                    // not claim that indexing completed successfully.
                }
            }
        }
    }
    cs.memtable_bytes += added;
    shared
        .memory_governor
        .add_index_delta_bytes(incoming_index_bytes);
    let maintenance_needed = cs.memtable_bytes >= shared.opts.memtable_max_bytes;
    let derived_schedule_needed = {
        let memory = shared.memory_governor.stats();
        memory.index_delta_bytes >= memory.index_delta_capacity_bytes / 2
            && shared.state.read().unwrap().derived_delta_memory_bytes() > 0
    };
    drop(cs);

    let wal_result = sync_group.map_or(Ok(()), |(group, leader)| {
        finish_or_wait_wal_sync(shared, group, leader)
    });
    let wal_time = wal_started.elapsed();

    let maintenance_result = (|| -> Result<()> {
        if maintenance_needed || derived_schedule_needed {
            let mut cs = lock_commit_after_group_sync(shared);
            if derived_schedule_needed && !shared.background_derived.lock().unwrap().running {
                if let Some(memory) = shared.memory_governor.try_acquire(
                    MemoryPool::Maintenance,
                    shared.opts.memory.maintenance_pool_bytes,
                ) {
                    let _ = schedule_frozen_derived(shared, memory)?;
                }
            }
            if cs.memtable_bytes >= shared.opts.memtable_max_bytes {
                // The derived worker owns the sole maintenance reservation
                // and needs this mutex for its final publication. Defer the
                // soft-threshold primary checkpoint instead of waiting for
                // that reservation while holding the mutex. A later commit
                // schedules it; hard index pressure waits outside the mutex.
                if shared.background_derived.lock().unwrap().running {
                    return Ok(());
                }
                let frozen_running = shared.state.read().unwrap().index.frozen.is_some();
                if !frozen_running {
                    let memory = Db::acquire_maintenance_memory(shared);
                    let _ = schedule_frozen_checkpoint(shared, &mut cs, memory)?;
                }
            }
        }
        Ok(())
    })();
    if let Err(error) = maintenance_result {
        // The transaction is already in the WAL and visible. Never turn a
        // post-commit checkpoint failure into an apparent transaction
        // failure that callers might retry. Defer it to the next write before
        // that write reaches its own durability point.
        let mut status = shared.background_checkpoint.lock().unwrap();
        status.commit_unknown = matches!(&error, Error::CommitUnknown(_));
        status.last_error = Some(format!("post-commit maintenance failed: {error}"));
    }
    let elapsed_nanos = |duration: Duration| duration.as_nanos().min(u64::MAX as u128) as u64;
    shared.commit_count.fetch_add(1, AtomicOrdering::Relaxed);
    shared.commit_nanos.fetch_add(
        elapsed_nanos(commit_started.elapsed()),
        AtomicOrdering::Relaxed,
    );
    shared
        .commit_lock_wait_nanos
        .fetch_add(elapsed_nanos(lock_wait), AtomicOrdering::Relaxed);
    shared
        .commit_prepare_nanos
        .fetch_add(elapsed_nanos(prepare_time), AtomicOrdering::Relaxed);
    shared
        .commit_locked_prepare_nanos
        .fetch_add(elapsed_nanos(locked_prepare_time), AtomicOrdering::Relaxed);
    shared
        .commit_wal_nanos
        .fetch_add(elapsed_nanos(wal_time), AtomicOrdering::Relaxed);
    shared
        .commit_wal_append_nanos
        .fetch_add(elapsed_nanos(wal_append_time), AtomicOrdering::Relaxed);
    shared
        .commit_apply_nanos
        .fetch_add(elapsed_nanos(apply_time), AtomicOrdering::Relaxed);
    match wal_result {
        Ok(()) => Ok(commit_version),
        Err(error) => Err(Error::CommitUnknown(format!(
            "version {commit_version} was published, but syncing its WAL group failed: {error}"
        ))),
    }
}

fn estimate_staged_index_bytes(st: &State, staged: &[PreparedTable]) -> Result<usize> {
    let mut bytes = 0usize;
    for table in staged {
        for change in &table.changes {
            bytes = bytes
                .saturating_add(change.id.len())
                .saturating_add(change.payload.as_ref().map_or(0, |value| value.len()))
                // Matches PrimaryIdx::delta_memory_bytes (96 bytes for the
                // B-tree key/value slot plus 40 for VersionEntry/Arc), with a
                // small conservative margin. The outer table key is amortized
                // across all rows in the transaction.
                .saturating_add(144);
            // Updates can create both a tombstone for the old derived entry
            // and a new entry. Charge both sides conservatively before WAL.
            bytes = bytes.saturating_add(
                table
                    .schema
                    .indexes
                    .len()
                    .saturating_mul(change.id.len().saturating_mul(2).saturating_add(320)),
            );
            if table.schema.text_indexes.is_empty() && table.schema.vector_indexes.is_empty() {
                continue;
            }
            let mut charge_record = |record: &Record| {
                for def in &table.schema.text_indexes {
                    if let Some(Value::Text(text)) = record.get(&def.column) {
                        for token in crate::text::tokenize(text) {
                            bytes = bytes
                                .saturating_add(token.len())
                                .saturating_add(change.id.len())
                                .saturating_add(112);
                        }
                    }
                }
                for def in &table.schema.vector_indexes {
                    if let Some(Value::Vector(vector)) = record.get(&def.column) {
                        let scalar = if def.quantized { 1 } else { 4 };
                        bytes = bytes
                            .saturating_add(vector.len().saturating_mul(scalar))
                            .saturating_add(def.m.saturating_mul(16))
                            .saturating_add(change.id.len())
                            .saturating_add(192);
                    }
                }
            };
            if let Some(record) = &change.operation {
                charge_record(record);
            }
            if let Some(previous) = st.latest_owned(&table.name, &change.id)? {
                if !previous.is_tombstone() {
                    let record = read_record_kind(&st.blobs, &st.readers, &previous.kind)?;
                    charge_record(&record);
                }
            }
        }
    }
    Ok(bytes)
}

fn validate_unique(st: &State, staged: &[PreparedTable]) -> Result<()> {
    if !st
        .catalog
        .tables
        .iter()
        .any(|schema| schema.indexes.iter().any(|index| index.unique))
    {
        return Ok(());
    }
    let mut staged_new: HashMap<(String, String, Vec<u8>), String> = HashMap::new();
    let staged_keys: HashSet<_> = staged
        .iter()
        .flat_map(|table| {
            table
                .changes
                .iter()
                .map(|change| (table.name.as_str(), change.id.as_str()))
        })
        .collect();
    for table in staged {
        for change in &table.changes {
            let Some(record) = &change.operation else {
                continue;
            };
            for def in &table.schema.indexes {
                if !def.unique {
                    continue;
                }
                let Some(value) = record.get(&def.column) else {
                    continue;
                };
                if value.is_null() {
                    continue;
                }
                let key = index_key(value);
                if let Some(previous) = staged_new.insert(
                    (table.name.clone(), def.column.clone(), key.clone()),
                    change.id.clone(),
                ) {
                    if previous != change.id {
                        return Err(Error::UniqueViolation {
                            table: table.name.clone(),
                            column: def.column.clone(),
                        });
                    }
                }
                if let Some(index) = st.secondary.get(&(table.name.clone(), def.column.clone())) {
                    for holder in index.ids(&key)? {
                        // A holder also written by this transaction is judged
                        // by its staged value (covered by staged_new above).
                        if holder != change.id
                            && !staged_keys.contains(&(table.name.as_str(), holder.as_str()))
                        {
                            return Err(Error::UniqueViolation {
                                table: table.name.clone(),
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

fn prepared_change<'a>(
    staged: &'a [PreparedTable],
    table: &str,
    id: &str,
) -> Option<&'a PreparedChange> {
    let table = staged.iter().find(|candidate| candidate.name == table)?;
    table
        .changes
        .binary_search_by(|change| change.id.as_str().cmp(id))
        .ok()
        .map(|index| &table.changes[index])
}

/// Read one row from the state that would exist after `staged` is applied.
/// The commit mutex is held by the caller, so this view cannot move beneath
/// validation.
fn final_record(
    st: &State,
    staged: &[PreparedTable],
    table: &str,
    id: &str,
) -> Result<Option<Record>> {
    let schema = st
        .catalog
        .table(table)
        .ok_or_else(|| Error::TableNotFound(table.into()))?;
    if let Some(change) = prepared_change(staged, table, id) {
        return Ok(change.operation.as_ref().map(|record| {
            let mut record = record.clone();
            if schema.has_implicit_id() {
                record.insert(ID_COLUMN.into(), Value::Text(id.to_owned()));
            }
            record
        }));
    }
    match st.latest_owned(table, id)? {
        Some(entry) if entry.version > schema.epoch && !entry.is_tombstone() => {
            let mut record = read_record_kind(&st.blobs, &st.readers, &entry.kind)?;
            if schema.has_implicit_id() {
                record.insert(ID_COLUMN.into(), Value::Text(id.to_owned()));
            }
            Ok(Some(record))
        }
        _ => Ok(None),
    }
}

fn final_ids_matching(
    st: &State,
    staged: &[PreparedTable],
    table: &str,
    column: &str,
    value: &Value,
) -> Result<BTreeSet<String>> {
    let mut candidates = BTreeSet::new();
    let schema = st
        .catalog
        .table(table)
        .ok_or_else(|| Error::TableNotFound(table.into()))?;
    if column == ID_COLUMN && schema.has_implicit_id() {
        if let Value::Text(id) = value {
            candidates.insert(id.clone());
        }
    } else if let Some(index) = st.secondary.get(&(table.to_owned(), column.to_owned())) {
        candidates.extend(index.ids(&index_key(value))?);
    } else {
        // Catalog validation normally guarantees an index for every FK side.
        // Keep the fallback for old catalogs and recovery tooling.
        st.index.visit_table(table, None, |id, _versions| {
            candidates.insert(id.to_owned());
            Ok(true)
        })?;
    }
    if let Some(prepared) = staged.iter().find(|candidate| candidate.name == table) {
        candidates.extend(prepared.changes.iter().map(|change| change.id.clone()));
    }
    let mut matching = BTreeSet::new();
    for id in candidates {
        if let Some(record) = final_record(st, staged, table, &id)? {
            if record.get(column) == Some(value) {
                matching.insert(id);
            }
        }
    }
    Ok(matching)
}

fn validate_foreign_keys(st: &State, staged: &[PreparedTable]) -> Result<()> {
    if !st
        .catalog
        .tables
        .iter()
        .any(|schema| !schema.foreign_keys.is_empty())
    {
        return Ok(());
    }

    // Every new final child value must resolve to a parent in the same final
    // view. This permits parent+child insertion in one transaction.
    for table in staged {
        for change in &table.changes {
            let Some(record) = &change.operation else {
                continue;
            };
            for foreign_key in &table.schema.foreign_keys {
                let Some(value) = record.get(&foreign_key.column) else {
                    continue;
                };
                if value.is_null() {
                    continue;
                }
                if final_ids_matching(
                    st,
                    staged,
                    &foreign_key.referenced_table,
                    &foreign_key.referenced_column,
                    value,
                )?
                .is_empty()
                {
                    return Err(Error::SchemaViolation(format!(
                        "foreign key violation: {}.{} has no matching {}.{}",
                        table.name,
                        foreign_key.column,
                        foreign_key.referenced_table,
                        foreign_key.referenced_column
                    )));
                }
            }
        }
    }

    // Removing a referenced row is legal only if the final view has no
    // children. A CASCADE miss means a child committed after the deleting
    // transaction's snapshot; return Conflict so the normal retry path can
    // rescan and delete it atomically.
    for parent in staged {
        for change in &parent.changes {
            if change.operation.is_some() {
                continue;
            }
            let Some(previous) = st.latest_owned(&parent.name, &change.id)? else {
                continue;
            };
            if previous.is_tombstone() || previous.version <= parent.schema.epoch {
                continue;
            }
            let mut old_record = read_record_kind(&st.blobs, &st.readers, &previous.kind)?;
            if parent.schema.has_implicit_id() {
                old_record.insert(ID_COLUMN.into(), Value::Text(change.id.clone()));
            }
            for child_schema in &st.catalog.tables {
                for foreign_key in child_schema
                    .foreign_keys
                    .iter()
                    .filter(|foreign_key| foreign_key.referenced_table == parent.name)
                {
                    let Some(old_value) = old_record.get(&foreign_key.referenced_column) else {
                        continue;
                    };
                    let children = final_ids_matching(
                        st,
                        staged,
                        &child_schema.name,
                        &foreign_key.column,
                        old_value,
                    )?;
                    if children.is_empty() {
                        continue;
                    }
                    match foreign_key.on_delete {
                        ReferentialAction::Restrict => {
                            return Err(Error::SchemaViolation(format!(
                                "cannot delete {}.{}: referenced by {}.{}",
                                parent.name, change.id, child_schema.name, foreign_key.column
                            )))
                        }
                        ReferentialAction::Cascade => {
                            return Err(Error::Conflict(format!(
                                "cascade for {}/{} must be retried after concurrent child change",
                                parent.name, change.id
                            )))
                        }
                    }
                }
            }
        }
    }
    Ok(())
}

/// Apply one committed change to the in-memory state, maintaining secondary
/// and vector indexes (which track the latest committed state only). Async
/// vector insertions are returned as jobs for the background thread.
fn apply_one_owned(
    st: &mut State,
    version: u64,
    schema: &TableSchema,
    table: &str,
    id: String,
    records: ApplyRecordState<'_>,
    jobs: &mut Vec<VecJob>,
) {
    let ApplyRecordState { put, prior } = records;
    let id_ref = id.as_str();
    let has_derived_indexes = !schema.indexes.is_empty()
        || !schema.text_indexes.is_empty()
        || !schema.vector_indexes.is_empty();
    if !has_derived_indexes {
        let kind = match put {
            Some((payload, _)) => VKind::MemPut(payload.clone()),
            None => VKind::MemTombstone,
        };
        st.index.push(table, id, VersionEntry { version, kind });
        return;
    }
    let defs = &schema.indexes;
    let tdefs = &schema.text_indexes;
    if !defs.is_empty() || !tdefs.is_empty() {
        if let Some(prior) = prior {
            for def in defs {
                if let Some(v) = prior.get(&def.column) {
                    if !v.is_null() {
                        if let Some(idx) = st
                            .secondary
                            .get_mut(&(table.to_owned(), def.column.clone()))
                        {
                            let key = index_key(v);
                            idx.remove(&key, id_ref);
                        }
                    }
                }
            }
            for tdef in tdefs {
                if let Some(Value::Text(old)) = prior.get(&tdef.column) {
                    if let Some(tidx) = st.text.get_mut(&(table.to_owned(), tdef.column.clone())) {
                        tidx.remove(id_ref, old);
                    }
                }
            }
        }
    }
    let kind = match put {
        Some((payload, _)) => VKind::MemPut(payload.clone()),
        None => VKind::MemTombstone,
    };
    if let Some((_, rec)) = put {
        for def in defs {
            if let Some(v) = rec.get(&def.column) {
                if !v.is_null() {
                    if let Some(idx) = st
                        .secondary
                        .get_mut(&(table.to_owned(), def.column.clone()))
                    {
                        idx.add(index_key(v), id_ref);
                    }
                }
            }
        }
        for tdef in tdefs {
            if let Some(Value::Text(s)) = rec.get(&tdef.column) {
                if let Some(tidx) = st.text.get_mut(&(table.to_owned(), tdef.column.clone())) {
                    tidx.add(id_ref, s);
                }
            }
        }
    }

    // Vector index maintenance. Inserting tombstones the previous label for
    // the same id; a put without a vector (or a delete) tombstones directly.
    for vdef in &schema.vector_indexes {
        let key = (table.to_owned(), vdef.column.clone());
        let new_vec = put.and_then(|(_, rec)| match rec.get(&vdef.column) {
            Some(Value::Vector(v)) => Some(v.clone()),
            _ => None,
        });
        match new_vec {
            Some(v) => match vdef.mode {
                IndexingMode::Sync => {
                    if let Some(vidx) = st.vector.get_mut(&key) {
                        vidx.insert(id_ref, &v);
                    }
                }
                IndexingMode::Async => jobs.push(VecJob {
                    version,
                    table: table.to_owned(),
                    column: vdef.column.clone(),
                    id: id_ref.to_owned(),
                    vector: v,
                }),
            },
            None => {
                if let Some(vidx) = st.vector.get_mut(&key) {
                    vidx.remove(id_ref);
                }
            }
        }
    }
    st.index.push(table, id, VersionEntry { version, kind });
}

/// Drain committed in-memory data into a new segment, publish a new manifest
/// referencing it, and rotate the WAL. Runs under the commit mutex.
fn checkpoint_measured(shared: &Arc<Shared>, cs: &mut CommitState) -> Result<()> {
    let started = Instant::now();
    // Derived overlays have their own frozen background pipeline. Canonical
    // checkpoint latency must not include serializing secondary/text/vector
    // indexes under the commit mutex.
    let result = checkpoint_locked(shared, cs);
    if result.is_ok() {
        let nanos = started.elapsed().as_nanos().min(u64::MAX as u128) as u64;
        shared
            .checkpoint_count
            .fetch_add(1, AtomicOrdering::Relaxed);
        shared
            .checkpoint_nanos
            .fetch_add(nanos, AtomicOrdering::Relaxed);
    }
    result
}

fn wait_for_background_checkpoint(shared: &Arc<Shared>) -> Result<()> {
    let mut status = shared.background_checkpoint.lock().unwrap();
    while status.running {
        status = shared
            .background_checkpoint_done
            .wait(status)
            .unwrap_or_else(|poison| poison.into_inner());
    }
    if let Some(message) = status.last_error.take() {
        if std::mem::take(&mut status.commit_unknown) {
            return Err(Error::CommitUnknown(message));
        }
        return Err(Error::Io(std::io::Error::other(format!(
            "background checkpoint failed: {message}"
        ))));
    }
    Ok(())
}

fn take_background_checkpoint_error(shared: &Shared) -> Result<()> {
    let mut status = shared.background_checkpoint.lock().unwrap();
    if !status.running {
        if let Some(message) = status.last_error.take() {
            if std::mem::take(&mut status.commit_unknown) {
                return Err(Error::CommitUnknown(message));
            }
            return Err(Error::Io(std::io::Error::other(format!(
                "background checkpoint failed: {message}"
            ))));
        }
    }
    Ok(())
}

fn lock_commit_for_maintenance<'a>(shared: &'a Arc<Shared>) -> Result<CommitGuard<'a>> {
    ensure_canonical_writable(shared)?;
    loop {
        wait_for_background_checkpoint(shared)?;
        wait_for_background_derived(shared)?;
        let guard = lock_commit_after_group_sync(shared);
        let derived_running = shared.background_derived.lock().unwrap().running;
        if shared.state.read().unwrap().index.frozen.is_none() && !derived_running {
            return Ok(guard);
        }
        drop(guard);
    }
}

fn wait_vector_indexing_shared(shared: &Shared) -> Result<()> {
    while shared.vector_backlog.load(AtomicOrdering::SeqCst) > 0 {
        if !shared.vector_worker_alive.load(AtomicOrdering::Acquire) {
            let message = shared
                .vector_worker_error
                .lock()
                .unwrap_or_else(|poison| poison.into_inner())
                .clone()
                .unwrap_or_else(|| "vector indexing worker stopped with pending work".into());
            return Err(Error::Io(std::io::Error::other(message)));
        }
        std::thread::sleep(Duration::from_millis(1));
    }
    Ok(())
}

/// WAL rotation must never overtake a group whose callers still depend on the
/// current generation's sync result. Ordinary run-manifest publication may
/// use the raw mutex because it does not replace the WAL.
fn lock_commit_after_group_sync(shared: &Arc<Shared>) -> CommitGuard<'_> {
    loop {
        let guard = shared.commit.lock();
        let Some(group) = guard.wal_sync_group.clone() else {
            return guard;
        };
        drop(guard);
        let _ = group.wait();
    }
}

fn ensure_canonical_writable(shared: &Shared) -> Result<()> {
    if let Some(message) = shared.canonical_error.lock().unwrap().as_ref() {
        return Err(Error::CommitUnknown(format!(
            "database writes are fenced until reopen after an uncertain canonical publication: {message}"
        )));
    }
    Ok(())
}

fn publication_sync_error(
    shared: &Shared,
    operation: &str,
    outcome: PublishOutcome,
) -> Option<Error> {
    let PublishOutcome::SyncFailed(error) = outcome else {
        return None;
    };
    let message =
        format!("{operation} was renamed into place, but syncing its directory failed: {error}");
    *shared.canonical_error.lock().unwrap() = Some(message.clone());
    Some(Error::CommitUnknown(message))
}

/// Publish a catalog-only generation while the caller holds commit + state.
/// `catalog.json` is a compatibility mirror; the manifest-embedded catalog is
/// the canonical schema paired with its exact segment/WAL generation.
fn publish_catalog_generation_locked(
    shared: &Arc<Shared>,
    _cs: &mut CommitState,
    state: &mut State,
    next: Catalog,
) -> Result<Option<Error>> {
    // Writing the mirror first is safe: if canonical publication does not
    // happen, current engines ignore the ahead mirror in favor of the embedded
    // catalog in the old manifest.
    next.save(&shared.dir.join(CATALOG_FILE))?;
    let (current, _) = Manifest::load(&shared.dir)?;
    let outcome = Manifest {
        format_version: FORMAT_VERSION,
        committed_version: current.committed_version,
        segments: current.segments,
        wal_id: current.wal_id,
        identity_high_water: current.identity_high_water,
        catalog: Some(next.clone()),
    }
    .publish(&shared.dir)?;
    state.catalog = next;
    Ok(publication_sync_error(shared, "catalog manifest", outcome))
}

fn fence_writes(shared: &Shared, message: impl Into<String>) {
    *shared.canonical_error.lock().unwrap() = Some(message.into());
}

fn background_checkpoint_supported(shared: &Shared) -> bool {
    // Canonical primary data can be frozen independently of disposable
    // secondary/text/vector overlays. Async vector jobs are the only exception:
    // their charged payloads have not reached the overlay yet, so freezing until
    // they drain keeps memory accounting and index publication ordered.
    shared.vector_backlog.load(AtomicOrdering::SeqCst) == 0
}

/// Freeze the active primary delta in O(1), transfer its memory charge to the
/// maintenance pool, and enqueue its durable flush. The caller holds the
/// commit mutex, so the WAL boundary and MVCC version describe exactly the
/// frozen generation.
fn schedule_frozen_checkpoint(
    shared: &Arc<Shared>,
    cs: &mut CommitState,
    memory: MemoryPermit,
) -> Result<bool> {
    if !background_checkpoint_supported(shared) {
        return Ok(false);
    }
    let mut status = shared.background_checkpoint.lock().unwrap();
    if status.running {
        return Ok(false);
    }
    debug_assert!(status.last_error.is_none(), "checked before WAL commit");
    status.commit_unknown = false;
    let (job, retained_index_bytes) = {
        let mut state = shared.state.write().unwrap();
        if state.index.frozen.is_some() || state.index.delta.is_empty() {
            return Ok(false);
        }
        let frozen = Arc::new(std::mem::take(&mut state.index.delta));
        let job = FrozenCheckpointJob {
            frozen: frozen.clone(),
            version: state.committed_version,
            segments: state.segments.clone(),
            next_segment_id: state.next_segment_id,
            catalog: state.catalog.clone(),
            identity_high_water: identity_manifest(&state),
            first_primary_run: state.index.runs.is_empty(),
            wal_id: cs.wal().id,
            wal_cutoff: cs.wal().len,
            memory: Some(memory),
        };
        state.index.frozen = Some(FrozenPrimary {
            version: job.version,
            delta: frozen,
        });
        let retained = state.index_delta_memory_bytes();
        (job, retained)
    };
    cs.memtable_bytes = 0;
    // Only the primary delta moved to the maintenance-owned frozen generation;
    // derived overlays remain live and must stay charged to their pool.
    shared
        .memory_governor
        .set_index_delta_bytes(retained_index_bytes);
    status.running = true;
    let sent = shared
        .checkpoint_tx
        .lock()
        .unwrap()
        .as_ref()
        .is_some_and(|sender| sender.send(job).is_ok());
    if !sent {
        status.running = false;
        drop(status);
        thaw_frozen_checkpoint_locked(shared);
        return Ok(false);
    }
    shared.memory_governor.record_index_consolidation();
    Ok(true)
}

fn thaw_frozen_checkpoint(shared: &Arc<Shared>) {
    let _commit = shared.commit.lock();
    thaw_frozen_checkpoint_locked(shared);
}

fn thaw_frozen_checkpoint_locked(shared: &Arc<Shared>) {
    let mut state = shared.state.write().unwrap();
    let Some(frozen) = state.index.frozen.take() else {
        return;
    };
    for (table, ids) in frozen.delta.iter() {
        let active = state.index.delta.entry(table.clone()).or_default();
        for (id, versions) in ids {
            let merged = active.entry(id.clone()).or_default();
            merged.extend(versions.iter().cloned());
            merged.sort_unstable_by_key(|entry| entry.version);
            merged.dedup_by_key(|entry| entry.version);
        }
    }
    let retained = state.index_delta_memory_bytes();
    drop(state);
    shared.memory_governor.set_index_delta_bytes(retained);
}

fn flush_frozen_checkpoint(shared: &Arc<Shared>, mut job: FrozenCheckpointJob) -> Result<()> {
    let started = Instant::now();
    let result = flush_frozen_checkpoint_inner(shared, &job);
    if result.is_err() {
        // Publication did not complete. Return the frozen generation to the
        // active pool; the original WAL still contains every commit.
        drop(job.memory.take());
        thaw_frozen_checkpoint(shared);
    } else {
        shared.checkpoint_nanos.fetch_add(
            started.elapsed().as_nanos().min(u64::MAX as u128) as u64,
            AtomicOrdering::Relaxed,
        );
    }
    result
}

fn flush_frozen_checkpoint_inner(shared: &Arc<Shared>, job: &FrozenCheckpointJob) -> Result<()> {
    struct ReservedWalFiles {
        bridge: PathBuf,
        active: PathBuf,
        armed: bool,
    }

    impl Drop for ReservedWalFiles {
        fn drop(&mut self) {
            if self.armed {
                let _ = fs::remove_file(&self.active);
                let _ = fs::remove_file(&self.bridge);
            }
        }
    }

    struct MemEntry {
        table_index: usize,
        id_start: usize,
        id_len: usize,
        version: u64,
        payload: Option<Arc<Vec<u8>>>,
    }

    let mem_count = job
        .frozen
        .values()
        .flat_map(|ids| ids.values())
        .map(Vec::len)
        .sum();
    let id_bytes = job
        .frozen
        .values()
        .flat_map(|ids| ids.keys())
        .map(String::len)
        .sum();
    let mut mem = Vec::with_capacity(mem_count);
    let mut mem_tables = Vec::with_capacity(job.frozen.len());
    let mut mem_ids = Vec::with_capacity(id_bytes);
    let mut new_segment_superseded = false;
    let mut delta_tables: Vec<_> = job.frozen.iter().collect();
    delta_tables.sort_unstable_by_key(|(table, _)| primary_table_prefix(table));
    for (table, ids) in delta_tables {
        let table_index = mem_tables.len();
        mem_tables.push(table.clone());
        for (id, versions) in ids {
            if versions.len() > 1 {
                new_segment_superseded = true;
            }
            let id_start = mem_ids.len();
            mem_ids.extend_from_slice(id.as_bytes());
            for version in versions {
                let payload = match &version.kind {
                    VKind::MemPut(payload) => Some(payload.clone()),
                    VKind::MemTombstone => None,
                    _ => continue,
                };
                mem.push(MemEntry {
                    table_index,
                    id_start,
                    id_len: id.len(),
                    version: version.version,
                    payload,
                });
            }
        }
    }
    if mem.is_empty() {
        return Ok(());
    }

    let seg_id = job.next_segment_id;
    let seg_path = shared
        .dir
        .join(SEGMENTS_DIR)
        .join(segment_file_name(seg_id));
    let mut locs = vec![(0, 0); mem.len()];
    let mut segment_order: Vec<usize> = (0..mem.len()).collect();
    segment_order.sort_unstable_by_key(|index| mem[*index].version);
    let raw = File::create(&seg_path)?;
    let writer_bytes = shared
        .opts
        .memory
        .maintenance_pool_bytes
        .clamp(1, 1024 * 1024);
    let mut writer = BufWriter::with_capacity(writer_bytes, raw);
    let mut position = 0u64;
    let mut encoded = Vec::new();
    for index in segment_order {
        let entry = &mem[index];
        let table = &mem_tables[entry.table_index];
        let id = std::str::from_utf8(&mem_ids[entry.id_start..entry.id_start + entry.id_len])
            .expect("copied from a String");
        let payload_rel = encode_entry_into(
            &mut encoded,
            entry.version,
            table,
            id,
            entry.payload.as_deref().map(Vec::as_slice),
        )?;
        locs[index] = (
            position + payload_rel,
            entry
                .payload
                .as_ref()
                .map_or(0, |payload| payload.len() as u32),
        );
        writer.write_all(&encoded)?;
        position = position.saturating_add(encoded.len() as u64);
    }
    writer.flush()?;
    let segment_file = writer
        .into_inner()
        .map_err(|error| Error::Io(error.into_error()))?;
    segment_file.sync_all()?;
    fsync_dir(&shared.dir.join(SEGMENTS_DIR))?;

    let mut new_segments = job.segments.clone();
    new_segments.push(SegmentMeta {
        id: seg_id,
        len: position,
    });
    let generation = primary_generation(job.version, &new_segments, &job.catalog);
    let indexes_dir = shared.dir.join(INDEXES_DIR);
    // Protect the prepared run from another publisher's orphan cleanup. This
    // mutex never serializes ordinary commits, and primary compaction releases
    // its maintenance memory before acquiring it, preserving lock order.
    let _publication = shared.primary_manifest_publication.lock().unwrap();
    let file = if job.first_primary_run {
        "primary.pidx".to_owned()
    } else {
        format!("primary-L0-{}.pidx.run", Ulid::new())
    };
    let level = if job.first_primary_run {
        PRIMARY_BASE_LEVEL
    } else {
        0
    };
    let primary_path = indexes_dir.join(&file);
    let primary_tmp = primary_path.with_extension("run.tmp");
    let mut primary_writer = PagedWriter::create(&primary_tmp, generation, None)?;
    let mut key = Vec::new();
    let mut value = Vec::with_capacity(25);
    let write_result = (|| -> Result<()> {
        for (entry, (payload_offset, payload_len)) in mem.iter().zip(&locs) {
            let table = &mem_tables[entry.table_index];
            let epoch = job.catalog.table(table).map_or(0, |schema| schema.epoch);
            if entry.version <= epoch {
                continue;
            }
            let id = std::str::from_utf8(&mem_ids[entry.id_start..entry.id_start + entry.id_len])
                .expect("copied from a String");
            encode_primary_key_into(table, id, &mut key);
            let kind = if entry.payload.is_some() {
                VKind::SegPut {
                    segment: seg_id,
                    payload_offset: *payload_offset,
                    payload_len: *payload_len,
                }
            } else {
                VKind::SegTombstone
            };
            encode_primary_entry_into(
                &VersionEntry {
                    version: entry.version,
                    kind,
                },
                &mut value,
            )?;
            primary_writer.add(&key, &value)?;
        }
        primary_writer.finish()
    })();
    if let Err(error) = write_result {
        let _ = fs::remove_file(&primary_tmp);
        return Err(error);
    }
    fs::rename(&primary_tmp, &primary_path)?;
    fsync_dir(&indexes_dir)?;
    let primary_bytes = fs::metadata(&primary_path)?.len();
    let primary_meta = PrimaryRunMeta {
        file,
        level,
        bytes: primary_bytes,
        generation,
    };
    let primary_index = Arc::new(PagedIndex::open(&primary_path)?);
    let segment_reader = File::open(&seg_path)?;

    // Two durable empty successor WALs form a recovery bridge before the
    // active writer is switched: a crash sees either the old manifest + old
    // WAL + successors, or the new manifest + copied tail + active successor.
    let bridge_wal_id = job
        .wal_id
        .checked_add(1)
        .ok_or_else(|| Error::Corrupt("WAL id exhausted during checkpoint".into()))?;
    let active_wal_id = job
        .wal_id
        .checked_add(2)
        .ok_or_else(|| Error::Corrupt("WAL id exhausted during checkpoint".into()))?;
    let wal_dir = shared.dir.join(WAL_DIR);
    let bridge_path = wal_path(&shared.dir, bridge_wal_id);
    let active_path = wal_path(&shared.dir, active_wal_id);
    OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&bridge_path)?
        .sync_all()?;
    if let Err(error) = OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&active_path)
        .and_then(|file| file.sync_all())
    {
        let _ = fs::remove_file(&bridge_path);
        return Err(error.into());
    }
    let mut reserved_wals = ReservedWalFiles {
        bridge: bridge_path.clone(),
        active: active_path.clone(),
        armed: true,
    };
    fsync_dir(&wal_dir)?;
    let active_wal = WalWriter::open(&shared.dir, active_wal_id)?;

    // The mutex is held only for validation and the in-memory writer swap.
    // All WAL copying and manifest fsyncs happen after it is released.
    let (mut old_wal, tail_len, mut new_primary_runs) = {
        let mut cs = lock_commit_after_group_sync(shared);
        if cs.wal().id != job.wal_id || cs.wal().len < job.wal_cutoff {
            return Err(Error::Corrupt(
                "background checkpoint observed an unexpected WAL generation".into(),
            ));
        }
        let state = shared.state.read().unwrap();
        let still_frozen = state.index.frozen.as_ref().is_some_and(|frozen| {
            frozen.version == job.version && Arc::ptr_eq(&frozen.delta, &job.frozen)
        });
        if !still_frozen {
            return Err(Error::Corrupt(
                "background checkpoint lost its frozen generation".into(),
            ));
        }
        let runs = state.index.run_metas();
        drop(state);
        let tail_len = cs.wal().len - job.wal_cutoff;
        let old_wal = cs
            .wal
            .replace(active_wal)
            .expect("writable checkpoint has an active WAL");
        (old_wal, tail_len, runs)
    };
    // From this point forward the active writer may contain acknowledged
    // commits. Neither successor may be removed on an error; recovery follows
    // the consecutive WAL chain from the still-current manifest.
    reserved_wals.armed = false;

    if let WalAppendOutcome::SyncFailed(error) = old_wal.sync_data() {
        let _commit = shared.commit.lock();
        thaw_frozen_checkpoint_locked(shared);
        return Err(error.into());
    }

    // Build the bridge through a temporary inode. Recovery can safely cross
    // the durable empty placeholder while this copy is incomplete; the full
    // duplicate tail appears atomically only after it has been synced.
    let bridge_tmp = wal_dir.join(format!("{bridge_wal_id:06}.wal.bridge.tmp"));
    let mut source = File::open(wal_path(&shared.dir, job.wal_id))?;
    source.seek(SeekFrom::Start(job.wal_cutoff))?;
    let mut raw_tail = source.take(tail_len);
    let target = File::create(&bridge_tmp)?;
    let mut target = BufWriter::with_capacity(writer_bytes, target);
    let copied = std::io::copy(&mut raw_tail, &mut target)?;
    if copied != tail_len {
        return Err(Error::Corrupt(
            "background checkpoint could not copy the complete WAL tail".into(),
        ));
    }
    target.flush()?;
    let target = target
        .into_inner()
        .map_err(|error| Error::Io(error.into_error()))?;
    target.sync_all()?;
    fs::rename(&bridge_tmp, &bridge_path)?;
    fsync_dir(&wal_dir)?;

    #[cfg(test)]
    {
        shared
            .primary_test_reached_manifest
            .store(true, AtomicOrdering::Release);
        while shared
            .primary_test_pause_before_manifest
            .load(AtomicOrdering::Acquire)
        {
            std::thread::yield_now();
        }
    }

    new_primary_runs.push(primary_meta.clone());
    let primary_publish_error = PrimaryRunManifest::new(generation, new_primary_runs)
        .publish(&indexes_dir)
        .err();
    let published_manifest = Manifest {
        format_version: FORMAT_VERSION,
        committed_version: job.version,
        segments: new_segments.clone(),
        wal_id: bridge_wal_id,
        identity_high_water: job.identity_high_water.clone(),
        catalog: Some(job.catalog.clone()),
    };
    #[cfg(test)]
    if shared
        .primary_test_fail_before_manifest_rename
        .swap(false, AtomicOrdering::AcqRel)
    {
        return Err(Error::Io(std::io::Error::other(
            "injected primary manifest rename failure",
        )));
    }
    let manifest_outcome = match published_manifest.publish(&shared.dir) {
        Ok(outcome) => outcome,
        Err(error) => {
            let _commit = shared.commit.lock();
            thaw_frozen_checkpoint_locked(shared);
            return Err(error);
        }
    };
    #[cfg(test)]
    let manifest_outcome = if shared
        .primary_test_fail_after_manifest_rename
        .swap(false, AtomicOrdering::AcqRel)
    {
        PublishOutcome::SyncFailed(std::io::Error::other(
            "injected primary manifest directory sync failure",
        ))
    } else {
        manifest_outcome
    };

    let retained = {
        let _commit = shared.commit.lock();
        let mut state = shared.state.write().unwrap();
        let still_frozen = state.index.frozen.as_ref().is_some_and(|frozen| {
            frozen.version == job.version && Arc::ptr_eq(&frozen.delta, &job.frozen)
        });
        if !still_frozen {
            return Err(Error::Corrupt(
                "background checkpoint lost its frozen generation before adoption".into(),
            ));
        }
        if !new_segment_superseded {
            new_segment_superseded = state.index.delta.iter().any(|(table, ids)| {
                job.frozen
                    .get(table)
                    .is_some_and(|frozen_ids| ids.keys().any(|id| frozen_ids.contains_key(id)))
            });
        }
        state.readers.insert(seg_id, Arc::new(segment_reader));
        state.segments = new_segments;
        state.next_segment_id = seg_id + 1;
        if new_segment_superseded {
            state.superseded_segments.insert(seg_id);
        }
        state.index.frozen = None;
        state.index.generation = generation;
        state.index.runs.push(PrimaryRun {
            meta: primary_meta,
            index: primary_index,
        });
        state.index_delta_memory_bytes()
    };

    let publication_error =
        publication_sync_error(shared, "background checkpoint manifest", manifest_outcome);
    if publication_error.is_none() {
        cleanup_orphans(
            &shared.dir,
            &Manifest {
                format_version: FORMAT_VERSION,
                committed_version: job.version,
                segments: shared.state.read().unwrap().segments.clone(),
                wal_id: bridge_wal_id,
                identity_high_water: job.identity_high_water.clone(),
                catalog: Some(job.catalog.clone()),
            },
        )?;
    }
    shared
        .primary_checkpoint_bytes_written
        .fetch_add(primary_bytes, AtomicOrdering::Relaxed);
    shared.memory_governor.set_index_delta_bytes(retained);
    shared
        .checkpoint_count
        .fetch_add(1, AtomicOrdering::Relaxed);
    if let Some(error) = primary_publish_error {
        record_maintenance_error(
            shared,
            format!("primary run manifest will be rebuilt on reopen: {error}"),
        );
        shared
            .index_maintenance_failures
            .fetch_add(1, AtomicOrdering::Relaxed);
    }
    cleanup_primary_run_orphans(&shared.dir);
    maybe_schedule_primary_compaction(shared);
    refresh_compaction_debt_if_needed(shared);
    maybe_schedule_auto_compaction(shared);
    publication_error.map_or(Ok(()), Err)
}

fn checkpoint_locked(shared: &Arc<Shared>, cs: &mut CommitState) -> Result<()> {
    if shared.opts.read_only {
        return Err(Error::ReadOnly);
    }
    struct MemEntry {
        table_index: usize,
        id_start: usize,
        id_len: usize,
        version: u64,
        payload: Option<Arc<Vec<u8>>>,
    }
    let (
        mem,
        mem_tables,
        mem_ids,
        segments,
        committed_version,
        next_segment_id,
        new_segment_superseded,
        catalog,
        identity_high_water,
        old_primary_runs,
        first_primary_run,
    ) = {
        let st = shared.state.read().unwrap();
        let mem_count = st
            .index
            .delta
            .values()
            .flat_map(|ids| ids.values())
            .flat_map(|versions| versions.iter())
            .filter(|version| matches!(&version.kind, VKind::MemPut(_) | VKind::MemTombstone))
            .count();
        let id_bytes = st
            .index
            .delta
            .values()
            .flat_map(|ids| ids.iter())
            .filter(|(_, versions)| {
                versions
                    .iter()
                    .any(|version| matches!(&version.kind, VKind::MemPut(_) | VKind::MemTombstone))
            })
            .map(|(id, _)| id.len())
            .sum();
        let mut mem: Vec<MemEntry> = Vec::with_capacity(mem_count);
        let mut mem_tables = Vec::with_capacity(st.index.delta.len());
        let mut mem_ids = Vec::with_capacity(id_bytes);
        let mut new_segment_superseded = false;
        let mut delta_tables: Vec<_> = st.index.delta.iter().collect();
        delta_tables.sort_unstable_by_key(|(table, _)| primary_table_prefix(table));
        for (table, ids) in delta_tables {
            let table_index = mem_tables.len();
            mem_tables.push(table.clone());
            for (id, versions) in ids {
                let resident_versions = versions.iter().filter(|version| {
                    matches!(&version.kind, VKind::MemPut(_) | VKind::MemTombstone)
                });
                let resident_count = resident_versions.clone().count();
                if resident_count > 1 {
                    new_segment_superseded = true;
                }
                if resident_count == 0 {
                    continue;
                }
                let id_start = mem_ids.len();
                mem_ids.extend_from_slice(id.as_bytes());
                for v in resident_versions {
                    match &v.kind {
                        VKind::MemPut(p) => mem.push(MemEntry {
                            table_index,
                            id_start,
                            id_len: id.len(),
                            version: v.version,
                            payload: Some(p.clone()),
                        }),
                        VKind::MemTombstone => mem.push(MemEntry {
                            table_index,
                            id_start,
                            id_len: id.len(),
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
            mem_tables,
            mem_ids,
            st.segments.clone(),
            st.committed_version,
            st.next_segment_id,
            new_segment_superseded,
            st.catalog.clone(),
            identity_manifest(&st),
            st.index.run_metas(),
            st.index.runs.is_empty(),
        )
    };
    if mem.is_empty() && cs.wal().len == 0 {
        return Ok(());
    }
    let mut new_segments = segments;
    let mut written: Option<(u32, Vec<(u64, u32)>)> = None;
    if !mem.is_empty() {
        let seg_id = next_segment_id;
        let mut locs = vec![(0, 0); mem.len()];
        let mut segment_order: Vec<usize> = (0..mem.len()).collect();
        segment_order.sort_unstable_by_key(|index| mem[*index].version);
        let seg_path = shared
            .dir
            .join(SEGMENTS_DIR)
            .join(segment_file_name(seg_id));
        let raw = File::create(&seg_path)?;
        let writer_bytes = shared
            .opts
            .memory
            .maintenance_pool_bytes
            .clamp(1, 1024 * 1024);
        let mut writer = BufWriter::with_capacity(writer_bytes, raw);
        let mut position = 0u64;
        let mut entry = Vec::new();
        for index in segment_order {
            let m = &mem[index];
            let table = &mem_tables[m.table_index];
            let id = std::str::from_utf8(&mem_ids[m.id_start..m.id_start + m.id_len])
                .expect("copied from a String");
            let payload_rel = encode_entry_into(
                &mut entry,
                m.version,
                table,
                id,
                m.payload.as_ref().map(|p| p.as_slice()),
            )?;
            locs[index] = (
                position + payload_rel,
                m.payload.as_ref().map_or(0, |p| p.len() as u32),
            );
            writer.write_all(&entry)?;
            position = position.saturating_add(entry.len() as u64);
        }
        writer.flush()?;
        let f = writer
            .into_inner()
            .map_err(|error| Error::Io(error.into_error()))?;
        f.sync_all()?;
        fsync_dir(&shared.dir.join(SEGMENTS_DIR))?;
        new_segments.push(SegmentMeta {
            id: seg_id,
            len: position,
        });
        written = Some((seg_id, locs));
    }

    // The compact snapshot is already in primary-key order. Build the
    // disposable primary run directly from it and the segment offsets instead
    // of writing those offsets back into every B-tree entry only to scan and
    // clear the same delta immediately afterward.
    let generation = primary_generation(committed_version, &new_segments, &catalog);
    let prepared_primary = if let Some((seg_id, locs)) = &written {
        let indexes_dir = shared.dir.join(INDEXES_DIR);
        let file = if first_primary_run {
            "primary.pidx".to_owned()
        } else {
            format!("primary-L0-{}.pidx.run", Ulid::new())
        };
        let level = if first_primary_run {
            PRIMARY_BASE_LEVEL
        } else {
            0
        };
        let path = indexes_dir.join(&file);
        let tmp = path.with_extension("run.tmp");
        let mut writer = PagedWriter::create(&tmp, generation, None)?;
        let mut key = Vec::new();
        let mut value = Vec::with_capacity(25);
        let mut entries = 0usize;
        let write_result = (|| -> Result<()> {
            for (m, (payload_offset, payload_len)) in mem.iter().zip(locs) {
                let table = &mem_tables[m.table_index];
                let epoch = catalog.table(table).map_or(0, |schema| schema.epoch);
                if m.version <= epoch {
                    continue;
                }
                let id = std::str::from_utf8(&mem_ids[m.id_start..m.id_start + m.id_len])
                    .expect("copied from a String");
                encode_primary_key_into(table, id, &mut key);
                let kind = match m.payload {
                    Some(_) => VKind::SegPut {
                        segment: *seg_id,
                        payload_offset: *payload_offset,
                        payload_len: *payload_len,
                    },
                    None => VKind::SegTombstone,
                };
                encode_primary_entry_into(
                    &VersionEntry {
                        version: m.version,
                        kind,
                    },
                    &mut value,
                )?;
                writer.add(&key, &value)?;
                entries += 1;
            }
            writer.finish()
        })();
        if let Err(error) = write_result {
            let _ = fs::remove_file(&tmp);
            return Err(error);
        }
        if entries == 0 {
            let _ = fs::remove_file(&tmp);
            None
        } else {
            fs::rename(&tmp, &path)?;
            fsync_dir(&indexes_dir)?;
            let bytes = fs::metadata(&path)?.len();
            let meta = PrimaryRunMeta {
                file,
                level,
                bytes,
                generation,
            };
            let index = Arc::new(PagedIndex::open(&path)?);
            Some((meta, index))
        }
    } else {
        None
    };
    let mut prepared_segment_reader = match &written {
        Some((seg_id, _)) => Some(File::open(
            shared
                .dir
                .join(SEGMENTS_DIR)
                .join(segment_file_name(*seg_id)),
        )?),
        None => None,
    };

    // Create the new WAL before the manifest that references it, so the
    // manifest never points at a missing file.
    let new_wal_id = cs.wal().id + 1;
    File::create(wal_path(&shared.dir, new_wal_id))?.sync_all()?;
    fsync_dir(&shared.dir.join(WAL_DIR))?;
    let next_wal = WalWriter::open(&shared.dir, new_wal_id)?;
    let manifest_outcome = Manifest {
        format_version: FORMAT_VERSION,
        committed_version,
        segments: new_segments.clone(),
        wal_id: new_wal_id,
        identity_high_water,
        catalog: Some(catalog.clone()),
    }
    .publish(&shared.dir)?;

    // Canonical publication has succeeded. Switch to the new WAL before any
    // disposable index work so a failed run-manifest publication cannot leave
    // future commits appending to an unlinked old WAL inode.
    cs.wal = Some(next_wal);

    {
        let mut st = shared.state.write().unwrap();
        if let Some((seg_id, _)) = &written {
            st.readers.insert(
                *seg_id,
                Arc::new(
                    prepared_segment_reader
                        .take()
                        .expect("reader prepared before canonical publication"),
                ),
            );
            st.next_segment_id = seg_id + 1;
            if new_segment_superseded {
                st.superseded_segments.insert(*seg_id);
            }
        }
        st.segments = new_segments;
    }

    let mut new_primary_runs = old_primary_runs;
    if let Some((meta, _)) = &prepared_primary {
        new_primary_runs.push(meta.clone());
    }
    let primary_publish_error = PrimaryRunManifest::new(generation, new_primary_runs)
        .publish(&shared.dir.join(INDEXES_DIR))
        .err();

    {
        let mut st = shared.state.write().unwrap();
        st.index.delta.clear();
        st.index.generation = generation;
        if let Some((meta, index)) = prepared_primary {
            shared
                .primary_checkpoint_bytes_written
                .fetch_add(meta.bytes, AtomicOrdering::Relaxed);
            st.index.runs.push(PrimaryRun { meta, index });
        }
    }
    cs.memtable_bytes = 0;
    let publication_error = publication_sync_error(shared, "checkpoint manifest", manifest_outcome);
    if let Some(error) = primary_publish_error {
        if publication_error.is_none() {
            fence_writes(
                shared,
                format!("checkpoint published but primary index publication failed: {error}"),
            );
        }
        return Err(publication_error.unwrap_or(error));
    }
    cleanup_primary_run_orphans(&shared.dir);
    maybe_schedule_primary_compaction(shared);
    let retained = shared.state.read().unwrap().index_delta_memory_bytes();
    shared.memory_governor.set_index_delta_bytes(retained);
    if publication_error.is_none() {
        cleanup_orphans(
            &shared.dir,
            &Manifest {
                format_version: FORMAT_VERSION,
                committed_version,
                segments: shared.state.read().unwrap().segments.clone(),
                wal_id: new_wal_id,
                identity_high_water: identity_manifest(&shared.state.read().unwrap()),
                catalog: Some(shared.state.read().unwrap().catalog.clone()),
            },
        )?;
    }
    publication_error.map_or(Ok(()), Err)
}

/// Move every mutable derived overlay into an immutable in-memory generation
/// in O(number of indexes), then let the maintenance worker perform graph/run
/// serialization without the commit mutex or state write lock.
fn schedule_frozen_derived(shared: &Arc<Shared>, memory: MemoryPermit) -> Result<bool> {
    let vector_ready = shared.vector_backlog.load(AtomicOrdering::SeqCst) == 0;
    let mut status = shared.background_derived.lock().unwrap();
    if status.running {
        return Ok(false);
    }
    status.last_error = None;

    let (secondary, text, vector, retained) = {
        let mut state = shared.state.write().unwrap();
        let version = state.committed_version;
        let mut secondary = Vec::new();
        for (key, index) in &mut state.secondary {
            let frozen = index.frozen.clone().or_else(|| index.freeze_delta(version));
            if let Some(frozen) = frozen {
                secondary.push(FrozenSecondaryJob {
                    key: key.clone(),
                    frozen,
                });
            }
        }
        let mut text = Vec::new();
        for (key, index) in &mut state.text {
            let frozen = index
                .frozen_delta()
                .or_else(|| index.freeze_delta_background(version));
            if let Some(frozen) = frozen {
                text.push(FrozenTextJob {
                    key: key.clone(),
                    frozen,
                });
            }
        }
        let definitions: HashMap<(String, String), VectorIndexDef> = state
            .catalog
            .tables
            .iter()
            .flat_map(|table| {
                table
                    .vector_indexes
                    .iter()
                    .cloned()
                    .map(|def| ((table.name.clone(), def.column.clone()), def))
            })
            .collect();
        let mut vector = Vec::new();
        for (key, index) in &mut state.vector {
            let frozen = index.frozen_delta().or_else(|| {
                vector_ready
                    .then(|| index.freeze_delta_background(version))
                    .flatten()
            });
            if let (Some(frozen), Some(generation), Some(def)) =
                (frozen, index.frozen_generation(), definitions.get(key))
            {
                vector.push(FrozenVectorJob {
                    table: key.0.clone(),
                    def: def.clone(),
                    frozen,
                    generation,
                });
            }
        }
        let retained = state.index_delta_memory_bytes();
        (secondary, text, vector, retained)
    };

    if secondary.is_empty() && text.is_empty() && vector.is_empty() {
        return Ok(false);
    }
    shared.memory_governor.set_index_delta_bytes(retained);
    let job = DerivedCheckpointJob {
        secondary,
        text,
        vector,
        memory: Some(memory),
    };
    status.running = true;
    let sent = shared
        .derived_tx
        .lock()
        .unwrap()
        .as_ref()
        .is_some_and(|sender| sender.send(job).is_ok());
    if !sent {
        status.running = false;
        return Err(Error::Io(std::io::Error::new(
            std::io::ErrorKind::BrokenPipe,
            "derived-index maintenance worker is unavailable",
        )));
    }
    shared.memory_governor.record_index_consolidation();
    Ok(true)
}

fn wait_for_background_derived(shared: &Arc<Shared>) -> Result<()> {
    let mut status = shared.background_derived.lock().unwrap();
    while status.running {
        status = shared.background_derived_done.wait(status).unwrap();
    }
    if let Some(message) = status.last_error.take() {
        return Err(Error::Io(std::io::Error::other(format!(
            "background derived-index publication failed: {message}"
        ))));
    }
    Ok(())
}

fn flush_frozen_derived(shared: &Arc<Shared>, job: DerivedCheckpointJob) -> Result<()> {
    let mut stage = "initializing derived-index publication".to_owned();
    flush_frozen_derived_inner(shared, job, &mut stage)
        .map_err(|error| Error::Io(std::io::Error::other(format!("{stage}: {error}"))))
}

#[cfg(test)]
fn pause_derived_before_publish_for_test(shared: &Shared) {
    shared
        .derived_test_reached_publish
        .store(true, AtomicOrdering::Release);
    while shared
        .derived_test_pause_before_publish
        .load(AtomicOrdering::Acquire)
    {
        std::thread::sleep(Duration::from_millis(1));
    }
}

#[cfg(not(test))]
fn pause_derived_before_publish_for_test(_shared: &Shared) {}

fn publish_background_derived_manifest(
    _shared: &Shared,
    manifest: PreparedDerivedRunManifest,
) -> Result<PublishOutcome> {
    #[cfg(test)]
    if _shared
        .derived_test_fail_before_manifest_rename
        .swap(false, AtomicOrdering::AcqRel)
    {
        return Err(Error::Io(std::io::Error::other(
            "injected derived manifest rename failure",
        )));
    }
    let outcome = manifest.publish()?;
    #[cfg(test)]
    let outcome = if _shared
        .derived_test_fail_after_manifest_rename
        .swap(false, AtomicOrdering::AcqRel)
    {
        PublishOutcome::SyncFailed(std::io::Error::other(
            "injected derived manifest directory sync failure",
        ))
    } else {
        outcome
    };
    Ok(outcome)
}

fn flush_frozen_derived_inner(
    shared: &Arc<Shared>,
    mut job: DerivedCheckpointJob,
    stage: &mut String,
) -> Result<()> {
    let _publication = shared.derived_manifest_publication.lock().unwrap();
    let indexes_dir = shared.dir.join(INDEXES_DIR);
    let vectors_dir = shared.dir.join(VECTORS_DIR);
    fs::create_dir_all(&indexes_dir)?;
    fs::create_dir_all(&vectors_dir)?;
    let temp_dir = shared
        .opts
        .memory
        .spill_directory
        .clone()
        .unwrap_or_else(|| indexes_dir.join("tmp"));
    fs::create_dir_all(&temp_dir)?;
    let budget = shared.opts.memory.maintenance_pool_bytes;

    for frozen_job in &job.secondary {
        *stage = format!(
            "writing secondary index {}.{}",
            frozen_job.key.0, frozen_job.key.1
        );
        let file = sidx_run_filename(&shared.dir, &frozen_job.key.0, &frozen_job.key.1, 0);
        let path = indexes_dir.join(&file);
        let prepared = path.with_file_name(format!("{file}.derived-pending"));
        let tmp = path.with_file_name(format!("{file}.derived-pending.tmp"));
        let generation = frozen_job.frozen.generation;
        let mut writer = ExternalPagedWriter::new(&tmp, &temp_dir, generation, budget)?;
        writer.add(SECONDARY_FORMAT_KEY, SECONDARY_FORMAT_VALUE)?;
        for (value, ids) in &frozen_job.frozen.delta {
            for id in ids {
                writer.add(
                    &secondary_pair_key(value, id),
                    &secondary_operation(generation, SECONDARY_ADD),
                )?;
            }
        }
        for (value, ids) in &frozen_job.frozen.removed {
            for id in ids {
                writer.add(
                    &secondary_pair_key(value, id),
                    &secondary_operation(generation, SECONDARY_DELETE),
                )?;
            }
        }
        if let Err(error) = writer.finish() {
            let _ = fs::remove_file(&tmp);
            return Err(error);
        }
        fs::rename(&tmp, &prepared).map_err(|error| {
            Error::Io(std::io::Error::new(
                error.kind(),
                format!(
                    "publishing frozen secondary run {} -> {} failed: {error}",
                    tmp.display(),
                    prepared.display()
                ),
            ))
        })?;
        fsync_dir(&indexes_dir)?;
        let mapped = Arc::new(PagedIndex::open(&prepared)?);
        validate_secondary_run(&mapped)?;
        *stage = format!(
            "publishing secondary index {}.{}",
            frozen_job.key.0, frozen_job.key.1
        );
        let bytes = fs::metadata(&prepared)?.len();
        let (meta, manifest) = {
            let _commit = shared.commit.lock();
            let state = shared.state.read().unwrap();
            let Some(index) = state.secondary.get(&frozen_job.key) else {
                drop(state);
                let _ = fs::remove_file(&prepared);
                continue;
            };
            if !index.frozen_matches(&frozen_job.frozen) {
                drop(state);
                let _ = fs::remove_file(&prepared);
                continue;
            }
            let meta = DerivedRunMeta {
                file,
                level: if index.runs.is_empty() {
                    DERIVED_BASE_LEVEL
                } else {
                    0
                },
                bytes,
                generation,
            };
            let mut metas = index.run_metas();
            metas.push(meta.clone());
            let manifest = DerivedRunManifest::new(
                DerivedRunKind::Secondary,
                &frozen_job.key.0,
                &frozen_job.key.1,
                generation,
                metas,
                [0, 0],
            );
            (meta, manifest)
        };
        *stage = format!(
            "preparing secondary run manifest {}.{}",
            frozen_job.key.0, frozen_job.key.1
        );
        let manifest = manifest.prepare(&sidx_manifest_path(
            &shared.dir,
            &frozen_job.key.0,
            &frozen_job.key.1,
        ))?;
        pause_derived_before_publish_for_test(shared);
        fs::rename(&prepared, &path)?;
        fsync_dir(&indexes_dir)?;
        let manifest_outcome = match publish_background_derived_manifest(shared, manifest) {
            Ok(outcome) => outcome,
            Err(error) => {
                let _ = fs::remove_file(&path);
                return Err(error);
            }
        };
        {
            let _commit = shared.commit.lock();
            let mut state = shared.state.write().unwrap();
            let index = state.secondary.get_mut(&frozen_job.key).ok_or_else(|| {
                Error::Corrupt("secondary index disappeared during background publication".into())
            })?;
            if !index.frozen_matches(&frozen_job.frozen) {
                return Err(Error::Corrupt(
                    "secondary frozen generation changed during background publication".into(),
                ));
            }
            index.runs.push(SecRun {
                meta: meta.clone(),
                index: mapped,
            });
            index.generation = generation;
            index.clear_frozen(&frozen_job.frozen);
        }
        shared
            .secondary_checkpoint_bytes_written
            .fetch_add(meta.bytes, AtomicOrdering::Relaxed);
        if let PublishOutcome::SyncFailed(error) = manifest_outcome {
            return Err(error.into());
        }
    }

    for frozen_job in &job.text {
        *stage = format!(
            "writing text index {}.{}",
            frozen_job.key.0, frozen_job.key.1
        );
        let file = tidx_run_filename(&shared.dir, &frozen_job.key.0, &frozen_job.key.1, 0);
        let path = indexes_dir.join(&file);
        let prepared = path.with_file_name(format!("{file}.derived-pending"));
        let tmp = path.with_file_name(format!("{file}.derived-pending.tmp"));
        if let Err(error) =
            TextIdx::write_frozen_delta_paged(&frozen_job.frozen, &tmp, &temp_dir, budget)
        {
            let _ = fs::remove_file(&tmp);
            return Err(error);
        }
        fs::rename(&tmp, &prepared).map_err(|error| {
            Error::Io(std::io::Error::new(
                error.kind(),
                format!(
                    "publishing frozen text run {} -> {} failed: {error}",
                    tmp.display(),
                    prepared.display()
                ),
            ))
        })?;
        fsync_dir(&indexes_dir)?;
        let mapped = Arc::new(PagedIndex::open(&prepared)?);
        validate_text_run(&mapped)?;
        *stage = format!(
            "publishing text index {}.{}",
            frozen_job.key.0, frozen_job.key.1
        );
        let generation = frozen_job.frozen.generation;
        let bytes = fs::metadata(&prepared)?.len();
        let (meta, manifest) = {
            let _commit = shared.commit.lock();
            let state = shared.state.read().unwrap();
            let Some(index) = state.text.get(&frozen_job.key) else {
                drop(state);
                let _ = fs::remove_file(&prepared);
                continue;
            };
            if !index.frozen_matches(&frozen_job.frozen) {
                drop(state);
                let _ = fs::remove_file(&prepared);
                continue;
            }
            let meta = DerivedRunMeta {
                file,
                level: if index.runs.is_empty() {
                    DERIVED_BASE_LEVEL
                } else {
                    0
                },
                bytes,
                generation,
            };
            let mut metas = index.run_metas();
            metas.push(meta.clone());
            let (doc_count, total_len) = index.frozen_doc_stats();
            let manifest = DerivedRunManifest::new(
                DerivedRunKind::Text,
                &frozen_job.key.0,
                &frozen_job.key.1,
                generation,
                metas,
                [doc_count, total_len],
            );
            (meta, manifest)
        };
        *stage = format!(
            "preparing text run manifest {}.{}",
            frozen_job.key.0, frozen_job.key.1
        );
        let manifest = manifest.prepare(&tidx_manifest_path(
            &shared.dir,
            &frozen_job.key.0,
            &frozen_job.key.1,
        ))?;
        pause_derived_before_publish_for_test(shared);
        fs::rename(&prepared, &path)?;
        fsync_dir(&indexes_dir)?;
        let manifest_outcome = match publish_background_derived_manifest(shared, manifest) {
            Ok(outcome) => outcome,
            Err(error) => {
                let _ = fs::remove_file(&path);
                return Err(error);
            }
        };
        {
            let _commit = shared.commit.lock();
            let mut state = shared.state.write().unwrap();
            let index = state.text.get_mut(&frozen_job.key).ok_or_else(|| {
                Error::Corrupt("text index disappeared during background publication".into())
            })?;
            if !index.frozen_matches(&frozen_job.frozen) {
                return Err(Error::Corrupt(
                    "text frozen generation changed during background publication".into(),
                ));
            }
            index.runs.push(TextRun {
                meta: meta.clone(),
                index: mapped,
            });
            index.generation = generation;
            index.clear_frozen(&frozen_job.frozen);
        }
        shared
            .text_checkpoint_bytes_written
            .fetch_add(meta.bytes, AtomicOrdering::Relaxed);
        if let PublishOutcome::SyncFailed(error) = manifest_outcome {
            return Err(error.into());
        }
    }

    for frozen_job in &job.vector {
        *stage = format!(
            "writing vector index {}.{}",
            frozen_job.table, frozen_job.def.column
        );
        let run = vectors_dir.join(format!("delta-{}.vidx.derived-pending", Ulid::new()));
        frozen_job.frozen.dump_file(
            &run,
            &frozen_job.table,
            &frozen_job.def.column,
            &frozen_job.def,
            frozen_job.generation,
        )?;
        fsync_dir(&vectors_dir)?;
        let file = File::open(&run)?;
        let mmap = unsafe { MmapOptions::new().map(&file) }?;
        let (loaded, version) = VecIdx::load_mmap(
            mmap,
            &frozen_job.table,
            &frozen_job.def.column,
            &frozen_job.def,
        )?;
        if version != frozen_job.generation {
            let _ = fs::remove_file(&run);
            return Err(Error::Corrupt(
                "background vector run has the wrong generation".into(),
            ));
        }
        *stage = format!(
            "publishing vector index {}.{}",
            frozen_job.table, frozen_job.def.column
        );
        pause_derived_before_publish_for_test(shared);
        let _cs = shared.commit.lock();
        let mut state = shared.state.write().unwrap();
        let key = (frozen_job.table.clone(), frozen_job.def.column.clone());
        let published = state
            .vector
            .get_mut(&key)
            .is_some_and(|index| index.publish_frozen_loaded(&frozen_job.frozen, loaded));
        drop(state);
        let _ = fs::remove_file(&run);
        if !published {
            continue;
        }
    }

    drop(job.memory.take());
    let retained = shared.state.read().unwrap().index_delta_memory_bytes();
    shared.memory_governor.set_index_delta_bytes(retained);
    maybe_schedule_secondary_compaction(shared);
    maybe_schedule_text_compaction(shared);
    Ok(())
}

fn consolidate_derived_indexes(shared: &Arc<Shared>) -> Result<()> {
    if shared.opts.read_only {
        return Ok(());
    }
    let idir = shared.dir.join(INDEXES_DIR);
    let vdir = shared.dir.join(VECTORS_DIR);
    fs::create_dir_all(&idir)?;
    fs::create_dir_all(&vdir)?;
    let temp_dir = shared
        .opts
        .memory
        .spill_directory
        .clone()
        .unwrap_or_else(|| idir.join("tmp"));
    let budget = shared.opts.memory.maintenance_pool_bytes;
    let mut st = shared.state.write().unwrap();
    let version = st.committed_version;
    let has_sorted_indexes = !st.secondary.is_empty() || !st.text.is_empty();
    let has_vector_indexes = !st.vector.is_empty();
    let mut secondary_written = 0u64;
    let mut text_written = 0u64;

    let secondary_keys: Vec<_> = st.secondary.keys().cloned().collect();
    for key in secondary_keys {
        let index = st
            .secondary
            .get_mut(&key)
            .expect("secondary key collected above");
        if index.delta.is_empty() && index.removed.is_empty() {
            index.generation = version;
            publish_secondary_manifest(&shared.dir, &key.0, &key.1, version, index)?;
            continue;
        }
        let first = index.runs.is_empty();
        let file = if first {
            sidx_path(&shared.dir, &key.0, &key.1)
                .file_name()
                .and_then(|name| name.to_str())
                .expect("secondary path has utf8 filename")
                .to_owned()
        } else {
            sidx_run_filename(&shared.dir, &key.0, &key.1, 0)
        };
        let path = idir.join(&file);
        let tmp = path.with_file_name(format!("{file}.tmp"));
        let mut writer = ExternalPagedWriter::new(&tmp, &temp_dir, version, budget)?;
        writer.add(SECONDARY_FORMAT_KEY, SECONDARY_FORMAT_VALUE)?;
        for (value, ids) in &index.delta {
            for id in ids {
                writer.add(
                    &secondary_pair_key(value, id),
                    &secondary_operation(version, SECONDARY_ADD),
                )?;
            }
        }
        for (value, ids) in &index.removed {
            for id in ids {
                writer.add(
                    &secondary_pair_key(value, id),
                    &secondary_operation(version, SECONDARY_DELETE),
                )?;
            }
        }
        if let Err(error) = writer.finish() {
            let _ = fs::remove_file(&tmp);
            return Err(error);
        }
        fs::rename(&tmp, &path)?;
        let meta = DerivedRunMeta {
            file,
            level: if first { DERIVED_BASE_LEVEL } else { 0 },
            bytes: fs::metadata(&path)?.len(),
            generation: version,
        };
        secondary_written = secondary_written.saturating_add(meta.bytes);
        let mapped = Arc::new(PagedIndex::open(&path)?);
        validate_secondary_run(&mapped)?;
        index.runs.push(SecRun {
            meta,
            index: mapped,
        });
        index.generation = version;
        index.delta.clear();
        index.removed.clear();
        publish_secondary_manifest(&shared.dir, &key.0, &key.1, version, index)?;
    }

    let text_keys: Vec<_> = st.text.keys().cloned().collect();
    for key in text_keys {
        let index = st.text.get_mut(&key).expect("text key collected above");
        if index.delta_memory_bytes() == 0 {
            index.generation = version;
            publish_text_manifest(&shared.dir, &key.0, &key.1, version, index)?;
            continue;
        }
        let first = index.runs.is_empty();
        let file = if first {
            tidx_path(&shared.dir, &key.0, &key.1)
                .file_name()
                .and_then(|name| name.to_str())
                .expect("text path has utf8 filename")
                .to_owned()
        } else {
            tidx_run_filename(&shared.dir, &key.0, &key.1, 0)
        };
        let path = idir.join(&file);
        let tmp = path.with_file_name(format!("{file}.tmp"));
        if let Err(error) = index.write_delta_paged(&tmp, &temp_dir, version, budget) {
            let _ = fs::remove_file(&tmp);
            return Err(error);
        }
        fs::rename(&tmp, &path)?;
        let meta = DerivedRunMeta {
            file,
            level: if first { DERIVED_BASE_LEVEL } else { 0 },
            bytes: fs::metadata(&path)?.len(),
            generation: version,
        };
        text_written = text_written.saturating_add(meta.bytes);
        let mapped = Arc::new(PagedIndex::open(&path)?);
        validate_text_run(&mapped)?;
        index.runs.push(TextRun {
            meta,
            index: mapped,
        });
        index.freeze_delta(version);
        publish_text_manifest(&shared.dir, &key.0, &key.1, version, index)?;
    }

    // Freeze each mutable HNSW overlay independently. Existing mmap graphs are
    // not rebuilt: searches merge the immutable runs, while canonical
    // segments remain the restart source of truth for ephemeral overlays.
    let vector_defs: Vec<(String, VectorIndexDef)> = st
        .catalog
        .tables
        .iter()
        .flat_map(|table| {
            table
                .vector_indexes
                .iter()
                .cloned()
                .map(|def| (table.name.clone(), def))
                .collect::<Vec<_>>()
        })
        .collect();
    for (table, def) in vector_defs {
        let key = (table.clone(), def.column.clone());
        if st
            .vector
            .get(&key)
            .is_none_or(|index| index.delta_memory_bytes() == 0)
        {
            continue;
        }
        let path = vidx_path(&shared.dir, &table, &def.column);
        let has_base = st
            .vector
            .get(&key)
            .expect("vector key collected above")
            .has_mapped_base();
        let run = if has_base {
            vdir.join(format!("delta-{}.vidx.run", Ulid::new()))
        } else {
            path.with_extension("vidx.tmp")
        };
        let flush = st
            .vector
            .get_mut(&key)
            .expect("vector key collected above")
            .flush_delta_mmap(&run, &table, &def.column, &def, version, has_base);
        if let Err(error) = flush {
            let _ = fs::remove_file(&run);
            return Err(error);
        }
        if !has_base {
            fs::rename(&run, &path)?;
        }
    }
    if has_sorted_indexes {
        fsync_dir(&idir)?;
    }
    if has_vector_indexes {
        fsync_dir(&vdir)?;
    }
    drop(st);
    shared
        .secondary_checkpoint_bytes_written
        .fetch_add(secondary_written, AtomicOrdering::Relaxed);
    shared
        .text_checkpoint_bytes_written
        .fetch_add(text_written, AtomicOrdering::Relaxed);
    maybe_schedule_secondary_compaction(shared);
    maybe_schedule_text_compaction(shared);
    Ok(())
}

/// Rebuild derived indexes after canonical segment rewriting without ever
/// retaining all rebuilt structures at once. Sorted indexes stream directly
/// to paged files; each HNSW graph must fit the maintenance pool, is persisted,
/// and is remapped before the next graph begins.
fn rebuild_derived_indexes_after_rewrite(
    shared: &Arc<Shared>,
    rebuild_secondary: bool,
) -> Result<()> {
    let idir = shared.dir.join(INDEXES_DIR);
    let vdir = shared.dir.join(VECTORS_DIR);
    fs::create_dir_all(&idir)?;
    fs::create_dir_all(&vdir)?;
    let temp_dir = shared
        .opts
        .memory
        .spill_directory
        .clone()
        .unwrap_or_else(|| idir.join("tmp"));
    let budget = shared.opts.memory.maintenance_pool_bytes;
    let mut st = shared.state.write().unwrap();
    let version = st.committed_version;

    if rebuild_secondary {
        let definitions: Vec<_> = st
            .catalog
            .tables
            .iter()
            .flat_map(|table| {
                table
                    .indexes
                    .iter()
                    .map(|def| (table.name.clone(), def.column.clone()))
                    .collect::<Vec<_>>()
            })
            .collect();
        let mut rebuilt = HashMap::new();
        for (table, column) in definitions {
            let path = sidx_path(&shared.dir, &table, &column);
            let tmp = path.with_extension("sidx.tmp");
            write_secondary_from_canonical(
                &tmp,
                &temp_dir,
                version,
                budget,
                &st.blobs,
                &table,
                &column,
                &st.index,
                &st.readers,
            )?;
            fs::rename(&tmp, &path)?;
            let meta = DerivedRunMeta {
                file: path
                    .file_name()
                    .and_then(|name| name.to_str())
                    .expect("secondary path has utf8 filename")
                    .to_owned(),
                level: DERIVED_BASE_LEVEL,
                bytes: fs::metadata(&path)?.len(),
                generation: version,
            };
            let index = SecIdx::paged_runs(
                version,
                vec![SecRun {
                    meta,
                    index: Arc::new(PagedIndex::open(&path)?),
                }],
            )?;
            publish_secondary_manifest(&shared.dir, &table, &column, version, &index)?;
            rebuilt.insert((table, column), index);
        }
        st.secondary = rebuilt;
    }

    let text_definitions: Vec<_> = st
        .catalog
        .tables
        .iter()
        .flat_map(|table| {
            table
                .text_indexes
                .iter()
                .map(|def| (table.name.clone(), def.column.clone()))
                .collect::<Vec<_>>()
        })
        .collect();
    let mut rebuilt_text = HashMap::new();
    for (table, column) in text_definitions {
        let path = tidx_path(&shared.dir, &table, &column);
        let tmp = path.with_extension("tidx.tmp");
        let (doc_count, total_len) = write_text_from_canonical(
            &tmp,
            &temp_dir,
            version,
            budget,
            &st.blobs,
            &table,
            &column,
            &st.index,
            &st.readers,
        )?;
        fs::rename(&tmp, &path)?;
        let meta = DerivedRunMeta {
            file: path
                .file_name()
                .and_then(|name| name.to_str())
                .expect("text path has utf8 filename")
                .to_owned(),
            level: DERIVED_BASE_LEVEL,
            bytes: fs::metadata(&path)?.len(),
            generation: version,
        };
        let index = TextIdx::paged_runs(
            version,
            vec![TextRun {
                meta,
                index: Arc::new(PagedIndex::open(&path)?),
            }],
            doc_count,
            total_len,
        )?;
        publish_text_manifest(&shared.dir, &table, &column, version, &index)?;
        rebuilt_text.insert((table, column), index);
    }
    st.text = rebuilt_text;

    let vector_definitions: Vec<_> = st
        .catalog
        .tables
        .iter()
        .flat_map(|table| {
            table
                .vector_indexes
                .iter()
                .cloned()
                .map(|def| (table.name.clone(), def))
                .collect::<Vec<_>>()
        })
        .collect();
    let mut rebuilt_vector = HashMap::new();
    for (table, def) in vector_definitions {
        let resident =
            build_one_vector_index(&st.blobs, &table, &def, &st.index, &st.readers, budget)?;
        let path = vidx_path(&shared.dir, &table, &def.column);
        let tmp = path.with_extension("vidx.tmp");
        resident.dump_file(&tmp, &table, &def.column, &def, version)?;
        fs::rename(&tmp, &path)?;
        let file = File::open(&path)?;
        let mmap = unsafe { MmapOptions::new().map(&file) }?;
        let (mapped, dump_version) = VecIdx::load_mmap(mmap, &table, &def.column, &def)?;
        if dump_version != version {
            return Err(Error::Corrupt(
                "rewritten vector index has the wrong generation".into(),
            ));
        }
        rebuilt_vector.insert((table, def.column.clone()), mapped);
    }
    st.vector = rebuilt_vector;
    fsync_dir(&idir)?;
    fsync_dir(&vdir)?;
    Ok(())
}

fn sidx_path(dir: &Path, table: &str, column: &str) -> PathBuf {
    let key = format!("{table}\u{0}{column}");
    dir.join(INDEXES_DIR)
        .join(format!("{:08x}.sidx", crc32fast::hash(key.as_bytes())))
}

fn sidx_manifest_path(dir: &Path, table: &str, column: &str) -> PathBuf {
    sidx_path(dir, table, column).with_extension("sidx.runs")
}

fn sidx_run_filename(dir: &Path, table: &str, column: &str, level: u8) -> String {
    let stem = sidx_path(dir, table, column)
        .file_stem()
        .and_then(|stem| stem.to_str())
        .expect("secondary path has utf8 stem")
        .to_owned();
    format!("{stem}-L{level}-{}.sidx.run", Ulid::new())
}

fn load_secondary_runs(dir: &Path, table: &str, column: &str, generation: u64) -> Result<SecIdx> {
    let manifest = DerivedRunManifest::load(
        &sidx_manifest_path(dir, table, column),
        DerivedRunKind::Secondary,
        table,
        column,
        generation,
    )?;
    let mut seen = HashSet::new();
    let mut runs = Vec::with_capacity(manifest.runs.len());
    for meta in manifest.runs {
        if !seen.insert(meta.file.clone()) {
            return Err(Error::Corrupt(
                "secondary run manifest: duplicate filename".into(),
            ));
        }
        let path = dir.join(INDEXES_DIR).join(&meta.file);
        if fs::metadata(&path)?.len() != meta.bytes {
            return Err(Error::Corrupt(format!(
                "secondary run {} has the wrong length",
                meta.file
            )));
        }
        let index = PagedIndex::open(&path)?;
        if index.dump_version() != meta.generation {
            return Err(Error::Corrupt(format!(
                "secondary run {} has the wrong generation",
                meta.file
            )));
        }
        runs.push(SecRun {
            meta,
            index: Arc::new(index),
        });
    }
    SecIdx::paged_runs(generation, runs)
}

fn publish_secondary_manifest(
    dir: &Path,
    table: &str,
    column: &str,
    generation: u64,
    index: &SecIdx,
) -> Result<()> {
    DerivedRunManifest::new(
        DerivedRunKind::Secondary,
        table,
        column,
        generation,
        index.run_metas(),
        [0, 0],
    )
    .publish(&sidx_manifest_path(dir, table, column))
}

/// Load immutable secondary runs only when they describe exactly the
/// canonical version being opened. A stale or damaged run is disposable and
/// is rebuilt from canonical records instead of being patched speculatively.
#[allow(clippy::too_many_arguments)]
fn load_or_build_secondary_indexes(
    dir: &Path,
    blobs: &Path,
    catalog: &Catalog,
    index: &PrimaryIdx,
    readers: &SegmentReaders,
    committed_version: u64,
    read_only: bool,
    memory: &MemoryOptions,
) -> Result<HashMap<(String, String), SecIdx>> {
    let mut out = HashMap::new();
    for table in &catalog.tables {
        for def in &table.indexes {
            let key = (table.name.clone(), def.column.clone());
            match load_secondary_runs(dir, &table.name, &def.column, committed_version) {
                Ok(index) => {
                    out.insert(key, index);
                }
                _ if read_only => {
                    // Correctness is preserved by the primary-index scan
                    // fallback. A read-only open never repairs derived data.
                }
                _ => {
                    let path = sidx_path(dir, &table.name, &def.column);
                    let tmp = path.with_extension("sidx.tmp");
                    let temp_dir = memory
                        .spill_directory
                        .clone()
                        .unwrap_or_else(|| dir.join(INDEXES_DIR).join("tmp"));
                    write_secondary_from_canonical(
                        &tmp,
                        &temp_dir,
                        committed_version,
                        memory.maintenance_pool_bytes,
                        blobs,
                        &table.name,
                        &def.column,
                        index,
                        readers,
                    )?;
                    fs::rename(&tmp, &path)?;
                    fsync_dir(&dir.join(INDEXES_DIR))?;
                    let meta = DerivedRunMeta {
                        file: path
                            .file_name()
                            .and_then(|name| name.to_str())
                            .expect("secondary path has utf8 filename")
                            .to_owned(),
                        level: DERIVED_BASE_LEVEL,
                        bytes: fs::metadata(&path)?.len(),
                        generation: committed_version,
                    };
                    let index = SecIdx::paged_runs(
                        committed_version,
                        vec![SecRun {
                            meta,
                            index: Arc::new(PagedIndex::open(&path)?),
                        }],
                    )?;
                    publish_secondary_manifest(
                        dir,
                        &table.name,
                        &def.column,
                        committed_version,
                        &index,
                    )?;
                    out.insert(key, index);
                }
            }
        }
    }
    Ok(out)
}

#[allow(clippy::too_many_arguments)]
fn write_secondary_from_canonical(
    target: &Path,
    temp_dir: &Path,
    dump_version: u64,
    budget: usize,
    blobs: &Path,
    table: &str,
    column: &str,
    index: &PrimaryIdx,
    readers: &SegmentReaders,
) -> Result<()> {
    let mut writer = ExternalPagedWriter::new(target, temp_dir, dump_version, budget)?;
    writer.add(SECONDARY_FORMAT_KEY, SECONDARY_FORMAT_VALUE)?;
    index.visit_table(table, None, |id, versions| {
        let Some(last) = versions.last() else {
            return Ok(true);
        };
        if last.is_tombstone() {
            return Ok(true);
        }
        let record = read_record_kind(blobs, readers, &last.kind)?;
        if let Some(value) = record.get(column).filter(|value| !value.is_null()) {
            writer.add(
                &secondary_pair_key(&index_key(value), id),
                &secondary_operation(dump_version, SECONDARY_ADD),
            )?;
        }
        Ok(true)
    })?;
    writer.finish()
}

fn cleanup_orphan_sidx(dir: &Path, catalog: &Catalog) {
    let mut expected = HashSet::new();
    for table in &catalog.tables {
        for def in &table.indexes {
            let base = sidx_path(dir, &table.name, &def.column);
            let manifest = sidx_manifest_path(dir, &table.name, &def.column);
            expected.insert(base);
            expected.insert(manifest.clone());
            for file in DerivedRunManifest::referenced_files(
                &manifest,
                DerivedRunKind::Secondary,
                &table.name,
                &def.column,
            ) {
                expected.insert(dir.join(INDEXES_DIR).join(file));
            }
        }
    }
    if let Ok(entries) = fs::read_dir(dir.join(INDEXES_DIR)) {
        for entry in entries.flatten() {
            let path = entry.path();
            let is_file = path
                .file_name()
                .and_then(|name| name.to_str())
                .is_some_and(|name| {
                    name.ends_with(".sidx")
                        || name.ends_with(".sidx.runs")
                        || name.ends_with(".sidx.run")
                        || name.ends_with(".sidx.tmp")
                        || name.ends_with(".sidx.run.tmp")
                        || name.ends_with(".sidx.runs.tmp")
                });
            if is_file && !expected.contains(&path) {
                let _ = fs::remove_file(path);
            }
        }
    }
}

fn build_one_vector_index(
    blobs: &Path,
    table: &str,
    def: &VectorIndexDef,
    index: &PrimaryIdx,
    readers: &SegmentReaders,
    budget: usize,
) -> Result<VecIdx> {
    let mut vidx = VecIdx::new(def.clone());
    index.visit_table(table, None, |id, versions| {
        let Some(last) = versions.last() else {
            return Ok(true);
        };
        if last.is_tombstone() {
            return Ok(true);
        }
        let rec = read_record_kind(blobs, readers, &last.kind)?;
        if let Some(Value::Vector(v)) = rec.get(&def.column) {
            vidx.insert(id, v);
            if vidx.delta_memory_bytes() > budget {
                return Err(Error::MemoryLimit(format!(
                    "building vector index {table}.{} exceeds the {budget}-byte maintenance pool; raise memory.maintenance_pool_bytes",
                    def.column
                )));
            }
        }
        Ok(true)
    })?;
    Ok(vidx)
}

fn vidx_path(dir: &Path, table: &str, column: &str) -> PathBuf {
    let key = format!("{table}\u{0}{column}");
    dir.join(VECTORS_DIR)
        .join(format!("{:08x}.vidx", crc32fast::hash(key.as_bytes())))
}

/// On open: load each persisted graph if valid, catching up incrementally
/// with everything committed after the dump; otherwise rebuild from scratch.
#[allow(clippy::too_many_arguments)]
fn load_or_build_vector_indexes(
    dir: &Path,
    blobs: &Path,
    catalog: &Catalog,
    index: &PrimaryIdx,
    readers: &SegmentReaders,
    committed_version: u64,
    read_only: bool,
    memory: &MemoryOptions,
) -> Result<HashMap<(String, String), VecIdx>> {
    let mut out: HashMap<(String, String), VecIdx> = HashMap::new();
    for table in &catalog.tables {
        for def in &table.vector_indexes {
            let loaded = File::open(vidx_path(dir, &table.name, &def.column))
                .ok()
                .and_then(|file| {
                    // SAFETY: the dump is immutable while this process owns the
                    // database lock; writers publish a different temp inode and
                    // rename it only after the map has been dropped.
                    unsafe { MmapOptions::new().map(&file) }.ok()
                })
                .and_then(|bytes| {
                    let is_mmap_format = bytes
                        .get(8..12)
                        .is_some_and(|format| format == 4u32.to_le_bytes());
                    if is_mmap_format {
                        VecIdx::load_mmap(bytes, &table.name, &def.column, def).ok()
                    } else {
                        VecIdx::load_bytes(&bytes, &table.name, &def.column, def).ok()
                    }
                });
            let vidx = match loaded {
                // A dump "from the future" can exist after a manifest.prev
                // rollback; it must be discarded, not caught up.
                Some((mut vidx, dump_version)) if dump_version <= committed_version => {
                    catch_up_vector_index(
                        blobs,
                        &mut vidx,
                        dump_version,
                        &table.name,
                        def,
                        index,
                        readers,
                        dir,
                        memory.index_delta_pool_bytes,
                    )?;
                    vidx
                }
                _ => {
                    let resident = build_one_vector_index(
                        blobs,
                        &table.name,
                        def,
                        index,
                        readers,
                        memory.maintenance_pool_bytes,
                    )?;
                    if read_only || resident.total_len() == 0 {
                        resident
                    } else {
                        let path = vidx_path(dir, &table.name, &def.column);
                        let tmp = path.with_extension("vidx.tmp");
                        if let Err(error) = resident.dump_file(
                            &tmp,
                            &table.name,
                            &def.column,
                            def,
                            committed_version,
                        ) {
                            let _ = fs::remove_file(&tmp);
                            return Err(error);
                        }
                        fs::rename(&tmp, &path)?;
                        fsync_dir(&dir.join(VECTORS_DIR))?;
                        let file = File::open(&path)?;
                        let mmap = unsafe { MmapOptions::new().map(&file) }?;
                        let (mapped, dump_version) =
                            VecIdx::load_mmap(mmap, &table.name, &def.column, def)?;
                        if dump_version != committed_version {
                            return Err(Error::Corrupt(
                                "rebuilt vector index has the wrong generation".into(),
                            ));
                        }
                        mapped
                    }
                }
            };
            let retained = out
                .values()
                .map(VecIdx::delta_memory_bytes)
                .sum::<usize>()
                .saturating_add(vidx.delta_memory_bytes());
            if retained > memory.index_delta_pool_bytes {
                return Err(Error::MemoryLimit(format!(
                    "vector index deltas need an estimated {retained} bytes, exceeding memory.index_delta_pool_bytes ({})",
                    memory.index_delta_pool_bytes
                )));
            }
            out.insert((table.name.clone(), def.column.clone()), vidx);
        }
    }
    Ok(out)
}

/// Re-apply everything committed after the dump: changed/deleted records,
/// and dumped ids whose canonical data was compacted away since.
#[allow(clippy::too_many_arguments)]
fn catch_up_vector_index(
    blobs: &Path,
    vidx: &mut VecIdx,
    dump_version: u64,
    table: &str,
    def: &VectorIndexDef,
    index: &PrimaryIdx,
    readers: &SegmentReaders,
    dir: &Path,
    budget: usize,
) -> Result<()> {
    index.visit_table(table, None, |id, versions| {
        let Some(last) = versions.last() else {
            return Ok(true);
        };
        if last.version <= dump_version {
            return Ok(true);
        }
        if last.is_tombstone() {
            vidx.remove(id);
            return Ok(true);
        }
        let rec = read_record_kind(blobs, readers, &last.kind)?;
        match rec.get(&def.column) {
            Some(Value::Vector(v)) => vidx.insert(id, v),
            _ => vidx.remove(id),
        }
        if vidx.delta_memory_bytes() >= budget {
            let run = dir
                .join(VECTORS_DIR)
                .join(format!("catch-up-{}.vidx.run", Ulid::new()));
            vidx.flush_delta_mmap(&run, table, &def.column, def, last.version, true)?;
        }
        Ok(true)
    })?;
    for id in vidx.ids() {
        let alive = index
            .latest(table, &id)?
            .map(|entry| !entry.is_tombstone())
            .unwrap_or(false);
        if !alive {
            vidx.remove(&id);
        }
    }
    Ok(())
}

/// Remove vector dumps that no longer correspond to a cataloged index.
fn cleanup_orphan_vidx(dir: &Path, catalog: &Catalog) {
    let expected: HashSet<PathBuf> = catalog
        .tables
        .iter()
        .flat_map(|t| {
            t.vector_indexes
                .iter()
                .map(|d| vidx_path(dir, &t.name, &d.column))
        })
        .collect();
    if let Ok(entries) = fs::read_dir(dir.join(VECTORS_DIR)) {
        for entry in entries.flatten() {
            let path = entry.path();
            let known = expected.contains(&path);
            let is_vidx = path.extension().is_some_and(|e| e == "vidx");
            if !known && (is_vidx || path.extension().is_some_and(|e| e == "tmp")) {
                let _ = fs::remove_file(&path);
            }
        }
    }
}

fn tidx_path(dir: &Path, table: &str, column: &str) -> PathBuf {
    let key = format!("{table}\u{0}{column}");
    dir.join(INDEXES_DIR)
        .join(format!("{:08x}.tidx", crc32fast::hash(key.as_bytes())))
}

fn tidx_manifest_path(dir: &Path, table: &str, column: &str) -> PathBuf {
    tidx_path(dir, table, column).with_extension("tidx.runs")
}

fn tidx_run_filename(dir: &Path, table: &str, column: &str, level: u8) -> String {
    let stem = tidx_path(dir, table, column)
        .file_stem()
        .and_then(|stem| stem.to_str())
        .expect("text path has utf8 stem")
        .to_owned();
    format!("{stem}-L{level}-{}.tidx.run", Ulid::new())
}

fn load_text_runs(dir: &Path, table: &str, column: &str, generation: u64) -> Result<TextIdx> {
    let manifest = DerivedRunManifest::load(
        &tidx_manifest_path(dir, table, column),
        DerivedRunKind::Text,
        table,
        column,
        generation,
    )?;
    let mut seen = HashSet::new();
    let mut runs = Vec::with_capacity(manifest.runs.len());
    for meta in manifest.runs {
        if !seen.insert(meta.file.clone()) {
            return Err(Error::Corrupt(
                "text run manifest: duplicate filename".into(),
            ));
        }
        let path = dir.join(INDEXES_DIR).join(&meta.file);
        if fs::metadata(&path)?.len() != meta.bytes {
            return Err(Error::Corrupt(format!(
                "text run {} has the wrong length",
                meta.file
            )));
        }
        let index = PagedIndex::open(&path)?;
        if index.dump_version() != meta.generation {
            return Err(Error::Corrupt(format!(
                "text run {} has the wrong generation",
                meta.file
            )));
        }
        runs.push(TextRun {
            meta,
            index: Arc::new(index),
        });
    }
    TextIdx::paged_runs(generation, runs, manifest.aux[0], manifest.aux[1])
}

fn publish_text_manifest(
    dir: &Path,
    table: &str,
    column: &str,
    generation: u64,
    index: &TextIdx,
) -> Result<()> {
    let (doc_count, total_len) = index.doc_stats();
    DerivedRunManifest::new(
        DerivedRunKind::Text,
        table,
        column,
        generation,
        index.run_metas(),
        [doc_count, total_len],
    )
    .publish(&tidx_manifest_path(dir, table, column))
}

#[allow(clippy::too_many_arguments)]
fn load_or_build_text_indexes(
    dir: &Path,
    blobs: &Path,
    catalog: &Catalog,
    index: &PrimaryIdx,
    readers: &SegmentReaders,
    committed_version: u64,
    read_only: bool,
    memory: &MemoryOptions,
) -> Result<HashMap<(String, String), TextIdx>> {
    let mut out = HashMap::new();
    for table in &catalog.tables {
        for def in &table.text_indexes {
            let key = (table.name.clone(), def.column.clone());
            if let Ok(index) = load_text_runs(dir, &table.name, &def.column, committed_version) {
                out.insert(key, index);
                continue;
            }
            if read_only {
                out.insert(
                    key,
                    build_one_text_index(
                        blobs,
                        &table.name,
                        &def.column,
                        index,
                        readers,
                        memory.index_delta_pool_bytes,
                    )?,
                );
                continue;
            }
            let path = tidx_path(dir, &table.name, &def.column);
            let tmp = path.with_extension("tidx.tmp");
            let temp_dir = memory
                .spill_directory
                .clone()
                .unwrap_or_else(|| dir.join(INDEXES_DIR).join("tmp"));
            let result = write_text_from_canonical(
                &tmp,
                &temp_dir,
                committed_version,
                memory.maintenance_pool_bytes,
                blobs,
                &table.name,
                &def.column,
                index,
                readers,
            );
            let (doc_count, total_len) = match result {
                Ok(stats) => stats,
                Err(error) => {
                    let _ = fs::remove_file(&tmp);
                    return Err(error);
                }
            };
            fs::rename(&tmp, &path)?;
            fsync_dir(&dir.join(INDEXES_DIR))?;
            let meta = DerivedRunMeta {
                file: path
                    .file_name()
                    .and_then(|name| name.to_str())
                    .expect("text path has utf8 filename")
                    .to_owned(),
                level: DERIVED_BASE_LEVEL,
                bytes: fs::metadata(&path)?.len(),
                generation: committed_version,
            };
            let text = TextIdx::paged_runs(
                committed_version,
                vec![TextRun {
                    meta,
                    index: Arc::new(PagedIndex::open(&path)?),
                }],
                doc_count,
                total_len,
            )?;
            publish_text_manifest(dir, &table.name, &def.column, committed_version, &text)?;
            out.insert(key, text);
        }
    }
    Ok(out)
}

fn build_one_text_index(
    blobs: &Path,
    table: &str,
    column: &str,
    index: &PrimaryIdx,
    readers: &SegmentReaders,
    budget: usize,
) -> Result<TextIdx> {
    let mut out = TextIdx::new();
    index.visit_table(table, None, |id, versions| {
        let Some(last) = versions.last() else {
            return Ok(true);
        };
        if last.is_tombstone() {
            return Ok(true);
        }
        let record = read_record_kind(blobs, readers, &last.kind)?;
        if let Some(Value::Text(text)) = record.get(column) {
            out.add(id, text);
            if out.delta_memory_bytes() > budget {
                return Err(Error::MemoryLimit(format!(
                    "read-only text-index recovery for {table}.{column} exceeds memory.index_delta_pool_bytes ({budget}); open writable once to rebuild its mmap index"
                )));
            }
        }
        Ok(true)
    })?;
    Ok(out)
}

#[allow(clippy::too_many_arguments)]
fn write_text_from_canonical(
    target: &Path,
    temp_dir: &Path,
    dump_version: u64,
    budget: usize,
    blobs: &Path,
    table: &str,
    column: &str,
    index: &PrimaryIdx,
    readers: &SegmentReaders,
) -> Result<(u64, u64)> {
    let mut writer = ExternalPagedWriter::new(target, temp_dir, dump_version, budget)?;
    TextIdx::write_format(&mut writer)?;
    let mut doc_count = 0u64;
    let mut total_len = 0u64;
    index.visit_table(table, None, |id, versions| {
        let Some(last) = versions.last() else {
            return Ok(true);
        };
        if last.is_tombstone() {
            return Ok(true);
        }
        let record = read_record_kind(blobs, readers, &last.kind)?;
        if let Some(Value::Text(text)) = record.get(column) {
            if let Some(len) = TextIdx::write_document(&mut writer, id, text, dump_version)? {
                doc_count += 1;
                total_len += len as u64;
            }
        }
        Ok(true)
    })?;
    writer.finish()?;
    Ok((doc_count, total_len))
}

fn cleanup_orphan_tidx(dir: &Path, catalog: &Catalog) {
    let mut expected = HashSet::new();
    for table in &catalog.tables {
        for def in &table.text_indexes {
            let base = tidx_path(dir, &table.name, &def.column);
            let manifest = tidx_manifest_path(dir, &table.name, &def.column);
            expected.insert(base);
            expected.insert(manifest.clone());
            for file in DerivedRunManifest::referenced_files(
                &manifest,
                DerivedRunKind::Text,
                &table.name,
                &def.column,
            ) {
                expected.insert(dir.join(INDEXES_DIR).join(file));
            }
        }
    }
    if let Ok(entries) = fs::read_dir(dir.join(INDEXES_DIR)) {
        for entry in entries.flatten() {
            let path = entry.path();
            let is_file = path
                .file_name()
                .and_then(|name| name.to_str())
                .is_some_and(|name| {
                    name.ends_with(".tidx")
                        || name.ends_with(".tidx.runs")
                        || name.ends_with(".tidx.run")
                        || name.ends_with(".tidx.tmp")
                        || name.ends_with(".tidx.run.tmp")
                        || name.ends_with(".tidx.runs.tmp")
                });
            if is_file && !expected.contains(&path) {
                let _ = fs::remove_file(path);
            }
        }
    }
}

fn payload_bytes(readers: &SegmentReaders, kind: &VKind) -> Result<Option<Vec<u8>>> {
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

/// Legacy logical scan used by tolerant read-only opens. Normal read-write
/// databases use `find_eq_streaming`, which walks segment files sequentially
/// and only materializes matching records.
fn find_eq_via_primary_index(
    st: &State,
    table: &str,
    column: &str,
    value: &Value,
    out: &mut Vec<(String, Record)>,
) -> Result<()> {
    let epoch = st
        .catalog
        .table(table)
        .ok_or_else(|| Error::TableNotFound(table.into()))?
        .epoch;
    st.index.visit_table(table, None, |id, versions| {
        let Some(last) = versions.iter().rev().find(|entry| entry.version > epoch) else {
            return Ok(true);
        };
        if last.is_tombstone() {
            return Ok(true);
        }
        let rec = read_record_kind(&st.blobs, &st.readers, &last.kind)?;
        if rec.get(column) == Some(value) {
            let mut rec = rec;
            rec.entry(ID_COLUMN.into())
                .or_insert_with(|| Value::Text(id.to_owned()));
            out.push((id.to_owned(), rec));
        }
        Ok(true)
    })?;
    Ok(())
}

/// Equality scan for an unindexed column. Immutable segments are visited in
/// file order through a buffered reader. For every latest visible record we
/// decode only the predicate column; a full Record is built only on a match.
/// Latest MemPut records are evaluated from their already-encoded payloads.
fn find_eq_streaming(
    shared: &Shared,
    st: &State,
    table: &str,
    column: &str,
    value: &Value,
    out: &mut Vec<(String, Record)>,
) -> Result<()> {
    let ordinal = st.catalog.table(table).and_then(|schema| {
        schema
            .columns
            .iter()
            .position(|candidate| candidate.name == column)
    });
    let predicate = EncodedEqPredicate {
        column,
        ordinal,
        value,
    };
    for meta in &st.segments {
        scan_segment_for_eq(shared, st, meta, table, &predicate, out)?;
    }

    // Segment entries have already been visited above. Only the bounded
    // resident delta can contain committed payloads that are not in a segment;
    // walking `visit_table` here would pointlessly merge every immutable
    // primary run and double-scan the complete database.
    let mut seen_active = HashSet::new();
    let resident_tables = [
        st.index.delta.get(table),
        st.index
            .frozen
            .as_ref()
            .and_then(|frozen| frozen.delta.get(table)),
    ];
    for (position, ids) in resident_tables.into_iter().enumerate() {
        let Some(ids) = ids else { continue };
        for (id, versions) in ids {
            if position == 0 {
                seen_active.insert(id.as_str());
            } else if seen_active.contains(id.as_str()) {
                continue;
            }
            let Some(VersionEntry {
                kind: VKind::MemPut(payload),
                ..
            }) = versions.iter().rev().find(|entry| {
                st.catalog
                    .table(table)
                    .is_some_and(|schema| entry.version > schema.epoch)
            })
            else {
                continue;
            };
            if encoded_record_column_eq(payload, &predicate, &st.blobs)? {
                let mut rec = decode_record(payload, Some(&st.blobs))?;
                rec.entry(ID_COLUMN.into())
                    .or_insert_with(|| Value::Text(id.to_owned()));
                out.push((id.to_owned(), rec));
            }
        }
    }
    Ok(())
}

struct EncodedEqPredicate<'a> {
    column: &'a str,
    ordinal: Option<usize>,
    value: &'a Value,
}

fn scan_segment_for_eq(
    shared: &Shared,
    st: &State,
    meta: &SegmentMeta,
    wanted_table: &str,
    predicate: &EncodedEqPredicate<'_>,
    out: &mut Vec<(String, Record)>,
) -> Result<()> {
    let table_epoch = st
        .catalog
        .table(wanted_table)
        .ok_or_else(|| Error::TableNotFound(wanted_table.into()))?
        .epoch;
    let path = shared
        .dir
        .join(SEGMENTS_DIR)
        .join(segment_file_name(meta.id));
    let file = File::open(path)?;
    // SAFETY: canonical segment files are immutable after manifest
    // publication. Compaction writes a new inode and only removes this one
    // after snapshots/readers can no longer reference it.
    let mapped = unsafe { MmapOptions::new().map(&file) }?;
    let _ = mapped.advise(Advice::Sequential);
    let segment_len = usize::try_from(meta.len)
        .map_err(|_| Error::Corrupt("segment length exceeds address space".into()))?;
    let data = mapped
        .get(..segment_len)
        .ok_or_else(|| Error::Corrupt(format!("segment {} is truncated", meta.id)))?;
    let mut pos = 0usize;

    while pos < data.len() {
        let kind = read_u8(data, &mut pos)?;
        if kind != KIND_PUT && kind != KIND_TOMBSTONE {
            return Err(Error::Corrupt(format!(
                "segment {}: unknown entry kind {kind}",
                meta.id
            )));
        }
        let version = read_u64(data, &mut pos)?;

        let table_len = read_u16(data, &mut pos)? as usize;
        let table_bytes = take_segment_slice(data, &mut pos, table_len)?;

        let id_len = read_u16(data, &mut pos)? as usize;
        let id_bytes = take_segment_slice(data, &mut pos, id_len)?;

        let payload_len = read_u32(data, &mut pos)? as usize;
        if kind == KIND_TOMBSTONE && payload_len != 0 {
            return Err(Error::Corrupt("tombstone with payload".into()));
        }
        let payload = take_segment_slice(data, &mut pos, payload_len)?;

        let current = kind == KIND_PUT
            && table_bytes == wanted_table.as_bytes()
            && version > table_epoch
            && (!st.superseded_segments.contains(&meta.id) || {
                let id = std::str::from_utf8(id_bytes)
                    .map_err(|_| Error::Corrupt("invalid utf8 in segment record id".into()))?;
                latest_is_segment_put(st, wanted_table, id, version, meta.id)
            });
        if current && encoded_record_column_eq(payload, predicate, &st.blobs)? {
            let id = std::str::from_utf8(id_bytes)
                .map_err(|_| Error::Corrupt("invalid utf8 in segment record id".into()))?;
            let mut rec = decode_record(payload, Some(&st.blobs))?;
            rec.entry(ID_COLUMN.into())
                .or_insert_with(|| Value::Text(id.to_owned()));
            out.push((id.to_owned(), rec));
        }

        // Segment CRCs were validated when the database was opened (and new
        // segments are produced under the commit lock). Consume the checksum
        // here without hashing every payload a second time.
        let _crc = read_u32(data, &mut pos)?;
    }
    Ok(())
}

fn take_segment_slice<'a>(data: &'a [u8], pos: &mut usize, len: usize) -> Result<&'a [u8]> {
    let end = pos
        .checked_add(len)
        .filter(|end| *end <= data.len())
        .ok_or_else(|| Error::Corrupt("truncated segment entry".into()))?;
    let value = &data[*pos..end];
    *pos = end;
    Ok(value)
}

fn latest_is_segment_put(st: &State, table: &str, id: &str, version: u64, segment: u32) -> bool {
    st.latest_owned(table, id)
        .ok()
        .flatten()
        .is_some_and(|latest| {
            latest.version == version
                && matches!(latest.kind, VKind::SegPut { segment: s, .. } if s == segment)
        })
}

fn encoded_record_column_eq(
    payload: &[u8],
    predicate: &EncodedEqPredicate<'_>,
    blobs: &Path,
) -> Result<bool> {
    if let Some(ordinal) = predicate.ordinal {
        let mut pos = 0usize;
        let count = read_u16(payload, &mut pos)? as usize;
        if ordinal < count {
            for current in 0..=ordinal {
                let name_len = read_u16(payload, &mut pos)? as usize;
                let end = pos
                    .checked_add(name_len)
                    .filter(|end| *end <= payload.len())
                    .ok_or_else(|| Error::Corrupt("unexpected end of record".into()))?;
                let name = &payload[pos..end];
                pos = end;
                if current == ordinal {
                    if name == predicate.column.as_bytes() {
                        return encoded_value_eq(payload, &mut pos, predicate.value, Some(blobs));
                    }
                    break;
                }
                skip_value(payload, &mut pos, None)?;
            }
        }
    }

    // Schemas can evolve while older payloads remain visible. If the ordinal
    // is absent or the encoded name does not agree, retain the name-based path
    // for correctness instead of assuming every record has the latest layout.
    let mut pos = 0usize;
    let count = read_u16(payload, &mut pos)? as usize;
    for _ in 0..count {
        let name_len = read_u16(payload, &mut pos)? as usize;
        let end = pos
            .checked_add(name_len)
            .filter(|end| *end <= payload.len())
            .ok_or_else(|| Error::Corrupt("unexpected end of record".into()))?;
        let name = std::str::from_utf8(&payload[pos..end])
            .map_err(|_| Error::Corrupt("invalid utf8 in column name".into()))?;
        pos = end;
        if name == predicate.column {
            return encoded_value_eq(payload, &mut pos, predicate.value, Some(blobs));
        }
        skip_value(payload, &mut pos, None)?;
    }
    Ok(false)
}

fn read_record_kind(blobs: &Path, readers: &SegmentReaders, kind: &VKind) -> Result<Record> {
    match kind {
        VKind::MemPut(p) => decode_record(p, Some(blobs)),
        VKind::SegPut { .. } => {
            let bytes = payload_bytes(readers, kind)?.expect("put has payload");
            decode_record(&bytes, Some(blobs))
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
    if let Value::Vector(v) = value {
        if v.iter().any(|component| !component.is_finite()) {
            return Err(Error::SchemaViolation(format!(
                "column '{}' requires finite float32 vector components",
                col.name
            )));
        }
        if let Some(dim) = col.dim.filter(|dim| v.len() != *dim) {
            return Err(Error::SchemaViolation(format!(
                "column '{}' expects vector<float32, {dim}>, got dimension {}",
                col.name,
                v.len()
            )));
        }
    }
    if let Value::Time(micros) = value {
        if !(0..86_400_000_000).contains(micros) {
            return Err(Error::SchemaViolation(format!(
                "column '{}' requires a time within one day",
                col.name
            )));
        }
    }
    if let Value::Text(text) = value {
        if let Some(max_length) = col.max_length {
            let actual = text.chars().count();
            if actual > max_length {
                return Err(Error::SchemaViolation(format!(
                    "column '{}' allows at most {max_length} characters, got {actual}",
                    col.name
                )));
            }
        }
        if let Some(values) = &col.enum_values {
            if !values.contains(text) {
                return Err(Error::SchemaViolation(format!(
                    "column '{}' does not allow enum value '{text}'",
                    col.name
                )));
            }
        }
    }
    Ok(())
}

/// Validate an insert against the schema, normalizing the caller's map in
/// place: unknown columns rejected, missing columns become Null (or error
/// when NOT NULL). No rebuild — the caller's allocations are reused.
fn normalize_record(schema: &TableSchema, mut record: Record) -> Result<Record> {
    // In the traditional schema `id` is only a transport field for the
    // physical ULID and is not encoded in the payload.  A declared `id`
    // (for example `id int AUTO_INCREMENT PRIMARY KEY`) is an ordinary
    // stored column and must survive normalization.
    if schema.has_implicit_id() {
        record.remove(ID_COLUMN);
    }
    for name in record.keys() {
        if schema.column(name).is_none() {
            return Err(Error::SchemaViolation(format!("unknown column '{name}'")));
        }
    }
    for col in &schema.columns {
        match record.get(&col.name) {
            Some(value) => check_value(col, value)?,
            None => {
                // An omitted column takes its declared default, or NULL.
                let value = col.default_value()?;
                if value.is_null() && !col.nullable {
                    return Err(Error::SchemaViolation(format!(
                        "column '{}' is not nullable",
                        col.name
                    )));
                }
                record.insert(col.name.clone(), value);
            }
        }
    }
    Ok(record)
}

// Record payload layout: u16 field count, then per field a u16-length-prefixed
// column name followed by a tagged value. Self-describing so that schema
// evolution in later phases does not invalidate old segments.

/// Encode a normalized record directly in schema column order, borrowing
/// names and values (no intermediate field list).
/// Writes large blob values out-of-line into `blobs/` during encoding.
pub(crate) struct BlobSink {
    dir: PathBuf,
    threshold: usize,
    staged: Vec<(PathBuf, PathBuf)>,
}

struct PendingBlobPublications {
    shared: Arc<Shared>,
    names: Vec<String>,
}

impl PendingBlobPublications {
    fn new(shared: Arc<Shared>, sink: &BlobSink) -> Self {
        let names = sink
            .staged
            .iter()
            .filter_map(|(_, final_path)| {
                final_path
                    .file_name()?
                    .to_str()?
                    .strip_suffix(".blob")
                    .map(str::to_owned)
            })
            .collect::<Vec<_>>();
        if !names.is_empty() {
            let mut pending = shared.pending_blob_publications.lock().unwrap();
            pending.extend(names.iter().cloned());
        }
        Self { shared, names }
    }
}

impl Drop for PendingBlobPublications {
    fn drop(&mut self) {
        if !self.names.is_empty() {
            let mut pending = self.shared.pending_blob_publications.lock().unwrap();
            for name in &self.names {
                pending.remove(name);
            }
        }
    }
}

impl BlobSink {
    fn new(db_dir: &Path, threshold: usize) -> BlobSink {
        BlobSink {
            dir: db_dir.join(BLOBS_DIR),
            threshold,
            staged: Vec::new(),
        }
    }

    /// Externalize `content` if it crosses the threshold; the chunk file is
    /// fsynced before the WAL commit that references it.
    fn maybe_externalize(&mut self, content: &[u8]) -> Result<Option<(String, u32)>> {
        if content.len() < self.threshold {
            return Ok(None);
        }
        let name = Ulid::new().to_string();
        let (bytes, crc) = write_blob_file_bytes(content);
        let final_path = self.dir.join(format!("{name}.blob"));
        let temporary_path = self.dir.join(format!("{name}.blob.tmp"));
        let mut f = File::create(&temporary_path)?;
        f.write_all(&bytes)?;
        f.sync_all()?;
        self.staged.push((temporary_path, final_path));
        Ok(Some((name, crc)))
    }

    /// Atomically expose every prepared chunk. Callers serialize this with
    /// blob GC through the database commit mutex.
    fn publish(&mut self) -> Result<()> {
        if self.staged.is_empty() {
            return Ok(());
        }
        for (temporary, final_path) in &self.staged {
            fs::rename(temporary, final_path)?;
        }
        fsync_dir(&self.dir)?;
        self.staged.clear();
        Ok(())
    }
}

impl Drop for BlobSink {
    fn drop(&mut self) {
        for (temporary, _) in &self.staged {
            let _ = fs::remove_file(temporary);
        }
    }
}

pub(crate) fn encode_record_ordered(
    schema: &TableSchema,
    record: &Record,
    mut sink: Option<&mut BlobSink>,
) -> Result<Vec<u8>> {
    let mut buf = Vec::with_capacity(64);
    let column_count = u16::try_from(schema.columns.len())
        .map_err(|_| Error::InvalidArgument("record has more than 65535 stored columns".into()))?;
    buf.extend_from_slice(&column_count.to_le_bytes());
    for col in &schema.columns {
        let name_len = u16::try_from(col.name.len()).map_err(|_| {
            Error::InvalidArgument("column name exceeds the 65535-byte storage limit".into())
        })?;
        buf.extend_from_slice(&name_len.to_le_bytes());
        buf.extend_from_slice(col.name.as_bytes());
        let value = record.get(&col.name).unwrap_or(&Value::Null);
        match (value, sink.as_deref_mut()) {
            (Value::Blob(content), Some(sink)) => match sink.maybe_externalize(content)? {
                Some((name, crc)) => encode_blob_ref(&mut buf, &name, content.len() as u64, crc)?,
                None => encode_value(&mut buf, value),
            },
            _ => encode_value(&mut buf, value),
        }
    }
    Ok(buf)
}

pub(crate) fn decode_record(buf: &[u8], blobs: Option<&Path>) -> Result<Record> {
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
        let value = decode_value(buf, &mut pos, blobs)?;
        record.insert(name, value);
    }
    Ok(record)
}

/// Collect the out-of-line blob references inside an encoded record payload
/// without materializing any values (compaction GC and check()).
pub(crate) fn scan_payload_blob_refs(buf: &[u8], out: &mut Vec<BlobRef>) -> Result<()> {
    let mut pos = 0usize;
    let count = read_u16(buf, &mut pos)? as usize;
    for _ in 0..count {
        let name_len = read_u16(buf, &mut pos)? as usize;
        pos = pos
            .checked_add(name_len)
            .filter(|&p| p <= buf.len())
            .ok_or_else(|| Error::Corrupt("unexpected end of record".into()))?;
        skip_value(buf, &mut pos, Some(out))?;
    }
    Ok(())
}

#[cfg(test)]
mod primary_index_tests {
    use super::*;

    #[test]
    fn point_read_admission_is_inactive_without_writers_and_bounds_mixed_readers() {
        let dir = tempfile::tempdir().unwrap();
        let db = Arc::new(Db::create(dir.path().join("point-admission.esql")).unwrap());
        db.create_table(TableSchema::new(
            "docs",
            vec![Column::new("value", ColumnType::Int64)],
        ))
        .unwrap();

        let unthrottled = PointReadAdmission::enter(&db.shared);
        assert!(!unthrottled.counted);
        drop(unthrottled);
        assert_eq!(
            db.shared
                .point_read_throttle_count
                .load(AtomicOrdering::Relaxed),
            0
        );

        db.shared.active_state_writers.store(
            db.shared.point_read_parallelism.max(1) as u64,
            AtomicOrdering::Release,
        );
        let held = PointReadAdmission::enter(&db.shared);
        assert!(held.counted);
        let worker_db = db.clone();
        let (done_tx, done_rx) = mpsc::channel();
        let worker = std::thread::spawn(move || {
            done_tx.send(worker_db.get("docs", "missing")).unwrap();
        });
        let deadline = Instant::now() + Duration::from_secs(2);
        while db
            .shared
            .point_read_throttle_count
            .load(AtomicOrdering::Acquire)
            == 0
        {
            assert!(Instant::now() < deadline, "second reader was not throttled");
            std::thread::yield_now();
        }
        assert!(done_rx.recv_timeout(Duration::from_millis(50)).is_err());

        drop(held);
        assert!(done_rx
            .recv_timeout(Duration::from_secs(2))
            .unwrap()
            .unwrap()
            .is_none());
        worker.join().unwrap();
        db.shared
            .active_state_writers
            .store(0, AtomicOrdering::Release);
        assert_eq!(
            db.shared.admitted_point_reads.load(AtomicOrdering::Acquire),
            0
        );
    }

    #[test]
    fn paged_primary_roundtrips_many_ids_and_versions() {
        let dir = tempfile::tempdir().unwrap();
        let mut resident = HashMap::new();
        let mut ids = BTreeMap::new();
        for id in 0..200u32 {
            let mut versions = Vec::new();
            for version in 1..=5u64 {
                versions.push(VersionEntry {
                    version: version * 1000 + id as u64,
                    kind: VKind::SegPut {
                        segment: 7,
                        payload_offset: version * 10,
                        payload_len: id,
                    },
                });
            }
            ids.insert(format!("{id:026}"), versions);
        }
        resident.insert("docs".to_owned(), ids);
        let path = dir.path().join("primary.pidx");
        let mut catalog = Catalog::new();
        catalog.tables.push(TableSchema::new("docs", Vec::new()));
        PrimaryIdx::resident(resident)
            .write_paged_for_catalog(&path, dir.path(), 99, 512, &catalog)
            .unwrap();
        let base = PagedIndex::open(&path).unwrap();
        let mut raw_entries = 0usize;
        let mut raw_ids = BTreeSet::new();
        base.scan(|key, _| {
            raw_entries += 1;
            raw_ids.insert(decode_primary_key(key)?.1.to_owned());
            Ok(())
        })
        .unwrap();
        assert_eq!(raw_entries, 1000);
        assert_eq!(raw_ids.len(), 200);
        let index = PrimaryIdx::paged(base);
        let mut visited = BTreeSet::new();
        index
            .visit_table("docs", None, |id, versions| {
                visited.insert(id.to_owned());
                assert_eq!(versions.len(), 5);
                Ok(true)
            })
            .unwrap();
        let missing: Vec<_> = raw_ids.difference(&visited).cloned().collect();
        assert_eq!(visited.len(), 200, "missing {missing:?}");
    }

    #[test]
    fn blob_gc_ignores_commit_staging_files_until_publish() {
        let dir = tempfile::tempdir().unwrap();
        fs::create_dir_all(dir.path().join(BLOBS_DIR)).unwrap();
        let mut sink = BlobSink::new(dir.path(), 1);
        let content = b"large durable payload";
        let (name, _) = sink
            .maybe_externalize(content)
            .unwrap()
            .expect("threshold externalizes");
        let temporary = dir.path().join(BLOBS_DIR).join(format!("{name}.blob.tmp"));
        let final_path = dir.path().join(BLOBS_DIR).join(format!("{name}.blob"));
        assert!(temporary.exists());

        gc_blob_files(&dir.path().join(BLOBS_DIR), |_| false);
        assert!(temporary.exists(), "GC must not remove an in-flight commit");
        sink.publish().unwrap();
        assert!(!temporary.exists());
        assert!(final_path.exists());
    }

    #[test]
    fn compaction_gc_preserves_blob_published_before_commit_lock() {
        struct ReleasePause(Arc<Shared>);
        impl Drop for ReleasePause {
            fn drop(&mut self) {
                self.0
                    .blob_test_pause_after_publish
                    .store(false, AtomicOrdering::Release);
            }
        }

        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("pending-blob-gc.esql");
        let db = Arc::new(
            Db::create_with(
                &path,
                DbOptions {
                    external_blob_threshold: 1,
                    ..DbOptions::default()
                },
            )
            .unwrap(),
        );
        db.create_table(TableSchema::new(
            "docs",
            vec![Column::new("payload", ColumnType::Blob)],
        ))
        .unwrap();
        db.shared
            .blob_test_pause_after_publish
            .store(true, AtomicOrdering::Release);
        let release = ReleasePause(db.shared.clone());
        let writer_db = db.clone();
        let writer = std::thread::spawn(move || {
            let mut record = Record::new();
            record.insert("id".into(), Value::Text("blob".into()));
            record.insert("payload".into(), Value::Blob(vec![7; 4 * 1024]));
            writer_db.insert("docs", record)
        });
        let deadline = Instant::now() + Duration::from_secs(5);
        while !db
            .shared
            .blob_test_reached_publish
            .load(AtomicOrdering::Acquire)
        {
            assert!(Instant::now() < deadline, "blob publisher did not pause");
            std::thread::yield_now();
        }

        db.compact().unwrap();
        assert_eq!(
            fs::read_dir(path.join(BLOBS_DIR))
                .unwrap()
                .filter_map(|entry| entry.ok())
                .filter(|entry| entry.path().extension().is_some_and(|ext| ext == "blob"))
                .count(),
            1,
            "GC must retain a durable blob whose WAL publication is pending"
        );
        drop(release);
        writer.join().unwrap().unwrap();
        assert_eq!(
            db.get("docs", "blob").unwrap().unwrap()["payload"],
            Value::Blob(vec![7; 4 * 1024])
        );
        drop(db);

        let reopened = Db::open(&path).unwrap();
        assert_eq!(
            reopened.get("docs", "blob").unwrap().unwrap()["payload"],
            Value::Blob(vec![7; 4 * 1024])
        );
    }

    #[test]
    fn sync_failure_is_reported_as_unknown_after_logical_publication() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("sync-unknown.esql");
        let db = Db::create(&path).unwrap();
        db.create_table(TableSchema::new(
            "docs",
            vec![Column::new("value", ColumnType::Int64)],
        ))
        .unwrap();
        db.shared.commit.lock().wal().fail_next_sync_for_test();

        let mut first = Record::new();
        first.insert("id".into(), Value::Text("a".into()));
        first.insert("value".into(), Value::Int64(1));
        assert!(matches!(
            db.insert("docs", first),
            Err(Error::CommitUnknown(_))
        ));
        assert_eq!(db.snapshot().version(), 1);
        assert_eq!(
            db.get("docs", "a").unwrap().unwrap()["value"],
            Value::Int64(1)
        );

        let mut second = Record::new();
        second.insert("id".into(), Value::Text("b".into()));
        second.insert("value".into(), Value::Int64(2));
        db.insert("docs", second).unwrap();
        assert_eq!(db.snapshot().version(), 2);
        drop(db);

        let reopened = Db::open(&path).unwrap();
        assert_eq!(reopened.scan("docs").unwrap().len(), 2);
    }

    #[test]
    fn coordinated_safe_sync_failure_is_reported_to_every_batch_member() {
        const WRITERS: usize = 8;

        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("coordinated-sync-unknown.esql");
        let db = Arc::new(
            Db::create_with(
                &path,
                DbOptions {
                    durability: Durability::Safe,
                    safe_group_commit_delay_us: 100_000,
                    ..DbOptions::default()
                },
            )
            .unwrap(),
        );
        db.create_table(TableSchema::new(
            "docs",
            vec![Column::new("value", ColumnType::Int64)],
        ))
        .unwrap();
        db.shared.commit.lock().wal().fail_next_sync_for_test();

        // Keep the contention signal set until the elected leader has opened
        // the test's deliberately wide coalescing window.
        db.shared
            .active_state_writers
            .fetch_add(1, AtomicOrdering::Release);
        let ready = Arc::new(std::sync::Barrier::new(WRITERS));
        let handles = (0..WRITERS)
            .map(|writer| {
                let db = db.clone();
                let ready = ready.clone();
                std::thread::spawn(move || {
                    let mut record = Record::new();
                    record.insert("id".into(), Value::Text(format!("writer-{writer}")));
                    record.insert("value".into(), Value::Int64(writer as i64));
                    let mut transaction = db.begin();
                    transaction.insert("docs", record).unwrap();
                    ready.wait();
                    transaction.commit()
                })
            })
            .collect::<Vec<_>>();
        let outcomes = handles
            .into_iter()
            .map(|handle| handle.join().unwrap())
            .collect::<Vec<_>>();
        db.shared
            .active_state_writers
            .fetch_sub(1, AtomicOrdering::Release);

        assert!(
            outcomes
                .iter()
                .all(|outcome| matches!(outcome, Err(Error::CommitUnknown(_)))),
            "every member must receive the shared sync failure: {outcomes:?}"
        );
        let stats = db.maintenance_stats();
        assert_eq!(stats.wal_syncs, 1);
        assert_eq!(stats.wal_sync_max_group_commits, WRITERS as u64);
        assert_eq!(db.scan("docs").unwrap().len(), WRITERS);

        let mut durable = Record::new();
        durable.insert("id".into(), Value::Text("durable".into()));
        durable.insert("value".into(), Value::Int64(99));
        db.insert("docs", durable).unwrap();
        drop(db);
        assert_eq!(
            Db::open(&path).unwrap().scan("docs").unwrap().len(),
            WRITERS + 1
        );
    }

    #[test]
    fn ambiguous_manifest_refresh_adopts_generation_and_fences_writes() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("manifest-unknown.esql");
        let db = Db::create(&path).unwrap();
        db.create_table(TableSchema::new(
            "docs",
            vec![Column::new("value", ColumnType::Int64)],
        ))
        .unwrap();
        let mut record = Record::new();
        record.insert("id".into(), Value::Text("a".into()));
        record.insert("value".into(), Value::Int64(1));
        db.insert("docs", record).unwrap();

        db.shared
            .primary_test_fail_after_manifest_rename
            .store(true, AtomicOrdering::Release);
        assert!(matches!(db.checkpoint(), Err(Error::CommitUnknown(_))));
        assert!(
            wal_path(&path, 1).exists(),
            "an ambiguous fallback refresh must not delete the old WAL"
        );
        assert!(wal_path(&path, 2).exists());
        assert_eq!(
            db.get("docs", "a").unwrap().unwrap()["value"],
            Value::Int64(1)
        );

        let mut refused = Record::new();
        refused.insert("value".into(), Value::Int64(2));
        assert!(matches!(
            db.insert("docs", refused),
            Err(Error::CommitUnknown(_))
        ));
        drop(db);

        let reopened = Db::open(&path).unwrap();
        assert_eq!(
            reopened.get("docs", "a").unwrap().unwrap()["value"],
            Value::Int64(1)
        );
        let mut accepted = Record::new();
        accepted.insert("value".into(), Value::Int64(3));
        reopened.insert("docs", accepted).unwrap();
    }

    #[test]
    fn checkpoint_manifest_rename_failure_thaws_and_retries() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("manifest-rename-retry.esql");
        let db = Db::create(&path).unwrap();
        db.create_table(TableSchema::new(
            "docs",
            vec![Column::new("value", ColumnType::Int64)],
        ))
        .unwrap();
        let mut record = Record::new();
        record.insert("id".into(), Value::Text("a".into()));
        record.insert("value".into(), Value::Int64(1));
        db.insert("docs", record).unwrap();

        db.shared
            .primary_test_fail_before_manifest_rename
            .store(true, AtomicOrdering::Release);
        let error = db.checkpoint().unwrap_err();
        assert!(error
            .to_string()
            .contains("injected primary manifest rename failure"));
        assert!(
            db.shared.state.read().unwrap().index.frozen.is_none(),
            "a pre-publication failure must thaw the queryable generation"
        );
        assert_eq!(
            db.get("docs", "a").unwrap().unwrap()["value"],
            Value::Int64(1)
        );

        db.checkpoint().unwrap();
        drop(db);
        let reopened = Db::open(&path).unwrap();
        assert_eq!(
            reopened.get("docs", "a").unwrap().unwrap()["value"],
            Value::Int64(1)
        );
    }

    #[test]
    fn dead_vector_worker_with_backlog_is_reported_instead_of_waiting_forever() {
        let dir = tempfile::tempdir().unwrap();
        let db = Db::create(dir.path().join("worker-failure.esql")).unwrap();
        db.shared.vector_backlog.store(1, AtomicOrdering::Release);
        db.shared
            .vector_worker_alive
            .store(false, AtomicOrdering::Release);
        *db.shared.vector_worker_error.lock().unwrap() = Some("injected worker failure".into());

        let error = db.wait_vector_indexing().unwrap_err();
        assert!(
            error.to_string().contains("injected worker failure"),
            "{error}"
        );

        // Restore the synthetic state before dropping the otherwise healthy
        // database and its real worker thread.
        db.shared.vector_backlog.store(0, AtomicOrdering::Release);
        db.shared
            .vector_worker_alive
            .store(true, AtomicOrdering::Release);
    }

    #[test]
    fn stale_async_vector_job_cannot_resurrect_a_deleted_record() {
        let dir = tempfile::tempdir().unwrap();
        let db = Db::create(dir.path().join("stale-vector-job.esql")).unwrap();
        db.create_table(TableSchema::new(
            "docs",
            vec![Column::vector("embedding", 2)],
        ))
        .unwrap();
        db.create_vector_index(
            "docs",
            "embedding",
            VectorIndexOptions {
                mode: IndexingMode::Async,
                ..VectorIndexOptions::default()
            },
        )
        .unwrap();
        let mut record = Record::new();
        record.insert("id".into(), Value::Text("victim".into()));
        record.insert("embedding".into(), Value::Vector(vec![1.0, 0.0]));
        db.insert("docs", record).unwrap();
        db.wait_vector_indexing().unwrap();
        let stale_version = db.snapshot().version();
        db.delete("docs", "victim").unwrap();

        db.shared
            .vector_backlog
            .fetch_add(1, AtomicOrdering::SeqCst);
        db.shared
            .vector_tx
            .lock()
            .unwrap()
            .as_ref()
            .unwrap()
            .send(VecJob {
                version: stale_version,
                table: "docs".into(),
                column: "embedding".into(),
                id: "victim".into(),
                vector: vec![1.0, 0.0],
            })
            .unwrap();
        db.wait_vector_indexing().unwrap();

        let state = db.shared.state.read().unwrap();
        let ids = state
            .vector
            .get(&("docs".to_owned(), "embedding".to_owned()))
            .unwrap()
            .ids();
        assert!(!ids.iter().any(|id| id == "victim"));
    }

    #[test]
    fn commits_and_queries_continue_while_derived_overlays_publish() {
        struct ReleasePause(Arc<Shared>);
        impl Drop for ReleasePause {
            fn drop(&mut self) {
                self.0
                    .derived_test_pause_before_publish
                    .store(false, AtomicOrdering::Release);
            }
        }

        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("derived-background.esql");
        let db = Db::create(&path).unwrap();
        db.query(
            "CREATE TABLE docs (tag text NOT NULL, body text NOT NULL, embedding vector(2) NOT NULL)",
        )
        .unwrap();
        db.query("CREATE INDEX ON docs (tag)").unwrap();
        db.create_text_index("docs", "body").unwrap();
        db.create_vector_index("docs", "embedding", VectorIndexOptions::default())
            .unwrap();
        let mut transaction = db.begin();
        for n in 0..100 {
            let mut record = Record::new();
            record.insert("id".into(), Value::Text(format!("doc-{n:03}")));
            record.insert("tag".into(), Value::Text("old".into()));
            record.insert("body".into(), Value::Text("frozen searchable text".into()));
            record.insert("embedding".into(), Value::Vector(vec![1.0, 0.0]));
            transaction.insert("docs", record).unwrap();
        }
        transaction.commit().unwrap();

        db.shared
            .derived_test_pause_before_publish
            .store(true, AtomicOrdering::Release);
        let _release = ReleasePause(db.shared.clone());
        let cs = db.shared.commit.lock();
        let memory = Db::acquire_maintenance_memory(&db.shared);
        assert!(schedule_frozen_derived(&db.shared, memory).unwrap());
        drop(cs);

        let deadline = Instant::now() + Duration::from_secs(5);
        while !db
            .shared
            .derived_test_reached_publish
            .load(AtomicOrdering::Acquire)
        {
            assert!(
                Instant::now() < deadline,
                "derived worker did not reach publication"
            );
            std::thread::sleep(Duration::from_millis(1));
        }

        // Exercise the coincident-threshold path: primary maintenance must
        // be deferred while the derived worker owns the maintenance pool.
        db.shared.commit.lock().memtable_bytes = db.shared.opts.memtable_max_bytes;

        let mut patch = Record::new();
        patch.insert("tag".into(), Value::Text("new".into()));
        patch.insert("body".into(), Value::Text("fresh generation token".into()));
        patch.insert("embedding".into(), Value::Vector(vec![0.0, 1.0]));
        db.update("docs", "doc-000", patch).unwrap();
        db.delete("docs", "doc-001").unwrap();

        let new_ids = db
            .find_eq("docs", "tag", &Value::Text("new".into()))
            .unwrap();
        let old_ids = db
            .find_eq("docs", "tag", &Value::Text("old".into()))
            .unwrap();
        let fresh = db.search_text("docs", "body", "fresh", 10, None).unwrap();
        let nearest = db
            .search_vector(
                "docs",
                "embedding",
                &[0.0, 1.0],
                1,
                &VectorSearchOptions::default(),
            )
            .unwrap();
        assert_eq!(new_ids.len(), 1);
        assert_eq!(old_ids.len(), 98);
        assert_eq!(fresh.len(), 1);
        assert_eq!(fresh[0].id, "doc-000");
        assert_eq!(nearest[0].id, "doc-000");

        // Maintenance must wait without owning the commit mutex: the derived
        // worker owns its memory permit and needs that mutex to publish.
        let maintenance_shared = db.shared.clone();
        let (maintenance_done, maintenance_wait) = std::sync::mpsc::channel();
        let maintenance_thread = std::thread::spawn(move || {
            let ready = lock_commit_for_maintenance(&maintenance_shared)
                .map(drop)
                .is_ok();
            maintenance_done.send(ready).unwrap();
        });
        std::thread::sleep(Duration::from_millis(20));
        assert!(matches!(
            maintenance_wait.try_recv(),
            Err(std::sync::mpsc::TryRecvError::Empty)
        ));

        drop(_release);
        assert!(maintenance_wait
            .recv_timeout(Duration::from_secs(5))
            .unwrap());
        maintenance_thread.join().unwrap();
        wait_for_background_derived(&db.shared).unwrap();
        assert_eq!(db.maintenance_stats().derived_publications, 1);
        assert_eq!(
            db.find_eq("docs", "tag", &Value::Text("new".into()))
                .unwrap()
                .len(),
            1
        );
        assert_eq!(
            db.search_text("docs", "body", "fresh", 10, None).unwrap()[0].id,
            "doc-000"
        );
        assert_eq!(
            db.search_vector(
                "docs",
                "embedding",
                &[0.0, 1.0],
                1,
                &VectorSearchOptions::default(),
            )
            .unwrap()[0]
                .id,
            "doc-000"
        );
        drop(db);

        let reopened = Db::open(&path).unwrap();
        assert_eq!(
            reopened
                .find_eq("docs", "tag", &Value::Text("new".into()))
                .unwrap()
                .len(),
            1
        );
        assert!(reopened.get("docs", "doc-001").unwrap().is_none());
    }

    #[test]
    fn commits_continue_and_recover_before_primary_manifest_publication() {
        struct ReleasePause(Arc<Shared>);
        impl Drop for ReleasePause {
            fn drop(&mut self) {
                self.0
                    .primary_test_pause_before_manifest
                    .store(false, AtomicOrdering::Release);
            }
        }

        fn copy_tree(from: &Path, to: &Path) {
            fs::create_dir_all(to).unwrap();
            for entry in fs::read_dir(from).unwrap() {
                let entry = entry.unwrap();
                let target = to.join(entry.file_name());
                if entry.file_type().unwrap().is_dir() {
                    copy_tree(&entry.path(), &target);
                } else {
                    fs::copy(entry.path(), target).unwrap();
                }
            }
        }

        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("primary-publication.esql");
        let crash_copy = dir.path().join("primary-publication-crash.esql");
        let db = Db::create_with(
            &path,
            DbOptions {
                durability: Durability::Safe,
                ..DbOptions::default()
            },
        )
        .unwrap();
        db.create_table(TableSchema::new(
            "docs",
            vec![Column::new("value", ColumnType::Int64)],
        ))
        .unwrap();
        let mut first = Record::new();
        first.insert("id".into(), Value::Text("before".into()));
        first.insert("value".into(), Value::Int64(1));
        db.insert("docs", first).unwrap();

        db.shared
            .primary_test_pause_before_manifest
            .store(true, AtomicOrdering::Release);
        let release = ReleasePause(db.shared.clone());
        {
            let mut cs = db.shared.commit.lock();
            let memory = Db::acquire_maintenance_memory(&db.shared);
            assert!(schedule_frozen_checkpoint(&db.shared, &mut cs, memory).unwrap());
        }
        let deadline = Instant::now() + Duration::from_secs(5);
        while !db
            .shared
            .primary_test_reached_manifest
            .load(AtomicOrdering::Acquire)
        {
            assert!(
                Instant::now() < deadline,
                "primary worker did not reach manifest publication"
            );
            std::thread::sleep(Duration::from_millis(1));
        }

        // The worker owns only the primary publication mutex here. This Safe
        // commit must complete and sync its active successor WAL independently.
        let mut tail = Record::new();
        tail.insert("id".into(), Value::Text("during".into()));
        tail.insert("value".into(), Value::Int64(2));
        db.insert("docs", tail).unwrap();

        // Snapshot the durable files at the exact crash boundary: the old
        // canonical manifest is still visible, while bridge + active WALs
        // contain all acknowledged versions.
        copy_tree(&path, &crash_copy);
        let recovered = Db::open_with(
            &crash_copy,
            DbOptions {
                durability: Durability::Safe,
                ..DbOptions::default()
            },
        )
        .unwrap();
        assert_eq!(recovered.scan("docs").unwrap().len(), 2);
        assert_eq!(
            recovered.get("docs", "during").unwrap().unwrap()["value"],
            Value::Int64(2)
        );
        drop(recovered);

        drop(release);
        wait_for_background_checkpoint(&db.shared).unwrap();
        assert_eq!(db.scan("docs").unwrap().len(), 2);
        drop(db);

        let reopened = Db::open(&path).unwrap();
        assert_eq!(reopened.scan("docs").unwrap().len(), 2);
    }

    #[test]
    fn retried_derived_publication_preserves_the_frozen_generation() {
        let dir = tempfile::tempdir().unwrap();
        let spill = dir.path().join("derived-retry-spill");
        let db = Db::create_with(
            dir.path().join("derived-retry-generation.esql"),
            DbOptions {
                memory: MemoryOptions {
                    spill_directory: Some(spill.clone()),
                    ..MemoryOptions::default()
                },
                ..DbOptions::default()
            },
        )
        .unwrap();
        db.query("CREATE TABLE docs (tag text NOT NULL, body text NOT NULL, embedding vector(2) NOT NULL)")
            .unwrap();
        db.query("CREATE INDEX ON docs (tag)").unwrap();
        db.create_text_index("docs", "body").unwrap();
        db.create_vector_index("docs", "embedding", VectorIndexOptions::default())
            .unwrap();

        let mut record = Record::new();
        record.insert("id".into(), Value::Text("doc".into()));
        record.insert("tag".into(), Value::Text("old".into()));
        record.insert("body".into(), Value::Text("old body".into()));
        record.insert("embedding".into(), Value::Vector(vec![1.0, 0.0]));
        db.insert("docs", record).unwrap();

        let frozen_generation = {
            let _commit = db.shared.commit.lock();
            let mut state = db.shared.state.write().unwrap();
            let generation = state.committed_version;
            state
                .secondary
                .get_mut(&("docs".into(), "tag".into()))
                .unwrap()
                .freeze_delta(generation)
                .unwrap();
            state
                .text
                .get_mut(&("docs".into(), "body".into()))
                .unwrap()
                .freeze_delta_background(generation)
                .unwrap();
            state
                .vector
                .get_mut(&("docs".into(), "embedding".into()))
                .unwrap()
                .freeze_delta_background(generation)
                .unwrap();
            generation
        };

        let mut patch = Record::new();
        patch.insert("tag".into(), Value::Text("new".into()));
        patch.insert("body".into(), Value::Text("new body".into()));
        patch.insert("embedding".into(), Value::Vector(vec![0.0, 1.0]));
        db.update("docs", "doc", patch).unwrap();

        if spill.is_dir() {
            fs::remove_dir(&spill).unwrap();
        }
        fs::write(&spill, b"block create_dir_all").unwrap();
        let commit = db.shared.commit.lock();
        let memory = Db::acquire_maintenance_memory(&db.shared);
        assert!(schedule_frozen_derived(&db.shared, memory).unwrap());
        drop(commit);
        assert!(wait_for_background_derived(&db.shared).is_err());
        let retained_after_failure = {
            let state = db.shared.state.read().unwrap();
            assert!(state.has_frozen_derived());
            state
                .index
                .delta_memory_bytes()
                .saturating_add(state.derived_delta_memory_bytes_including_frozen())
        };
        let memory_after_failure = db.global_memory_stats();
        assert_eq!(memory_after_failure.maintenance_in_use_bytes, 0);
        assert_eq!(
            memory_after_failure.index_delta_bytes,
            retained_after_failure as u64
        );

        fs::remove_file(&spill).unwrap();
        fs::create_dir(&spill).unwrap();
        let commit = db.shared.commit.lock();
        let memory = Db::acquire_maintenance_memory(&db.shared);
        assert!(schedule_frozen_derived(&db.shared, memory).unwrap());
        drop(commit);
        wait_for_background_derived(&db.shared).unwrap();

        let state = db.shared.state.read().unwrap();
        assert_eq!(
            state
                .secondary
                .get(&("docs".into(), "tag".into()))
                .unwrap()
                .runs
                .last()
                .unwrap()
                .meta
                .generation,
            frozen_generation
        );
        assert_eq!(
            state
                .text
                .get(&("docs".into(), "body".into()))
                .unwrap()
                .runs
                .last()
                .unwrap()
                .meta
                .generation,
            frozen_generation
        );
        assert_eq!(
            state
                .vector
                .get(&("docs".into(), "embedding".into()))
                .unwrap()
                .frozen_generation(),
            None
        );
    }

    #[test]
    fn derived_manifest_rename_and_sync_failures_preserve_queryability() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("derived-manifest-failures.esql");
        let db = Db::create(&path).unwrap();
        db.query("CREATE TABLE docs (tag text NOT NULL)").unwrap();
        db.query("CREATE INDEX ON docs (tag)").unwrap();

        let mut first = Record::new();
        first.insert("id".into(), Value::Text("a".into()));
        first.insert("tag".into(), Value::Text("kept".into()));
        db.insert("docs", first).unwrap();
        db.shared
            .derived_test_fail_before_manifest_rename
            .store(true, AtomicOrdering::Release);
        let error = db.checkpoint().unwrap_err();
        assert!(error
            .to_string()
            .contains("injected derived manifest rename failure"));
        assert_eq!(
            db.find_eq("docs", "tag", &Value::Text("kept".into()))
                .unwrap()
                .len(),
            1,
            "the frozen overlay remains queryable after a failed rename"
        );

        db.checkpoint().unwrap();
        let mut second = Record::new();
        second.insert("id".into(), Value::Text("b".into()));
        second.insert("tag".into(), Value::Text("kept".into()));
        db.insert("docs", second).unwrap();
        db.shared
            .derived_test_fail_after_manifest_rename
            .store(true, AtomicOrdering::Release);
        let error = db.checkpoint().unwrap_err();
        assert!(error
            .to_string()
            .contains("injected derived manifest directory sync failure"));
        assert_eq!(
            db.find_eq("docs", "tag", &Value::Text("kept".into()))
                .unwrap()
                .len(),
            2,
            "a logically renamed disposable manifest is adopted before reporting fsync failure"
        );
        drop(db);

        let reopened = Db::open(&path).unwrap();
        assert_eq!(
            reopened
                .find_eq("docs", "tag", &Value::Text("kept".into()))
                .unwrap()
                .len(),
            2
        );
    }

    #[test]
    fn open_cleans_only_abandoned_derived_publication_files() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("pending-cleanup.esql");
        drop(Db::create(&path).unwrap());

        let pending_index = path.join(INDEXES_DIR).join("orphan.derived-pending");
        let pending_vector = path.join(VECTORS_DIR).join("orphan.derived-pending.tmp");
        let unrelated = path.join(INDEXES_DIR).join("keep.pending");
        fs::write(&pending_index, b"pending").unwrap();
        fs::write(&pending_vector, b"pending").unwrap();
        fs::write(&unrelated, b"keep").unwrap();

        drop(Db::open(&path).unwrap());
        assert!(!pending_index.exists());
        assert!(!pending_vector.exists());
        assert!(unrelated.exists());
    }
}
