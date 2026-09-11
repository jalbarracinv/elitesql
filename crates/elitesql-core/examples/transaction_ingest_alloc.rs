//! Allocation-instrumented transactional ingest diagnostic.
//!
//! This is intentionally a separate binary: the atomic allocator counters
//! perturb timings, so its results must never be mixed with normal benchmark
//! latency samples.
use elitesql_core::{
    AutoCompactionOptions, Column, ColumnType, Db, DbOptions, Durability, Record, TableSchema,
    Value,
};
use serde_json::json;
use std::alloc::{GlobalAlloc, Layout, System};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};

struct CountAlloc;
static ALLOCATIONS: AtomicU64 = AtomicU64::new(0);
static ALLOCATED_BYTES: AtomicU64 = AtomicU64::new(0);
static DEALLOCATED_BYTES: AtomicU64 = AtomicU64::new(0);
static LIVE_BYTES: AtomicU64 = AtomicU64::new(0);
static PEAK_LIVE_BYTES: AtomicU64 = AtomicU64::new(0);

fn add_live(bytes: u64) {
    let live = LIVE_BYTES
        .fetch_add(bytes, Ordering::Relaxed)
        .saturating_add(bytes);
    PEAK_LIVE_BYTES.fetch_max(live, Ordering::Relaxed);
}

unsafe impl GlobalAlloc for CountAlloc {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        let pointer = unsafe { System.alloc(layout) };
        if !pointer.is_null() {
            ALLOCATIONS.fetch_add(1, Ordering::Relaxed);
            ALLOCATED_BYTES.fetch_add(layout.size() as u64, Ordering::Relaxed);
            add_live(layout.size() as u64);
        }
        pointer
    }

    unsafe fn dealloc(&self, pointer: *mut u8, layout: Layout) {
        DEALLOCATED_BYTES.fetch_add(layout.size() as u64, Ordering::Relaxed);
        LIVE_BYTES.fetch_sub(layout.size() as u64, Ordering::Relaxed);
        unsafe { System.dealloc(pointer, layout) }
    }

    unsafe fn realloc(&self, pointer: *mut u8, layout: Layout, new_size: usize) -> *mut u8 {
        let new_pointer = unsafe { System.realloc(pointer, layout, new_size) };
        if !new_pointer.is_null() {
            ALLOCATIONS.fetch_add(1, Ordering::Relaxed);
            ALLOCATED_BYTES.fetch_add(new_size as u64, Ordering::Relaxed);
            DEALLOCATED_BYTES.fetch_add(layout.size() as u64, Ordering::Relaxed);
            if new_size >= layout.size() {
                add_live((new_size - layout.size()) as u64);
            } else {
                LIVE_BYTES.fetch_sub((layout.size() - new_size) as u64, Ordering::Relaxed);
            }
        }
        new_pointer
    }
}

#[global_allocator]
static ALLOCATOR: CountAlloc = CountAlloc;

#[derive(Clone, Copy, Default)]
struct AllocationSnapshot {
    allocations: u64,
    allocated_bytes: u64,
    deallocated_bytes: u64,
}

impl AllocationSnapshot {
    fn now() -> Self {
        Self {
            allocations: ALLOCATIONS.load(Ordering::Relaxed),
            allocated_bytes: ALLOCATED_BYTES.load(Ordering::Relaxed),
            deallocated_bytes: DEALLOCATED_BYTES.load(Ordering::Relaxed),
        }
    }

    fn since(self, before: Self) -> Self {
        Self {
            allocations: self.allocations.saturating_sub(before.allocations),
            allocated_bytes: self.allocated_bytes.saturating_sub(before.allocated_bytes),
            deallocated_bytes: self
                .deallocated_bytes
                .saturating_sub(before.deallocated_bytes),
        }
    }

    fn add(&mut self, other: Self) {
        self.allocations = self.allocations.saturating_add(other.allocations);
        self.allocated_bytes = self.allocated_bytes.saturating_add(other.allocated_bytes);
        self.deallocated_bytes = self
            .deallocated_bytes
            .saturating_add(other.deallocated_bytes);
    }
}

#[derive(Default)]
struct Phase {
    elapsed: Duration,
    allocation: AllocationSnapshot,
}

impl Phase {
    fn measure<T>(&mut self, operation: impl FnOnce() -> T) -> T {
        let before_alloc = AllocationSnapshot::now();
        let started = Instant::now();
        let result = operation();
        self.elapsed = self.elapsed.saturating_add(started.elapsed());
        self.allocation
            .add(AllocationSnapshot::now().since(before_alloc));
        result
    }

    fn json(&self) -> serde_json::Value {
        json!({
            "seconds": self.elapsed.as_secs_f64(),
            "allocations": self.allocation.allocations,
            "allocated_bytes": self.allocation.allocated_bytes,
            "deallocated_bytes": self.allocation.deallocated_bytes,
        })
    }
}

const BODY: &str = "A deterministic payload used by both engines for scalable benchmarks.";

