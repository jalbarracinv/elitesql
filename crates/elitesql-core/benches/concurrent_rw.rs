//! Concurrent-reader and mixed read/write throughput benchmark.
//!
//! The fixture is bulk-loaded and checkpointed before timing so reads use the
//! persisted mmap/segment path. A `writers=0` point isolates reader scaling;
//! positive writer counts measure the same reads while disjoint transactions
//! commit concurrently.
//!
//! Run:
//!   cargo bench -p elitesql-core --bench concurrent_rw
//!   cargo bench -p elitesql-core --bench concurrent_rw -- --smoke

use std::env;
use std::error::Error;
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Barrier};
use std::time::{Duration, Instant};

use elitesql_core::{
    AutoCompactionOptions, Column, ColumnType, Db, DbOptions, Durability, MemoryOptions, Record,
    TableSchema, Value,
};

const BODY: &str = "Concurrent persisted-read benchmark payload.";

#[derive(Debug)]
struct Config {
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
            readers: vec![1, 2, 4, 8, 16],
            writers: vec![0, 1, 4],
            rows: 100_000,
            read_operations: 1_000_000,
            write_rows: 40_000,
            batch_size: 10,
            repetitions: 3,
            csv: workspace_root().join("benchmark-results/concurrent-rw.csv"),
        }
    }
}

impl Config {
    fn parse() -> Result<Option<Self>, String> {
        let mut config = Self::default();
        let mut args = env::args().skip(1);
        while let Some(arg) = args.next() {
            match arg.as_str() {
                "--readers" => config.readers = parse_list(&required(&mut args, &arg)?, false)?,
                "--writers" => config.writers = parse_list(&required(&mut args, &arg)?, true)?,
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
                    config.readers = vec![1, 4, 8];
                    config.writers = vec![0, 1];
                    config.rows = 10_000;
                    config.read_operations = 20_000;
                    config.write_rows = 2_000;
                    config.repetitions = 1;
                }
                "--bench" => {}
                "-h" | "--help" => return Ok(None),
                _ => return Err(format!("unknown argument '{arg}'\n\n{}", usage())),
            }
        }
        if config.readers.is_empty()
            || config.writers.is_empty()
            || config.readers.contains(&0)
            || config.rows == 0
            || config.read_operations == 0
            || config.write_rows == 0
            || config.batch_size == 0
            || config.repetitions == 0
        {
            return Err(
                "reader counts and workload sizes must be positive; only writers may include zero"
                    .into(),
            );
        }
        Ok(Some(config))
    }
}

fn usage() -> &'static str {
    "Concurrent persisted readers and mixed read/write benchmark\n\
     \n\
     Options:\n\
       --readers LIST       Reader thread counts [default: 1,2,4,8,16]\n\
       --writers LIST       Writer counts; zero means read-only [default: 0,1,4]\n\
       --rows N             Persisted fixture rows [default: 100k]\n\
       --read-operations N  Total point reads per run [default: 1m]\n\
       --write-rows N       Total inserted rows in mixed runs [default: 40k]\n\
       --batch-size N       Rows per write transaction [default: 10]\n\
       --repetitions N      Fresh repetitions [default: 3]\n\
       --csv PATH           Structured output path\n\
       --smoke              Small correctness/performance smoke matrix"
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

