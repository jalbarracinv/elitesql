//! Deterministic review benchmark, also runnable against the audited baseline.
//! JSON lines; allocation counters cover all engine threads during each phase.
use elitesql_core::{
    AutoCompactionOptions, Db, DbOptions, Durability, MemoryOptions, QueryOutput, Record, Value,
};
use serde_json::json;
use std::alloc::{GlobalAlloc, Layout, System};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Instant;

struct CountAlloc;
static ALLOCS: AtomicU64 = AtomicU64::new(0);
static BYTES: AtomicU64 = AtomicU64::new(0);
unsafe impl GlobalAlloc for CountAlloc {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        ALLOCS.fetch_add(1, Ordering::Relaxed);
        BYTES.fetch_add(layout.size() as u64, Ordering::Relaxed);
        unsafe { System.alloc(layout) }
    }
    unsafe fn dealloc(&self, pointer: *mut u8, layout: Layout) {
        unsafe { System.dealloc(pointer, layout) }
    }
    unsafe fn realloc(&self, pointer: *mut u8, layout: Layout, size: usize) -> *mut u8 {
        ALLOCS.fetch_add(1, Ordering::Relaxed);
        BYTES.fetch_add(size as u64, Ordering::Relaxed);
        unsafe { System.realloc(pointer, layout, size) }
    }
}
#[global_allocator]
static ALLOCATOR: CountAlloc = CountAlloc;

fn percentile(samples: &[u64], percent: usize) -> u64 {
    samples[((samples.len() - 1) * percent) / 100]
}

