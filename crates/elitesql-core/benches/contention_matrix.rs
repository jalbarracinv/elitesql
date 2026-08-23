//! Mixed reader/writer matrix for commit shapes that do not share the same
//! validation or derived-index costs.
//!
//! Profiles: inserts, updates, deletes, identity allocation, foreign-key
//! validation, and synchronous equality/BM25/HNSW maintenance. `cold` closes
//! and reopens the database, skips warmups, and asks the OS to discard clean
//! pages with POSIX_FADV_DONTNEED before timing.

use std::env;
use std::error::Error;
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Barrier};
use std::time::{Duration, Instant};

#[cfg(any(target_os = "linux", target_os = "android"))]
use std::os::fd::AsRawFd;

use elitesql_core::{
    AutoCompactionOptions, Column, ColumnType, Db, DbOptions, Durability, ForeignKeyDef,
    IndexingMode, MemoryOptions, Record, ReferentialAction, TableSchema, Value, VectorIndexOptions,
    VectorSearchOptions,
};

const BODY: &str = "contention matrix persisted reader payload";
const VECTOR_DIM: usize = 8;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Workload {
    Insert,
    Update,
    Delete,
    Identity,
    ForeignKey,
    Derived,
}

impl Workload {
    const ALL: [Self; 6] = [
        Self::Insert,
        Self::Update,
        Self::Delete,
        Self::Identity,
        Self::ForeignKey,
        Self::Derived,
    ];

    fn parse(value: &str) -> Result<Vec<Self>, String> {
        parse_named_list(value, |name| match name {
            "insert" => Some(Self::Insert),
            "update" => Some(Self::Update),
            "delete" => Some(Self::Delete),
            "identity" => Some(Self::Identity),
            "foreign-key" | "foreign_key" | "fk" => Some(Self::ForeignKey),
            "derived" | "indexes" => Some(Self::Derived),
            _ => None,
        })
    }

    fn name(self) -> &'static str {
        match self {
            Self::Insert => "insert",
            Self::Update => "update",
            Self::Delete => "delete",
            Self::Identity => "identity",
            Self::ForeignKey => "foreign-key",
            Self::Derived => "derived",
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum CacheMode {
    Warm,
    Cold,
}

impl CacheMode {
    fn parse(value: &str) -> Result<Vec<Self>, String> {
        parse_named_list(value, |name| match name {
            "warm" => Some(Self::Warm),
            "cold" => Some(Self::Cold),
            _ => None,
        })
    }

    fn name(self) -> &'static str {
        match self {
            Self::Warm => "warm",
            Self::Cold => "cold",
        }
    }
}

fn parse_named_list<T: Copy + PartialEq>(
    value: &str,
    parse: impl Fn(&str) -> Option<T>,
) -> Result<Vec<T>, String> {
    let mut out = Vec::new();
    for raw in value.split(',') {
        let name = raw.trim().to_ascii_lowercase();
        let parsed = parse(&name).ok_or_else(|| format!("unknown matrix value '{raw}'"))?;
        if !out.contains(&parsed) {
            out.push(parsed);
        }
    }
    if out.is_empty() {
        return Err("matrix list cannot be empty".into());
    }
    Ok(out)
}

#[derive(Debug)]
struct Config {
    workloads: Vec<Workload>,
    caches: Vec<CacheMode>,
    readers: Vec<usize>,
    writers: Vec<usize>,
    rows: usize,
    read_operations: usize,
    write_rows: usize,
    batch_size: usize,
    repetitions: usize,
    csv: PathBuf,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            workloads: Workload::ALL.to_vec(),
            caches: vec![CacheMode::Warm, CacheMode::Cold],
            readers: vec![16],
            writers: vec![4],
            rows: 50_000,
            read_operations: 100_000,
            write_rows: 5_000,
            batch_size: 10,
            repetitions: 3,
            csv: workspace_root().join("benchmark-results/contention-matrix.csv"),
        }
    }
}

