//! Concurrent-writer comparison between EliteSQL and SQLite.
//!
//! The measured window contains only small transactional writes. Automatic
//! checkpoints are disabled for SQLite and avoided in EliteSQL with a large
//! memtable; one final checkpoint is timed separately for each engine.
//!
//! Run:
//!   cargo bench -p elitesql-core --bench concurrent_writers
//!   cargo bench -p elitesql-core --bench concurrent_writers -- --smoke

use std::env;
use std::error::Error;
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Barrier};
use std::time::{Duration, Instant};

use elitesql_core::{
    Column, ColumnType, Db, DbOptions, Durability, MemoryOptions, Record, TableSchema, Value,
};
use rusqlite::{params, Connection};

const BODY: &str = "Concurrent writer benchmark payload.";

#[derive(Clone, Copy, Debug)]
enum DurabilityProfile {
    Fast,
    Balanced,
    Safe,
}

impl DurabilityProfile {
    fn parse(value: &str) -> Result<Self, String> {
        match value.to_ascii_lowercase().as_str() {
            "fast" => Ok(Self::Fast),
            "balanced" => Ok(Self::Balanced),
            "safe" => Ok(Self::Safe),
            _ => Err(format!(
                "unknown durability '{value}'; expected fast, balanced, or safe"
            )),
        }
    }

    fn elitesql(self) -> Durability {
        match self {
            Self::Fast => Durability::Fast,
            Self::Balanced => Durability::Balanced,
            Self::Safe => Durability::Safe,
        }
    }

    fn sqlite(self) -> &'static str {
        match self {
            Self::Fast => "OFF",
            Self::Balanced => "NORMAL",
            Self::Safe => "FULL",
        }
    }

    fn name(self) -> &'static str {
        match self {
            Self::Fast => "fast",
            Self::Balanced => "balanced",
            Self::Safe => "safe",
        }
    }
}

#[derive(Debug)]
struct Config {
    writers: Vec<usize>,
    total_rows: usize,
    batch_size: usize,
    repetitions: usize,
    durability: DurabilityProfile,
    csv: PathBuf,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            writers: vec![1, 2, 4, 8],
            total_rows: 200_000,
            batch_size: 10,
            repetitions: 3,
            durability: DurabilityProfile::Fast,
            csv: workspace_root().join("benchmark-results/concurrent-writers.csv"),
        }
    }
}

impl Config {
    fn parse() -> Result<Option<Self>, String> {
        let mut config = Self::default();
        let mut args = env::args().skip(1);
        while let Some(arg) = args.next() {
            match arg.as_str() {
                "--writers" => {
                    config.writers = parse_writers(&required_value(&mut args, "--writers")?)?;
                }
                "--rows" => {
                    config.total_rows = parse_count(&required_value(&mut args, "--rows")?)?;
                }
                "--batch-size" => {
                    config.batch_size = parse_count(&required_value(&mut args, "--batch-size")?)?;
                }
                "--repetitions" => {
                    config.repetitions = parse_count(&required_value(&mut args, "--repetitions")?)?;
                }
                "--durability" => {
                    config.durability =
                        DurabilityProfile::parse(&required_value(&mut args, "--durability")?)?;
                }
                "--csv" => {
                    let path = PathBuf::from(required_value(&mut args, "--csv")?);
                    config.csv = if path.is_absolute() {
                        path
                    } else {
                        workspace_root().join(path)
                    };
                }
                "--smoke" => {
                    config.total_rows = 4_000;
                    config.batch_size = 10;
                    config.repetitions = 1;
                }
                "--bench" => {}
                "-h" | "--help" => return Ok(None),
                _ => return Err(format!("unknown argument '{arg}'\n\n{}", usage())),
            }
        }
        if config.writers.is_empty() || config.writers.contains(&0) {
            return Err("--writers must contain positive integers".into());
        }
        if config.total_rows == 0 || config.batch_size == 0 || config.repetitions == 0 {
            return Err("--rows, --batch-size and --repetitions must be positive".into());
        }
        if config.total_rows < *config.writers.iter().max().expect("not empty") {
            return Err("--rows must be at least the largest writer count".into());
        }
        Ok(Some(config))
    }
}

fn workspace_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../..")
        .to_path_buf()
}

#[derive(Debug)]
struct RunResult {
    engine: &'static str,
    writers: usize,
    repetition: usize,
    rows: usize,
    batch_size: usize,
    elapsed: Duration,
    checkpoint: Duration,
    latencies_ns: Vec<u64>,
}

impl RunResult {
    fn rows_per_second(&self) -> f64 {
        self.rows as f64 / self.elapsed.as_secs_f64()
    }