fn main() {
    let count: usize = std::env::var("ELITESQL_AUDIT_ROWS")
        .ok()
        .and_then(|value| value.parse().ok())
        .unwrap_or(20_000);
    let queries: usize = std::env::var("ELITESQL_AUDIT_QUERIES")
        .ok()
        .and_then(|value| value.parse().ok())
        .unwrap_or(200);
    assert!(count >= 200 && queries > 0);
    let directory = tempfile::tempdir().unwrap();
    let db = Db::create_with(
        directory.path().join("db"),
        DbOptions {
            durability: Durability::Fast,
            auto_compaction: AutoCompactionOptions::disabled(),
            memtable_max_bytes: u64::MAX,
            memory: MemoryOptions {
                query_working_bytes: 1024 * 1024,
                ..MemoryOptions::default()
            },
            ..DbOptions::default()
        },
    )
    .unwrap();
    db.query("CREATE TABLE items(n int, payload text)").unwrap();
    let started = Instant::now();
    for first in (0..count).step_by(1000) {
        let mut transaction = db.begin();
        for n in first..(first + 1000).min(count) {
            let mut row = Record::new();
            row.insert("id".into(), Value::Text(format!("r{n:08}")));
            row.insert("n".into(), Value::Int64(n as i64));
            row.insert("payload".into(), Value::Text("review fixture".into()));
            transaction.insert("items", row).unwrap();
        }
        transaction.commit().unwrap();
    }
    println!(
        "{}",
        json!({"phase":"load","rows":count,"seconds":started.elapsed().as_secs_f64(),"durability":"fast"})
    );
    db.query("CREATE INDEX ON items(n)").unwrap();
    db.checkpoint().unwrap();
    for phase in [
        "cursor_id",
        "cursor_index",
        "cursor_scan",
        "and_index_last",
        "and_index_first",
        "top_k",
        "full_sort",
    ] {
        let repeats = if phase == "top_k" || phase == "full_sort" {
            10
        } else {
            queries
        };
        let before_spills = db.query_memory_stats();
        let before_allocs = ALLOCS.load(Ordering::Relaxed);
        let before_bytes = BYTES.load(Ordering::Relaxed);
        let mut times = Vec::new();
        for iteration in 0..repeats {
            let n = (iteration * 7919 + count / 2) % count;
            let sql = match phase {
                "cursor_id" => format!("SELECT n FROM items WHERE id = 'r{n:08}'"),
                "cursor_index" => format!("SELECT n FROM items WHERE n = {n}"),
                "cursor_scan" => format!("SELECT n FROM items WHERE n = {n} OR n = {n}"),
                "and_index_last" => {
                    format!("SELECT n FROM items WHERE payload = 'review fixture' AND n = {n}")
                }
                "and_index_first" => {
                    format!("SELECT n FROM items WHERE n = {n} AND payload = 'review fixture'")
                }
                "top_k" => "SELECT n FROM items ORDER BY n DESC LIMIT 10".into(),
                _ => "SELECT n FROM items ORDER BY n DESC".into(),
            };
            let begin = Instant::now();
            if phase == "top_k" || phase == "full_sort" {
                let QueryOutput::Rows { rows, .. } = db.query(&sql).unwrap() else {
                    panic!("expected rows");
                };
                assert_eq!(rows[0][0], Value::Int64(count as i64 - 1));
                assert_eq!(rows.len(), if phase == "top_k" { 10 } else { count });
                std::hint::black_box(rows);
            } else if phase.starts_with("and_index_") {
                let QueryOutput::Rows { rows, .. } = db.query(&sql).unwrap() else {
                    panic!("expected rows");
                };
                assert_eq!(rows, vec![vec![Value::Int64(n as i64)]]);
            } else {
                let mut cursor = db.query_cursor(&sql).unwrap();
                assert_eq!(
                    cursor.next().unwrap().unwrap(),
                    vec![Value::Int64(n as i64)]
                );
                assert!(cursor.next().is_none());
            }
            times.push(begin.elapsed().as_nanos() as u64);
        }
        let allocations = ALLOCS.load(Ordering::Relaxed) - before_allocs;
        let allocated_bytes = BYTES.load(Ordering::Relaxed) - before_bytes;
        times.sort_unstable();
        let stats = db.query_memory_stats();
        println!(
            "{}",
            json!({"phase":phase,"rows":count,"operations":repeats,"p50_ns":percentile(&times,50),"p95_ns":percentile(&times,95),"p99_ns":percentile(&times,99),"allocations":allocations,"allocated_bytes":allocated_bytes,"spill_files":stats.spill_files-before_spills.spill_files,"spilled_bytes":stats.spilled_bytes-before_spills.spilled_bytes})
        );
    }
    for profile in ["plain", "identity", "foreign_key"] {
        let sql = match profile {
            "plain" => "CREATE TABLE plain(n int)",
            "identity" => "CREATE TABLE identity(id int AUTO_INCREMENT PRIMARY KEY,n int)",
            _ => "CREATE TABLE foreign_key(n int REFERENCES items(n))",
        };
        if profile == "foreign_key" {
            db.query("DROP INDEX ON items(n)").unwrap();
            db.query("CREATE UNIQUE INDEX ON items(n)").unwrap();
        }
        db.query(sql).unwrap();
        let before = db.maintenance_stats();
        let before_allocations = ALLOCS.load(Ordering::Relaxed);
        let mut samples = Vec::new();
        for n in 0..200 {
            let begin = Instant::now();
            let mut row = Record::new();
            row.insert("n".into(), Value::Int64(n));
            db.insert(profile, row).unwrap();
            samples.push(begin.elapsed().as_nanos() as u64);
        }
        let allocations = ALLOCS.load(Ordering::Relaxed) - before_allocations;
        let after = db.maintenance_stats();
        samples.sort_unstable();
        println!(
            "{}",
            json!({"phase":format!("commit_{profile}"),"operations":200,"durability":"fast","p50_ns":percentile(&samples,50),"p95_ns":percentile(&samples,95),"p99_ns":percentile(&samples,99),"allocations":allocations,"prepare_ns":(after.commit_prepare_time-before.commit_prepare_time).as_nanos(),"locked_prepare_ns":(after.commit_locked_prepare_time-before.commit_locked_prepare_time).as_nanos(),"lock_wait_ns":(after.commit_lock_wait_time-before.commit_lock_wait_time).as_nanos(),"lock_hold_ns":(after.commit_lock_hold_time-before.commit_lock_hold_time).as_nanos(),"wal_ns":(after.commit_wal_time-before.commit_wal_time).as_nanos(),"commit_ns":(after.commit_time-before.commit_time).as_nanos(),"apply_ns":(after.commit_apply_time-before.commit_apply_time).as_nanos(),"wal_append_ns":(after.commit_wal_append_time-before.commit_wal_append_time).as_nanos(),"sync_ns":(after.wal_sync_time-before.wal_sync_time).as_nanos(),"debt_operations":after.compaction_debt_operations})
        );
    }
}