impl Config {
    fn parse() -> Result<Option<Self>, String> {
        let mut config = Self::default();
        let mut args = env::args().skip(1);
        while let Some(arg) = args.next() {
            match arg.as_str() {
                "--workloads" => config.workloads = Workload::parse(&required(&mut args, &arg)?)?,
                "--cache" => config.caches = CacheMode::parse(&required(&mut args, &arg)?)?,
                "--readers" => config.readers = parse_counts(&required(&mut args, &arg)?)?,
                "--writers" => config.writers = parse_counts(&required(&mut args, &arg)?)?,
                "--rows" => config.rows = parse_count(&required(&mut args, &arg)?)?,
                "--read-operations" => {
                    config.read_operations = parse_count(&required(&mut args, &arg)?)?
                }
                "--write-rows" => config.write_rows = parse_count(&required(&mut args, &arg)?)?,
                "--batch-size" => config.batch_size = parse_count(&required(&mut args, &arg)?)?,
                "--repetitions" => config.repetitions = parse_count(&required(&mut args, &arg)?)?,
                "--csv" => {
                    let path = PathBuf::from(required(&mut args, &arg)?);
                    config.csv = if path.is_absolute() {
                        path
                    } else {
                        workspace_root().join(path)
                    };
                }
                "--smoke" => {
                    config.rows = 2_000;
                    config.read_operations = 5_000;
                    config.write_rows = 200;
                    config.batch_size = 10;
                    config.repetitions = 1;
                    config.readers = vec![4];
                    config.writers = vec![2];
                    config.caches = vec![CacheMode::Warm];
                }
                "--bench" => {}
                "-h" | "--help" => return Ok(None),
                _ => return Err(format!("unknown argument '{arg}'\n\n{}", usage())),
            }
        }
        if config.readers.contains(&0)
            || config.writers.contains(&0)
            || config.rows == 0
            || config.read_operations == 0
            || config.write_rows == 0
            || config.batch_size == 0
            || config.repetitions == 0
        {
            return Err("thread counts and workload sizes must be positive".into());
        }
        Ok(Some(config))
    }
}

fn usage() -> &'static str {
    "EliteSQL mixed contention matrix\n\
     \n\
     Options:\n\
       --workloads LIST     insert,update,delete,identity,foreign-key,derived\n\
       --cache LIST         warm,cold [default: both]\n\
       --readers LIST       Point-reader thread counts [default: 16]\n\
       --writers LIST       Writer thread counts [default: 4]\n\
       --rows N             Persisted reader rows [default: 50k]\n\
       --read-operations N  Point reads per run [default: 100k]\n\
       --write-rows N       Mutations per run [default: 5k]\n\
       --batch-size N       Mutations per transaction [default: 10]\n\
       --repetitions N      Fresh runs [default: 3]\n\
       --csv PATH           Structured output path\n\
       --smoke              All profiles, warm cache, small fixture"
}

fn workspace_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../..")
        .to_path_buf()
}

fn required(args: &mut impl Iterator<Item = String>, flag: &str) -> Result<String, String> {
    args.next()
        .ok_or_else(|| format!("missing value after {flag}"))
}

fn parse_count(value: &str) -> Result<usize, String> {
    let value = value.replace('_', "").to_ascii_lowercase();
    let (digits, multiplier) = match value.as_bytes().last() {
        Some(b'k') => (&value[..value.len() - 1], 1_000usize),
        Some(b'm') => (&value[..value.len() - 1], 1_000_000usize),
        _ => (value.as_str(), 1usize),
    };
    digits
        .parse::<usize>()
        .map_err(|_| format!("invalid count '{value}'"))?
        .checked_mul(multiplier)
        .ok_or_else(|| format!("count '{value}' is too large"))
}

fn parse_counts(value: &str) -> Result<Vec<usize>, String> {
    let mut out = value
        .split(',')
        .map(|part| parse_count(part.trim()))
        .collect::<Result<Vec<_>, _>>()?;
    out.sort_unstable();
    out.dedup();
    Ok(out)
}

fn base_id(row: usize) -> String {
    format!("base-{row:08}")
}

fn base_record(row: usize) -> Record {
    let mut record = Record::new();
    record.insert("id".into(), Value::Text(base_id(row)));
    record.insert("value".into(), Value::Int64(row as i64));
    record.insert("body".into(), Value::Text(BODY.into()));
    record
}

fn work_id(writer: usize, row: usize) -> String {
    format!("work-{writer:04}-{row:08}")
}