fn record(i: usize) -> Record {
    let mut record = Record::new();
    record.insert("id".into(), Value::Text(format!("row-{i:010}")));
    record.insert("title".into(), Value::Text(format!("document number {i}")));
    record.insert("body".into(), Value::Text(BODY.into()));
    record.insert("score".into(), Value::Int64(i as i64));
    record
}

fn main() {
    let rows = std::env::var("ELITESQL_INGEST_ROWS")
        .ok()
        .and_then(|value| value.parse::<usize>().ok())
        .unwrap_or(1_000_000);
    let batch_size = std::env::var("ELITESQL_INGEST_BATCH")
        .ok()
        .and_then(|value| value.parse::<usize>().ok())
        .unwrap_or(10_000);
    assert!(rows > 0 && batch_size > 0);

    let directory = tempfile::tempdir().unwrap();
    let db = Db::create_with(
        directory.path().join("bench.esql"),
        DbOptions {
            durability: Durability::Fast,
            auto_compaction: AutoCompactionOptions::disabled(),
            ..DbOptions::default()
        },
    )
    .unwrap();
    db.create_table(TableSchema::new(
        "docs",
        vec![
            Column::new("title", ColumnType::Text).not_null(),
            Column::new("body", ColumnType::Text),
            Column::new("score", ColumnType::Int64),
        ],
    ))
    .unwrap();

    let stats_before = db.maintenance_stats();
    let total_before = AllocationSnapshot::now();
    let total_started = Instant::now();
    let mut construction = Phase::default();
    let mut staging = Phase::default();
    let mut commit = Phase::default();
    for start in (0..rows).step_by(batch_size) {
        let end = (start + batch_size).min(rows);
        let records = construction.measure(|| (start..end).map(record).collect::<Vec<_>>());
        let mut transaction = db.begin();
        staging.measure(|| {
            for row in records {
                transaction.insert("docs", row).unwrap();
            }
        });
        commit.measure(|| transaction.commit().unwrap());
    }
    let ingest_seconds = total_started.elapsed().as_secs_f64();

    let mut checkpoint = Phase::default();
    checkpoint.measure(|| db.checkpoint().unwrap());
    let mut drain = Phase::default();
    drain.measure(|| db.wait_for_primary_compaction().unwrap());
    let total_seconds = total_started.elapsed().as_secs_f64();
    let total_allocation = AllocationSnapshot::now().since(total_before);
    let stats = db.maintenance_stats();
    let memory = db.global_memory_stats();

    let last_id = format!("row-{:010}", rows - 1);
    let last = db.get("docs", &last_id).unwrap().expect("last row exists");
    assert_eq!(last.get("score"), Some(&Value::Int64((rows - 1) as i64)));

    println!(
        "{}",
        json!({
            "rows": rows,
            "batch_size": batch_size,
            "durability": "fast",
            "ingest_seconds": ingest_seconds,
            "total_seconds": total_seconds,
            "construction": construction.json(),
            "staging": staging.json(),
            "commit_calls": commit.json(),
            "checkpoint": checkpoint.json(),
            "maintenance_drain": drain.json(),
            "total_allocations": total_allocation.allocations,
            "total_allocated_bytes": total_allocation.allocated_bytes,
            "total_deallocated_bytes": total_allocation.deallocated_bytes,
            "live_bytes_at_end": LIVE_BYTES.load(Ordering::Relaxed),
            "peak_live_bytes": PEAK_LIVE_BYTES.load(Ordering::Relaxed),
            "commit_phases_ns": {
                "prepare": (stats.commit_phase_prepare_time - stats_before.commit_phase_prepare_time).as_nanos(),
                "record_encode": (stats.commit_phase_record_encode_time - stats_before.commit_phase_record_encode_time).as_nanos(),
                "validation": (stats.commit_phase_validation_time - stats_before.commit_phase_validation_time).as_nanos(),
                "wal_encode": (stats.commit_phase_wal_encode_time - stats_before.commit_phase_wal_encode_time).as_nanos(),
                "wal_append": (stats.commit_wal_append_time - stats_before.commit_wal_append_time).as_nanos(),
                "sync_wait": (stats.commit_phase_sync_wait_time - stats_before.commit_phase_sync_wait_time).as_nanos(),
                "mvcc_apply": (stats.commit_apply_time - stats_before.commit_apply_time).as_nanos(),
                "maintenance_wait": (stats.commit_phase_maintenance_wait_time - stats_before.commit_phase_maintenance_wait_time).as_nanos(),
            },
            "wal_appended_bytes": stats.wal_appended_bytes - stats_before.wal_appended_bytes,
            "checkpoint_bytes_written": stats.primary_checkpoint_bytes_written - stats_before.primary_checkpoint_bytes_written,
            "promotion_bytes_read": stats.primary_run_compaction_bytes_read - stats_before.primary_run_compaction_bytes_read,
            "promotion_bytes_written": stats.primary_run_compaction_bytes_written - stats_before.primary_run_compaction_bytes_written,
            "index_delta_peak_bytes": memory.index_delta_peak_bytes,
            "maintenance_peak_bytes": memory.maintenance_peak_bytes,
        })
    );
}