    fn percentile_us(&self, percentile: usize) -> f64 {
        let index = (self.latencies_ns.len() * percentile)
            .div_ceil(100)
            .saturating_sub(1);
        self.latencies_ns[index.min(self.latencies_ns.len() - 1)] as f64 / 1_000.0
    }

    fn max_us(&self) -> f64 {
        *self.latencies_ns.last().expect("at least one transaction") as f64 / 1_000.0
    }
}

fn usage() -> &'static str {
    "Concurrent EliteSQL vs SQLite writer benchmark\n\
     \n\
     Usage:\n\
       cargo bench -p elitesql-core --bench concurrent_writers -- [OPTIONS]\n\
     \n\
     Options:\n\
       --writers LIST       Comma-separated writer counts [default: 1,2,4,8]\n\
       --rows N             Total rows per engine/run [default: 200k]\n\
       --batch-size N       Rows per transaction [default: 10]\n\
       --repetitions N      Runs per engine/writer count [default: 3]\n\
       --durability MODE    fast, balanced, or safe [default: fast]\n\
       --csv PATH           CSV output [default: benchmark-results/concurrent-writers.csv]\n\
       --smoke              Use 4k rows and one repetition\n\
       -h, --help           Show this help"
}

fn required_value(args: &mut impl Iterator<Item = String>, flag: &str) -> Result<String, String> {
    args.next()
        .ok_or_else(|| format!("missing value after {flag}"))
}

fn parse_count(value: &str) -> Result<usize, String> {
    let normalized = value.replace('_', "").to_ascii_lowercase();
    let (digits, multiplier) = match normalized.as_bytes().last() {
        Some(b'k') => (&normalized[..normalized.len() - 1], 1_000usize),
        Some(b'm') => (&normalized[..normalized.len() - 1], 1_000_000usize),
        _ => (normalized.as_str(), 1usize),
    };
    let base = digits
        .parse::<usize>()
        .map_err(|_| format!("invalid count '{value}'"))?;
    base.checked_mul(multiplier)
        .ok_or_else(|| format!("count '{value}' is too large"))
}

fn parse_writers(value: &str) -> Result<Vec<usize>, String> {
    let mut writers = value
        .split(',')
        .map(|part| parse_count(part.trim()))
        .collect::<Result<Vec<_>, _>>()?;
    writers.sort_unstable();
    writers.dedup();
    Ok(writers)
}

fn row_id(writer: usize, sequence: usize) -> String {
    format!("w{writer:02}-row-{sequence:08}")
}

fn record(writer: usize, sequence: usize) -> Record {
    let mut record = Record::new();
    record.insert("id".into(), Value::Text(row_id(writer, sequence)));
    record.insert(
        "title".into(),
        Value::Text(format!("writer {writer} row {sequence}")),
    );
    record.insert("writer".into(), Value::Int64(writer as i64));
    record.insert("sequence".into(), Value::Int64(sequence as i64));
    record.insert("body".into(), Value::Text(BODY.into()));
    record
}

fn rows_for_writer(total: usize, writers: usize, writer: usize) -> usize {
    total / writers + usize::from(writer < total % writers)
}