fn work_record(writer: usize, row: usize) -> Record {
    let mut record = Record::new();
    record.insert("id".into(), Value::Text(work_id(writer, row)));
    record.insert("writer".into(), Value::Int64(writer as i64));
    record.insert("value".into(), Value::Int64(row as i64));
    record.insert("body".into(), Value::Text(BODY.into()));
    record
}

fn derived_record(writer: usize, row: usize) -> Record {
    let mut record = work_record(writer, row);
    record.insert(
        "body".into(),
        Value::Text(format!("derived searchable writer {writer} row {row}")),
    );
    let mut vector = vec![0.0f32; VECTOR_DIM];
    vector[writer % VECTOR_DIM] = 1.0;
    vector[(row + 3) % VECTOR_DIM] += 0.25;
    record.insert("embedding".into(), Value::Vector(vector));
    record
}

fn share(total: usize, workers: usize, worker: usize) -> usize {
    total / workers + usize::from(worker < total % workers)
}

fn duration_ns(duration: Duration) -> u64 {
    duration.as_nanos().min(u64::MAX as u128) as u64
}

fn percentile_us(sorted: &[u64], percentile: usize) -> f64 {
    if sorted.is_empty() {
        return 0.0;
    }
    let index = (sorted.len() * percentile)
        .div_ceil(100)
        .saturating_sub(1)
        .min(sorted.len() - 1);
    sorted[index] as f64 / 1_000.0
}

fn db_options(config: &Config) -> DbOptions {
    let delta_bytes = config
        .write_rows
        .saturating_mul(2_048)
        .max(32 * 1024 * 1024);
    let query_bytes = 64 * 1024 * 1024;
    let reserve_bytes = 8 * 1024 * 1024;
    DbOptions {
        durability: Durability::Fast,
        memtable_max_bytes: u64::MAX,
        memory: MemoryOptions {
            total_memory_bytes: query_bytes + delta_bytes * 2 + reserve_bytes,
            query_pool_bytes: query_bytes,
            index_delta_pool_bytes: delta_bytes,
            maintenance_pool_bytes: delta_bytes,
            reserved_memory_bytes: reserve_bytes,
            ..MemoryOptions::default()
        },
        auto_compaction: AutoCompactionOptions::disabled(),
        ..DbOptions::default()
    }
}

fn create_workload_fixture(
    db: &Db,
    workload: Workload,
    writers: usize,
    write_rows: usize,
) -> Result<(), String> {
    match workload {
        Workload::Insert => {}
        Workload::Update | Workload::Delete => {
            db.create_table(TableSchema::new(
                "work",
                vec![
                    Column::new("writer", ColumnType::Int64).not_null(),
                    Column::new("value", ColumnType::Int64).not_null(),
                    Column::new("body", ColumnType::Text).not_null(),
                ],
            ))
            .map_err(|error| error.to_string())?;
            let rows = (0..writers).flat_map(|writer| {
                (0..share(write_rows, writers, writer)).map(move |row| work_record(writer, row))
            });
            db.bulk_insert_sorted("work", rows)
                .map_err(|error| error.to_string())?;
        }
        Workload::Identity => {
            db.create_table(TableSchema::new(
                "work",
                vec![
                    Column::new("sequence", ColumnType::Int64).identity(),
                    Column::new("writer", ColumnType::Int64).not_null(),
                    Column::new("value", ColumnType::Int64).not_null(),
                    Column::new("body", ColumnType::Text).not_null(),
                ],
            ))
            .map_err(|error| error.to_string())?;
        }
        Workload::ForeignKey => {
            db.create_table(TableSchema::new(
                "parents",
                vec![Column::new("writer", ColumnType::Int64).not_null()],
            ))
            .map_err(|error| error.to_string())?;
            let parents = (0..writers).map(|writer| {
                let mut record = Record::new();
                record.insert("id".into(), Value::Text(format!("parent-{writer:04}")));
                record.insert("writer".into(), Value::Int64(writer as i64));
                record
            });
            db.bulk_insert_sorted("parents", parents)
                .map_err(|error| error.to_string())?;
            let mut child = TableSchema::new(
                "work",
                vec![
                    Column::new("parent_id", ColumnType::Text).not_null(),
                    Column::new("writer", ColumnType::Int64).not_null(),
                    Column::new("value", ColumnType::Int64).not_null(),
                ],
            );
            child.foreign_keys.push(ForeignKeyDef {
                column: "parent_id".into(),
                referenced_table: "parents".into(),
                referenced_column: "id".into(),
                on_delete: ReferentialAction::Restrict,
            });
            db.create_table(child).map_err(|error| error.to_string())?;
        }
        Workload::Derived => {
            db.create_table(TableSchema::new(
                "work",
                vec![
                    Column::new("writer", ColumnType::Int64).not_null(),
                    Column::new("value", ColumnType::Int64).not_null(),
                    Column::new("body", ColumnType::Text).not_null(),
                    Column::vector("embedding", VECTOR_DIM).not_null(),
                ],
            ))
            .map_err(|error| error.to_string())?;
            db.create_index("work", "value", false)
                .map_err(|error| error.to_string())?;
            db.create_text_index("work", "body")
                .map_err(|error| error.to_string())?;
            db.create_vector_index(
                "work",
                "embedding",
                VectorIndexOptions {
                    mode: IndexingMode::Sync,
                    m: 8,
                    ef_construction: 64,
                    ..VectorIndexOptions::default()
                },
            )
            .map_err(|error| error.to_string())?;
        }
    }
    Ok(())
}