fn parse_list(value: &str, allow_zero: bool) -> Result<Vec<usize>, String> {
    let mut values = value
        .split(',')
        .map(|part| parse_count(part.trim()))
        .collect::<Result<Vec<_>, _>>()?;
    values.sort_unstable();
    values.dedup();
    if values.is_empty() || (!allow_zero && values.contains(&0)) {
        return Err(format!("invalid count list '{value}'"));
    }
    Ok(values)
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

fn write_record(writer: usize, row: usize) -> Record {
    let mut record = Record::new();
    record.insert(
        "id".into(),
        Value::Text(format!("write-{writer:04}-{row:08}")),
    );
    record.insert("value".into(), Value::Int64(row as i64));
    record.insert("body".into(), Value::Text(BODY.into()));
    record
}

fn share(total: usize, workers: usize, worker: usize) -> usize {
    total / workers + usize::from(worker < total % workers)
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

fn duration_ns(duration: Duration) -> u64 {
    duration.as_nanos().min(u64::MAX as u128) as u64
}

struct ResultRow {
    readers: usize,
    writers: usize,
    repetition: usize,
    elapsed: Duration,
    reads: usize,
    written: usize,
    read_latencies: Vec<u64>,
    write_latencies: Vec<u64>,
    validation_scan: Duration,
    query_waits: u64,
    point_read_throttles: u64,
    coordinated_batches: u64,
    coordinated_commits: u64,
    lock_wait_us: f64,
    lock_hold_us: f64,
}

fn run(
    config: &Config,
    readers: usize,
    writers: usize,
    repetition: usize,
) -> Result<ResultRow, String> {
    let dir = tempfile::tempdir().map_err(|error| error.to_string())?;
    let delta_bytes = config.write_rows.saturating_mul(384).max(24 * 1024 * 1024);
    let query_bytes = 64 * 1024 * 1024;
    let reserve_bytes = 8 * 1024 * 1024;
    let db = Db::create_with(
        dir.path().join("concurrent-rw.esql"),
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
        },
    )
    .map_err(|error| error.to_string())?;
    db.create_table(TableSchema::new(
        "docs",
        vec![
            Column::new("value", ColumnType::Int64).not_null(),
            Column::new("body", ColumnType::Text).not_null(),
        ],
    ))
    .map_err(|error| error.to_string())?;
    db.bulk_insert_sorted("docs", (0..config.rows).map(base_record))
        .map_err(|error| error.to_string())?;

    // Warm every worker's first lookup before timing page faults or setup.
    for reader in 0..readers {
        let row = (reader.wrapping_mul(7_919)) % config.rows;
        db.get("docs", &base_id(row))
            .map_err(|error| error.to_string())?
            .ok_or_else(|| format!("warmup row {row} missing"))?;
    }
    let memory_before = db.global_memory_stats();
    let maintenance_before = db.maintenance_stats();

    let db = Arc::new(db);
    let barrier = Arc::new(Barrier::new(readers + writers + 1));
    let mut reader_handles = Vec::with_capacity(readers);
    for reader in 0..readers {
        let db = Arc::clone(&db);
        let barrier = Arc::clone(&barrier);
        let operations = share(config.read_operations, readers, reader);
        let rows = config.rows;
        reader_handles.push(std::thread::spawn(move || -> Result<Vec<u64>, String> {
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
                    return Err(format!("reader row {row} returned the wrong value"));
                }
                latencies.push(duration_ns(started.elapsed()));
            }
            Ok(latencies)
        }));
    }

    let mut writer_handles = Vec::with_capacity(writers);
    for writer in 0..writers {
        let db = Arc::clone(&db);
        let barrier = Arc::clone(&barrier);
        let rows = share(config.write_rows, writers, writer);
        let batch_size = config.batch_size;
        writer_handles.push(std::thread::spawn(move || -> Result<Vec<u64>, String> {
            let mut latencies = Vec::with_capacity(rows.div_ceil(batch_size));
            barrier.wait();
            for start in (0..rows).step_by(batch_size) {
                let end = (start + batch_size).min(rows);
                let started = Instant::now();
                let mut transaction = db.begin();
                for row in start..end {
                    transaction
                        .insert("docs", write_record(writer, row))
                        .map_err(|error| error.to_string())?;
                }
                transaction.commit().map_err(|error| error.to_string())?;
                latencies.push(duration_ns(started.elapsed()));
            }
            Ok(latencies)
        }));
    }

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
    let mut write_latencies = Vec::new();
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

    let validation_scan_started = Instant::now();
    let found = db.scan("docs").map_err(|error| error.to_string())?.len();
    let validation_scan = validation_scan_started.elapsed();
    let written = if writers == 0 { 0 } else { config.write_rows };
    if found != config.rows + written {
        return Err(format!(
            "validation found {found} rows, expected {}",
            config.rows + written
        ));
    }
    let memory_after = db.global_memory_stats();
    let maintenance_after = db.maintenance_stats();
    let commits = maintenance_after
        .commits
        .saturating_sub(maintenance_before.commits)
        .max(1) as f64;
    Ok(ResultRow {
        readers,
        writers,
        repetition,
        elapsed,
        reads: read_latencies.len(),
        written,
        read_latencies,
        write_latencies,
        validation_scan,
        query_waits: memory_after
            .query_waits
            .saturating_sub(memory_before.query_waits),
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
    })
}