fn run_elitesql(config: &Config, writers: usize, repetition: usize) -> Result<RunResult, String> {
    let dir = tempfile::tempdir().map_err(|e| e.to_string())?;
    // This benchmark isolates commit concurrency, so both engines postpone
    // checkpoint work until after the measured window. Size EliteSQL's
    // bounded resident delta for this fixture and assert below that it did not
    // consolidate early. The 384-byte charge is deliberately above this
    // benchmark record's measured primary-delta estimate.
    let delta_bytes = config.total_rows.saturating_mul(384).max(24 * 1024 * 1024);
    let query_bytes = 64usize * 1024 * 1024;
    let reserve_bytes = 8usize * 1024 * 1024;
    let total_bytes = query_bytes
        .saturating_add(delta_bytes.saturating_mul(2))
        .saturating_add(reserve_bytes);
    let db = Db::create_with(
        dir.path().join("concurrent.esql"),
        DbOptions {
            durability: config.durability.elitesql(),
            // Keep automatic checkpoints outside the concurrency window.
            memtable_max_bytes: u64::MAX,
            memory: MemoryOptions {
                total_memory_bytes: total_bytes,
                query_pool_bytes: query_bytes,
                index_delta_pool_bytes: delta_bytes,
                maintenance_pool_bytes: delta_bytes,
                reserved_memory_bytes: reserve_bytes,
                ..MemoryOptions::default()
            },
            auto_compaction: elitesql_core::AutoCompactionOptions::disabled(),
            ..DbOptions::default()
        },
    )
    .map_err(|e| e.to_string())?;
    db.create_table(TableSchema::new(
        "docs",
        vec![
            Column::new("title", ColumnType::Text).not_null(),
            Column::new("writer", ColumnType::Int64),
            Column::new("sequence", ColumnType::Int64),
            Column::new("body", ColumnType::Text),
        ],
    ))
    .map_err(|e| e.to_string())?;

    let db = Arc::new(db);
    let barrier = Arc::new(Barrier::new(writers + 1));
    let mut handles = Vec::with_capacity(writers);
    for writer in 0..writers {
        let db = Arc::clone(&db);
        let barrier = Arc::clone(&barrier);
        let row_count = rows_for_writer(config.total_rows, writers, writer);
        let batch_size = config.batch_size;
        handles.push(std::thread::spawn(move || -> Result<Vec<u64>, String> {
            barrier.wait();
            let mut latencies = Vec::with_capacity(row_count.div_ceil(batch_size));
            for start in (0..row_count).step_by(batch_size) {
                let end = (start + batch_size).min(row_count);
                let started = Instant::now();
                let mut transaction = db.begin();
                for sequence in start..end {
                    transaction
                        .insert("docs", record(writer, sequence))
                        .map_err(|e| e.to_string())?;
                }
                transaction.commit().map_err(|e| e.to_string())?;
                latencies.push(duration_ns(started.elapsed()));
            }
            Ok(latencies)
        }));
    }

    barrier.wait();
    let started = Instant::now();
    let mut latencies = Vec::new();
    for handle in handles {
        latencies.extend(
            handle
                .join()
                .map_err(|_| "EliteSQL writer thread panicked".to_string())??,
        );
    }
    let elapsed = started.elapsed();

    let memory = db.global_memory_stats();
    if memory.index_consolidations != 0 {
        return Err(format!(
            "EliteSQL consolidated its index delta {} times inside the measured window; increase the benchmark delta estimate",
            memory.index_consolidations
        ));
    }

    let checkpoint_started = Instant::now();
    db.checkpoint().map_err(|e| e.to_string())?;
    let checkpoint = checkpoint_started.elapsed();
    let found = db.scan("docs").map_err(|e| e.to_string())?.len();
    if found != config.total_rows {
        return Err(format!(
            "EliteSQL validation found {found} rows, expected {}",
            config.total_rows
        ));
    }
    latencies.sort_unstable();
    Ok(RunResult {
        engine: "EliteSQL",
        writers,
        repetition,
        rows: config.total_rows,
        batch_size: config.batch_size,
        elapsed,
        checkpoint,
        latencies_ns: latencies,
    })
}

fn sqlite_setup(path: &Path, durability: DurabilityProfile) -> Result<Connection, String> {
    let connection = Connection::open(path).map_err(|e| e.to_string())?;
    connection
        .execute_batch(&format!(
            "PRAGMA journal_mode=WAL;\n\
             PRAGMA synchronous={};\n\
             PRAGMA wal_autocheckpoint=0;\n\
             CREATE TABLE docs (\n\
               id TEXT PRIMARY KEY,\n\
               title TEXT NOT NULL,\n\
               writer INTEGER,\n\
               sequence INTEGER,\n\
               body TEXT\n\
             );",
            durability.sqlite()
        ))
        .map_err(|e| e.to_string())?;
    Ok(connection)
}