#[cfg(any(target_os = "linux", target_os = "android"))]
fn evict_database_files(root: &Path) -> (usize, usize) {
    let mut attempted = 0usize;
    let mut succeeded = 0usize;
    let mut pending = vec![root.to_path_buf()];
    while let Some(path) = pending.pop() {
        let Ok(metadata) = fs::metadata(&path) else {
            continue;
        };
        if metadata.is_dir() {
            if let Ok(entries) = fs::read_dir(path) {
                pending.extend(entries.flatten().map(|entry| entry.path()));
            }
            continue;
        }
        let Ok(file) = fs::File::open(&path) else {
            continue;
        };
        attempted += 1;
        // SAFETY: the descriptor is valid for this call and no pointer is
        // passed. Failure only means this run is less cold than requested.
        let result =
            unsafe { libc::posix_fadvise(file.as_raw_fd(), 0, 0, libc::POSIX_FADV_DONTNEED) };
        succeeded += usize::from(result == 0);
    }
    (attempted, succeeded)
}

#[cfg(not(any(target_os = "linux", target_os = "android")))]
fn evict_database_files(_root: &Path) -> (usize, usize) {
    (0, 0)
}

struct ResultRow {
    workload: Workload,
    cache: CacheMode,
    readers: usize,
    writers: usize,
    repetition: usize,
    elapsed: Duration,
    read_latencies: Vec<u64>,
    write_latencies: Vec<u64>,
    read_operations: usize,
    write_rows: usize,
    point_read_throttles: u64,
    coordinated_batches: u64,
    coordinated_commits: u64,
    lock_wait_us: f64,
    lock_hold_us: f64,
    evict_attempted: usize,
    evict_succeeded: usize,
}