fn print_result(result: &ResultRow) {
    println!(
        "  readers={:<2} writers={:<2} run={} reads={:>10.0}/s p50={:>7.2} us p95={:>8.2} us p99={:>8.2} us writes={:>9.0}/s wp99={:>8.2} us scan={:.3} s query_waits={} read_throttles={} coordinated={}/{} lock(wait/hold)={:.1}/{:.1} us",
        result.readers,
        result.writers,
        result.repetition,
        result.reads as f64 / result.elapsed.as_secs_f64(),
        percentile_us(&result.read_latencies, 50),
        percentile_us(&result.read_latencies, 95),
        percentile_us(&result.read_latencies, 99),
        result.written as f64 / result.elapsed.as_secs_f64(),
        percentile_us(&result.write_latencies, 99),
        result.validation_scan.as_secs_f64(),
        result.query_waits,
        result.point_read_throttles,
        result.coordinated_commits,
        result.coordinated_batches,
        result.lock_wait_us,
        result.lock_hold_us,
    );
}

fn csv(results: &[ResultRow], config: &Config) -> String {
    let mut output = String::from(
        "readers,writers,repetition,fixture_rows,read_operations,write_rows,batch_size,elapsed_seconds,reads_per_second,read_p50_us,read_p95_us,read_p99_us,read_max_us,writes_per_second,write_p50_us,write_p95_us,write_p99_us,write_max_us,validation_scan_seconds,query_waits,point_read_throttles,coordinated_batches,coordinated_commits,lock_wait_us,lock_hold_us\n",
    );
    for result in results {
        output.push_str(&format!(
            "{},{},{},{},{},{},{},{:.9},{:.3},{:.3},{:.3},{:.3},{:.3},{:.3},{:.3},{:.3},{:.3},{:.3},{:.9},{},{},{},{},{:.3},{:.3}\n",
            result.readers,
            result.writers,
            result.repetition,
            config.rows,
            result.reads,
            result.written,
            config.batch_size,
            result.elapsed.as_secs_f64(),
            result.reads as f64 / result.elapsed.as_secs_f64(),
            percentile_us(&result.read_latencies, 50),
            percentile_us(&result.read_latencies, 95),
            percentile_us(&result.read_latencies, 99),
            result.read_latencies.last().copied().unwrap_or(0) as f64 / 1_000.0,
            result.written as f64 / result.elapsed.as_secs_f64(),
            percentile_us(&result.write_latencies, 50),
            percentile_us(&result.write_latencies, 95),
            percentile_us(&result.write_latencies, 99),
            result.write_latencies.last().copied().unwrap_or(0) as f64 / 1_000.0,
            result.validation_scan.as_secs_f64(),
            result.query_waits,
            result.point_read_throttles,
            result.coordinated_batches,
            result.coordinated_commits,
            result.lock_wait_us,
            result.lock_hold_us,
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
    println!("EliteSQL concurrent persisted readers and mixed workload");
    println!(
        "  fixture={} reads={} mixed_write_rows={} batch={} readers={:?} writers={:?} repetitions={}",
        config.rows,
        config.read_operations,
        config.write_rows,
        config.batch_size,
        config.readers,
        config.writers,
        config.repetitions,
    );

    let mut results = Vec::new();
    for &writers in &config.writers {
        for &readers in &config.readers {
            for repetition in 1..=config.repetitions {
                let result = run(&config, readers, writers, repetition)?;
                print_result(&result);
                results.push(result);
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