fn run_sqlite(config: &Config, writers: usize, repetition: usize) -> Result<RunResult, String> {
    let dir = tempfile::tempdir().map_err(|e| e.to_string())?;
    let path = dir.path().join("concurrent.sqlite3");
    // Keep one connection open so SQLite does not checkpoint automatically
    // when the last writer connection closes inside the measured window.
    let keeper = sqlite_setup(&path, config.durability)?;

    let barrier = Arc::new(Barrier::new(writers + 1));
    let mut handles = Vec::with_capacity(writers);
    for writer in 0..writers {
        let path = path.clone();
        let barrier = Arc::clone(&barrier);
        let row_count = rows_for_writer(config.total_rows, writers, writer);
        let batch_size = config.batch_size;
        let synchronous = config.durability.sqlite();
        handles.push(std::thread::spawn(move || -> Result<Vec<u64>, String> {
            let mut connection = Connection::open(path).map_err(|e| e.to_string())?;
            connection
                .busy_timeout(Duration::from_secs(60))
                .map_err(|e| e.to_string())?;
            connection
                .execute_batch(&format!(
                    "PRAGMA synchronous={synchronous}; PRAGMA wal_autocheckpoint=0;"
                ))
                .map_err(|e| e.to_string())?;
            barrier.wait();
            let mut latencies = Vec::with_capacity(row_count.div_ceil(batch_size));
            for start in (0..row_count).step_by(batch_size) {
                let end = (start + batch_size).min(row_count);
                let started = Instant::now();
                let transaction = connection.transaction().map_err(|e| e.to_string())?;
                {
                    let mut statement = transaction
                        .prepare_cached(
                            "INSERT INTO docs (id, title, writer, sequence, body) \
                             VALUES (?1, ?2, ?3, ?4, ?5)",
                        )
                        .map_err(|e| e.to_string())?;
                    for sequence in start..end {
                        statement
                            .execute(params![
                                row_id(writer, sequence),
                                format!("writer {writer} row {sequence}"),
                                writer as i64,
                                sequence as i64,
                                BODY,
                            ])
                            .map_err(|e| e.to_string())?;
                    }
                }
                transaction.commit().map_err(|e| e.to_string())?;
                latencies.push(duration_ns(started.elapsed()));
            }
            Ok(latencies)
        }));
    }

    barrier.wait();
    let started = Instant::now();
    let mut latencies = Vec::new();
    for handle in handles {
        latencies.extend(
            handle
                .join()
                .map_err(|_| "SQLite writer thread panicked".to_string())??,
        );
    }
    let elapsed = started.elapsed();

    keeper
        .execute_batch(&format!(
            "PRAGMA synchronous={}; PRAGMA wal_autocheckpoint=0;",
            config.durability.sqlite()
        ))
        .map_err(|e| e.to_string())?;
    let checkpoint_started = Instant::now();
    keeper
        .execute_batch("PRAGMA wal_checkpoint(TRUNCATE);")
        .map_err(|e| e.to_string())?;
    let checkpoint = checkpoint_started.elapsed();
    let found: usize = keeper
        .query_row("SELECT count(*) FROM docs", [], |row| row.get(0))
        .map_err(|e| e.to_string())?;
    if found != config.total_rows {
        return Err(format!(
            "SQLite validation found {found} rows, expected {}",
            config.total_rows
        ));
    }
    latencies.sort_unstable();
    Ok(RunResult {
        engine: "SQLite",
        writers,
        repetition,
        rows: config.total_rows,
        batch_size: config.batch_size,
        elapsed,
        checkpoint,
        latencies_ns: latencies,
    })
}

fn duration_ns(duration: Duration) -> u64 {
    duration.as_nanos().min(u64::MAX as u128) as u64
}

fn print_result(result: &RunResult) {
    println!(
        "  {:<8} writers={:<2} run={} {:>10.0} rows/s  p50={:>8.1} us  p95={:>8.1} us  p99={:>8.1} us  max={:>9.1} us  checkpoint={:.3} s",
        result.engine,
        result.writers,
        result.repetition,
        result.rows_per_second(),
        result.percentile_us(50),
        result.percentile_us(95),
        result.percentile_us(99),
        result.max_us(),
        result.checkpoint.as_secs_f64(),
    );
}

fn csv(results: &[RunResult], config: &Config) -> String {
    let mut out = String::from(
        "engine,writers,repetition,rows,batch_size,durability,elapsed_seconds,rows_per_second,p50_us,p95_us,p99_us,max_us,checkpoint_seconds\n",
    );
    for result in results {
        out.push_str(&format!(
            "{},{},{},{},{},{},{:.9},{:.3},{:.3},{:.3},{:.3},{:.3},{:.9}\n",
            result.engine,
            result.writers,
            result.repetition,
            result.rows,
            result.batch_size,
            config.durability.name(),
            result.elapsed.as_secs_f64(),
            result.rows_per_second(),
            result.percentile_us(50),
            result.percentile_us(95),
            result.percentile_us(99),
            result.max_us(),
            result.checkpoint.as_secs_f64(),
        ));
    }
    out
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

    println!("EliteSQL vs SQLite concurrent writers");
    println!("  EliteSQL:    {}", env!("CARGO_PKG_VERSION"));
    println!("  SQLite:      {} (rusqlite bundled)", rusqlite::version());
    println!("  writers:     {:?}", config.writers);
    println!("  total rows:  {} per engine/run", config.total_rows);
    println!("  batch size:  {} rows/transaction", config.batch_size);
    println!("  repetitions: {}", config.repetitions);
    println!(
        "  durability:  EliteSQL {} <-> SQLite WAL + synchronous={}",
        config.durability.name(),
        config.durability.sqlite()
    );
    println!("  checkpoints: outside measured write window");

    let mut results = Vec::new();
    for &writers in &config.writers {
        for repetition in 1..=config.repetitions {
            // Alternate engine order to reduce systematic thermal/cache bias.
            if repetition % 2 == 1 {
                let elitesql = run_elitesql(&config, writers, repetition)?;
                print_result(&elitesql);
                results.push(elitesql);
                let sqlite = run_sqlite(&config, writers, repetition)?;
                print_result(&sqlite);
                results.push(sqlite);
            } else {
                let sqlite = run_sqlite(&config, writers, repetition)?;
                print_result(&sqlite);
                results.push(sqlite);
                let elitesql = run_elitesql(&config, writers, repetition)?;
                print_result(&elitesql);
                results.push(elitesql);
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