fn run(
    config: &Config,
    workload: Workload,
    cache: CacheMode,
    readers: usize,
    writers: usize,
    repetition: usize,
) -> Result<ResultRow, String> {
    let dir = tempfile::tempdir().map_err(|error| error.to_string())?;
    let path = dir.path().join("contention.esql");
    let options = db_options(config);
    let mut db = Db::create_with(&path, options.clone()).map_err(|error| error.to_string())?;
    db.create_table(TableSchema::new(
        "docs",
        vec![
            Column::new("writer", ColumnType::Int64),
            Column::new("value", ColumnType::Int64).not_null(),
            Column::new("body", ColumnType::Text).not_null(),
        ],
    ))
    .map_err(|error| error.to_string())?;
    db.bulk_insert_sorted("docs", (0..config.rows).map(base_record))
        .map_err(|error| error.to_string())?;
    create_workload_fixture(&db, workload, writers, config.write_rows)?;
    db.checkpoint().map_err(|error| error.to_string())?;

    let (evict_attempted, evict_succeeded) = match cache {
        CacheMode::Warm => {
            for reader in 0..readers {
                let row = reader.wrapping_mul(7_919) % config.rows;
                db.get("docs", &base_id(row))
                    .map_err(|error| error.to_string())?
                    .ok_or_else(|| format!("warmup row {row} missing"))?;
            }
            (0, 0)
        }
        CacheMode::Cold => {
            drop(db);
            let before = evict_database_files(&path);
            db = Db::open_with(&path, options).map_err(|error| error.to_string())?;
            let after = evict_database_files(&path);
            (before.0 + after.0, before.1 + after.1)
        }
    };

    let memory_before = db.global_memory_stats();
    let maintenance_before = db.maintenance_stats();
    let db = Arc::new(db);
    let barrier = Arc::new(Barrier::new(readers + writers + 1));

    let reader_handles = (0..readers)
        .map(|reader| {
            let db = db.clone();
            let barrier = barrier.clone();
            let operations = share(config.read_operations, readers, reader);
            let rows = config.rows;
            std::thread::spawn(move || -> Result<Vec<u64>, String> {
                let mut latencies = Vec::with_capacity(operations);
                let mut row = reader.wrapping_mul(104_729) % rows;
                barrier.wait();
                for _ in 0..operations {
                    row = (row + 7_919) % rows;
                    let started = Instant::now();
                    let record = db
                        .get("docs", &base_id(row))
                        .map_err(|error| error.to_string())?
                        .ok_or_else(|| format!("reader row {row} missing"))?;
                    if record.get("value") != Some(&Value::Int64(row as i64)) {
                        return Err(format!("reader row {row} returned wrong value"));
                    }
                    latencies.push(duration_ns(started.elapsed()));
                }
                Ok(latencies)
            })
        })
        .collect::<Vec<_>>();

    let writer_handles = (0..writers)
        .map(|writer| {
            let db = db.clone();
            let barrier = barrier.clone();
            let rows = share(config.write_rows, writers, writer);
            let batch_size = config.batch_size;
            std::thread::spawn(move || -> Result<Vec<u64>, String> {
                let mut latencies = Vec::with_capacity(rows.div_ceil(batch_size));
                barrier.wait();
                for start in (0..rows).step_by(batch_size) {
                    let end = (start + batch_size).min(rows);
                    let started = Instant::now();
                    let mut transaction = db.begin();
                    for row in start..end {
                        match workload {
                            Workload::Insert => {
                                transaction
                                    .insert("docs", work_record(writer, row))
                                    .map_err(|error| error.to_string())?;
                            }
                            Workload::Update => {
                                let mut patch = Record::new();
                                patch.insert("value".into(), Value::Int64(-(row as i64) - 1));
                                transaction
                                    .update("work", &work_id(writer, row), patch)
                                    .map_err(|error| error.to_string())?;
                            }
                            Workload::Delete => {
                                if !transaction
                                    .delete("work", &work_id(writer, row))
                                    .map_err(|error| error.to_string())?
                                {
                                    return Err(format!("delete target {writer}/{row} missing"));
                                }
                            }
                            Workload::Identity => {
                                let mut record = Record::new();
                                record.insert("writer".into(), Value::Int64(writer as i64));
                                record.insert("value".into(), Value::Int64(row as i64));
                                record.insert("body".into(), Value::Text(BODY.into()));
                                transaction
                                    .insert("work", record)
                                    .map_err(|error| error.to_string())?;
                            }
                            Workload::ForeignKey => {
                                let mut record = Record::new();
                                record.insert("id".into(), Value::Text(work_id(writer, row)));
                                record.insert(
                                    "parent_id".into(),
                                    Value::Text(format!("parent-{writer:04}")),
                                );
                                record.insert("writer".into(), Value::Int64(writer as i64));
                                record.insert("value".into(), Value::Int64(row as i64));
                                transaction
                                    .insert("work", record)
                                    .map_err(|error| error.to_string())?;
                            }
                            Workload::Derived => {
                                transaction
                                    .insert("work", derived_record(writer, row))
                                    .map_err(|error| error.to_string())?;
                            }
                        }
                    }
                    transaction.commit().map_err(|error| error.to_string())?;
                    latencies.push(duration_ns(started.elapsed()));
                }
                Ok(latencies)
            })
        })
        .collect::<Vec<_>>();

    let started = Instant::now();
    barrier.wait();
    let mut read_latencies = Vec::with_capacity(config.read_operations);
    for handle in reader_handles {
        read_latencies.extend(
            handle
                .join()
                .map_err(|_| "reader thread panicked".to_owned())??,
        );
    }
    let mut write_latencies = Vec::with_capacity(config.write_rows.div_ceil(config.batch_size));
    for handle in writer_handles {
        write_latencies.extend(
            handle
                .join()
                .map_err(|_| "writer thread panicked".to_owned())??,
        );
    }
    let elapsed = started.elapsed();
    read_latencies.sort_unstable();
    write_latencies.sort_unstable();

    let expected_docs = config.rows
        + if workload == Workload::Insert {
            config.write_rows
        } else {
            0
        };
    let docs = db.scan("docs").map_err(|error| error.to_string())?.len();
    if docs != expected_docs {
        return Err(format!(
            "docs validation found {docs}, expected {expected_docs}"
        ));
    }
    if workload != Workload::Insert {
        if workload == Workload::Derived {
            db.wait_vector_indexing()
                .map_err(|error| error.to_string())?;
        }
        let work = db.scan("work").map_err(|error| error.to_string())?;
        let expected = if workload == Workload::Delete {
            0
        } else {
            config.write_rows
        };
        if work.len() != expected {
            return Err(format!(
                "work validation found {}, expected {expected}",
                work.len()
            ));
        }
        if workload == Workload::Update
            && work
                .iter()
                .any(|(_, record)| !matches!(record.get("value"), Some(Value::Int64(value)) if *value < 0))
        {
            return Err("an update row retained its old value".into());
        }
        if workload == Workload::Derived {
            if db
                .find_eq("work", "value", &Value::Int64(0))
                .map_err(|error| error.to_string())?
                .is_empty()
            {
                return Err("derived equality index returned no rows".into());
            }
            if db
                .search_text("work", "body", "searchable", 3, None)
                .map_err(|error| error.to_string())?
                .is_empty()
            {
                return Err("derived text index returned no rows".into());
            }
            let mut query = vec![0.0f32; VECTOR_DIM];
            query[0] = 1.0;
            if db
                .search_vector(
                    "work",
                    "embedding",
                    &query,
                    3,
                    &VectorSearchOptions::default(),
                )
                .map_err(|error| error.to_string())?
                .is_empty()
            {
                return Err("derived vector index returned no rows".into());
            }
        }
    }

    let memory_after = db.global_memory_stats();
    let maintenance_after = db.maintenance_stats();
    let commits = maintenance_after
        .commits
        .saturating_sub(maintenance_before.commits)
        .max(1) as f64;
    if memory_after
        .index_consolidations
        .saturating_sub(memory_before.index_consolidations)
        != 0
    {
        return Err("matrix consolidated index deltas inside the measured window".into());
    }

    Ok(ResultRow {
        workload,
        cache,
        readers,
        writers,
        repetition,
        elapsed,
        read_operations: read_latencies.len(),
        write_rows: config.write_rows,
        read_latencies,
        write_latencies,
        point_read_throttles: maintenance_after
            .point_read_throttles
            .saturating_sub(maintenance_before.point_read_throttles),
        coordinated_batches: maintenance_after
            .coordinated_batches
            .saturating_sub(maintenance_before.coordinated_batches),
        coordinated_commits: maintenance_after
            .coordinated_commits
            .saturating_sub(maintenance_before.coordinated_commits),
        lock_wait_us: maintenance_after
            .commit_lock_wait_time
            .saturating_sub(maintenance_before.commit_lock_wait_time)
            .as_secs_f64()
            * 1_000_000.0
            / commits,
        lock_hold_us: maintenance_after
            .commit_lock_hold_time
            .saturating_sub(maintenance_before.commit_lock_hold_time)
            .as_secs_f64()
            * 1_000_000.0
            / commits,
        evict_attempted,
        evict_succeeded,
    })
}

fn print_result(result: &ResultRow) {
    println!(
        "  {:<11} {:<4} readers={:<2} writers={:<2} run={} reads={:>9.0}/s rp99={:>8.1} us writes={:>8.0}/s wp50/p95/p99={:>7.1}/{:>8.1}/{:>8.1} us throttles={} coordinated={}/{} lock={:.1}/{:.1} us evict={}/{}",
        result.workload.name(),
        result.cache.name(),
        result.readers,
        result.writers,
        result.repetition,
        result.read_operations as f64 / result.elapsed.as_secs_f64(),
        percentile_us(&result.read_latencies, 99),
        result.write_rows as f64 / result.elapsed.as_secs_f64(),
        percentile_us(&result.write_latencies, 50),
        percentile_us(&result.write_latencies, 95),
        percentile_us(&result.write_latencies, 99),
        result.point_read_throttles,
        result.coordinated_commits,
        result.coordinated_batches,
        result.lock_wait_us,
        result.lock_hold_us,
        result.evict_succeeded,
        result.evict_attempted,
    );
}

fn csv(results: &[ResultRow], config: &Config) -> String {
    let mut output = String::from(
        "workload,cache,readers,writers,repetition,fixture_rows,read_operations,write_rows,batch_size,elapsed_seconds,reads_per_second,read_p50_us,read_p95_us,read_p99_us,read_max_us,writes_per_second,write_p50_us,write_p95_us,write_p99_us,write_max_us,point_read_throttles,coordinated_batches,coordinated_commits,lock_wait_us,lock_hold_us,evict_attempted,evict_succeeded\n",
    );
    for result in results {
        output.push_str(&format!(
            "{},{},{},{},{},{},{},{},{},{:.9},{:.3},{:.3},{:.3},{:.3},{:.3},{:.3},{:.3},{:.3},{:.3},{:.3},{},{},{},{:.3},{:.3},{},{}\n",
            result.workload.name(),
            result.cache.name(),
            result.readers,
            result.writers,
            result.repetition,
            config.rows,
            result.read_operations,
            result.write_rows,
            config.batch_size,
            result.elapsed.as_secs_f64(),
            result.read_operations as f64 / result.elapsed.as_secs_f64(),
            percentile_us(&result.read_latencies, 50),
            percentile_us(&result.read_latencies, 95),
            percentile_us(&result.read_latencies, 99),
            result.read_latencies.last().copied().unwrap_or(0) as f64 / 1_000.0,
            result.write_rows as f64 / result.elapsed.as_secs_f64(),
            percentile_us(&result.write_latencies, 50),
            percentile_us(&result.write_latencies, 95),
            percentile_us(&result.write_latencies, 99),
            result.write_latencies.last().copied().unwrap_or(0) as f64 / 1_000.0,
            result.point_read_throttles,
            result.coordinated_batches,
            result.coordinated_commits,
            result.lock_wait_us,
            result.lock_hold_us,
            result.evict_attempted,
            result.evict_succeeded,
        ));
    }
    output
}

fn main() -> Result<(), Box<dyn Error>> {
    let config = match Config::parse() {
        Ok(Some(config)) => config,
        Ok(None) => {
            println!("{}", usage());
            return Ok(());
        }
        Err(error) => return Err(error.into()),
    };
    println!("EliteSQL mixed reader/writer contention matrix");
    println!(
        "  workloads={:?} cache={:?} readers={:?} writers={:?} fixture={} reads={} writes={} batch={} repetitions={}",
        config.workloads,
        config.caches,
        config.readers,
        config.writers,
        config.rows,
        config.read_operations,
        config.write_rows,
        config.batch_size,
        config.repetitions,
    );
    #[cfg(not(any(target_os = "linux", target_os = "android")))]
    if config.caches.contains(&CacheMode::Cold) {
        println!(
            "  note: cold reopens without warmup; OS page-cache eviction is unsupported on this platform (evict=0/0)"
        );
    }

    let mut results = Vec::new();
    for &workload in &config.workloads {
        for &cache in &config.caches {
            for &writers in &config.writers {
                for &readers in &config.readers {
                    for repetition in 1..=config.repetitions {
                        let result = run(&config, workload, cache, readers, writers, repetition)?;
                        print_result(&result);
                        results.push(result);
                    }
                }
            }
        }
    }
    if let Some(parent) = config.csv.parent() {
        fs::create_dir_all(parent)?;
    }
    fs::write(&config.csv, csv(&results, &config))?;
    println!("\nCSV: {}", config.csv.display());
    Ok(())
}
