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

#[derive(Clone, Copy, Debug)]
enum SqliteSyncProfile {
    Ordinary,
    Strict,
}

impl SqliteSyncProfile {
    fn parse_many(value: &str) -> Result<Vec<Self>, String> {
        match value.to_ascii_lowercase().as_str() {
            "ordinary" | "fsync" => Ok(vec![Self::Ordinary]),
            "strict" | "fullfsync" => Ok(vec![Self::Strict]),
            "both" => Ok(vec![Self::Ordinary, Self::Strict]),
            _ => Err(format!(
                "unknown SQLite sync profile '{value}'; expected ordinary, strict, or both"
            )),
        }
    }

    fn name(self) -> &'static str {
        match self {
            Self::Ordinary => "ordinary",
            Self::Strict => "strict",
        }
    }

    fn engine(self) -> &'static str {
        match self {
            Self::Ordinary => "SQLite-fsync",
            Self::Strict => "SQLite-fullfsync",
        }
    }

    fn fullfsync(self) -> &'static str {
        match self {
            Self::Ordinary => "OFF",
            Self::Strict => "ON",
        }
    }

    fn primitive(self) -> &'static str {
        match self {
            Self::Ordinary => "fsync",
            Self::Strict => "F_FULLFSYNC",
        }
    }
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
    sqlite_sync_profiles: Vec<SqliteSyncProfile>,
    safe_group_commit_delay_us: u64,
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
            sqlite_sync_profiles: vec![SqliteSyncProfile::Ordinary],
            safe_group_commit_delay_us: 200,
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
                "--sqlite-sync" => {
                    config.sqlite_sync_profiles = SqliteSyncProfile::parse_many(&required_value(
                        &mut args,
                        "--sqlite-sync",
                    )?)?;
                }
                "--safe-group-delay-us" => {
                    config.safe_group_commit_delay_us =
                        parse_count(&required_value(&mut args, "--safe-group-delay-us")?)? as u64;
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
    sync_primitive: &'static str,
    writers: usize,
    repetition: usize,
    rows: usize,
    batch_size: usize,
    elapsed: Duration,
    checkpoint: Duration,
    latencies_ns: Vec<u64>,
    wal_syncs: Option<u64>,
    grouped_commits: Option<u64>,
    coordinated_batches: Option<u64>,
    coordinated_commits: Option<u64>,
    lock_wait_us: Option<f64>,
    lock_hold_us: Option<f64>,
    locked_prepare_us: Option<f64>,
    wal_append_us: Option<f64>,
    apply_us: Option<f64>,
    sync_us: Option<f64>,
    commits_per_sync: Option<f64>,
    max_group_commits: Option<u64>,
    synced_bytes: Option<u64>,
    max_group_bytes: Option<u64>,
    coalesce_us: Option<f64>,
    leader_lock_wait_us: Option<f64>,
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
       --sqlite-sync MODE   ordinary, strict, or both [default: ordinary]\n\
       --safe-group-delay-us N  Safe coalescing window [default: 200]\n\
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
            safe_group_commit_delay_us: config.safe_group_commit_delay_us,
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

    let started = Instant::now();
    barrier.wait();
    let mut latencies = Vec::new();
    for handle in handles {
        latencies.extend(
            handle
                .join()
                .map_err(|_| "EliteSQL writer thread panicked".to_string())??,
        );
    }
    let elapsed = started.elapsed();
    let maintenance = db.maintenance_stats();
    let commits = maintenance.commits.max(1) as f64;
    let syncs = maintenance.wal_syncs as f64;

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
        sync_primitive: elitesql_sync_primitive(),
        writers,
        repetition,
        rows: config.total_rows,
        batch_size: config.batch_size,
        elapsed,
        checkpoint,
        latencies_ns: latencies,
        wal_syncs: Some(maintenance.wal_syncs),
        grouped_commits: Some(maintenance.grouped_commits),
        coordinated_batches: Some(maintenance.coordinated_batches),
        coordinated_commits: Some(maintenance.coordinated_commits),
        lock_wait_us: Some(maintenance.commit_lock_wait_time.as_secs_f64() * 1_000_000.0 / commits),
        lock_hold_us: Some(maintenance.commit_lock_hold_time.as_secs_f64() * 1_000_000.0 / commits),
        locked_prepare_us: Some(
            maintenance.commit_locked_prepare_time.as_secs_f64() * 1_000_000.0 / commits,
        ),
        wal_append_us: Some(
            maintenance.commit_wal_append_time.as_secs_f64() * 1_000_000.0 / commits,
        ),
        apply_us: Some(maintenance.commit_apply_time.as_secs_f64() * 1_000_000.0 / commits),
        sync_us: (syncs > 0.0)
            .then_some(maintenance.wal_sync_time.as_secs_f64() * 1_000_000.0 / syncs),
        commits_per_sync: (syncs > 0.0).then_some(maintenance.commits as f64 / syncs),
        max_group_commits: Some(maintenance.wal_sync_max_group_commits),
        synced_bytes: Some(maintenance.wal_synced_bytes),
        max_group_bytes: Some(maintenance.wal_sync_max_group_bytes),
        coalesce_us: Some(
            maintenance.wal_group_coalesce_time.as_secs_f64() * 1_000_000.0 / commits,
        ),
        leader_lock_wait_us: Some(
            maintenance.wal_group_leader_lock_wait_time.as_secs_f64() * 1_000_000.0 / commits,
        ),
    })
}

fn elitesql_sync_primitive() -> &'static str {
    if cfg!(target_os = "macos") {
        "F_FULLFSYNC"
    } else {
        "sync_data"
    }
}

fn sqlite_setup(
    path: &Path,
    durability: DurabilityProfile,
    sync_profile: SqliteSyncProfile,
) -> Result<Connection, String> {
    let connection = Connection::open(path).map_err(|e| e.to_string())?;
    connection
        .execute_batch(&format!(
            "PRAGMA journal_mode=WAL;\n\
             PRAGMA synchronous={};\n\
             PRAGMA fullfsync={};\n\
             PRAGMA checkpoint_fullfsync={};\n\
             PRAGMA wal_autocheckpoint=0;\n\
             CREATE TABLE docs (\n\
               id TEXT PRIMARY KEY,\n\
               title TEXT NOT NULL,\n\
               writer INTEGER,\n\
               sequence INTEGER,\n\
               body TEXT\n\
             );",
            durability.sqlite(),
            sync_profile.fullfsync(),
            sync_profile.fullfsync(),
        ))
        .map_err(|e| e.to_string())?;
    verify_sqlite_sync_profile(&connection, sync_profile)?;
    Ok(connection)
}

fn verify_sqlite_sync_profile(
    connection: &Connection,
    sync_profile: SqliteSyncProfile,
) -> Result<(), String> {
    let fullfsync: i64 = connection
        .query_row("PRAGMA fullfsync", [], |row| row.get(0))
        .map_err(|e| e.to_string())?;
    let checkpoint_fullfsync: i64 = connection
        .query_row("PRAGMA checkpoint_fullfsync", [], |row| row.get(0))
        .map_err(|e| e.to_string())?;
    let expected = i64::from(matches!(sync_profile, SqliteSyncProfile::Strict));
    if fullfsync != expected || checkpoint_fullfsync != expected {
        return Err(format!(
            "SQLite did not apply the requested {} sync profile: fullfsync={fullfsync}, checkpoint_fullfsync={checkpoint_fullfsync}",
            sync_profile.name()
        ));
    }
    Ok(())
}

fn run_sqlite(
    config: &Config,
    writers: usize,
    repetition: usize,
    sync_profile: SqliteSyncProfile,
) -> Result<RunResult, String> {
    let dir = tempfile::tempdir().map_err(|e| e.to_string())?;
    let path = dir.path().join("concurrent.sqlite3");
    // Keep one connection open so SQLite does not checkpoint automatically
    // when the last writer connection closes inside the measured window.
    let keeper = sqlite_setup(&path, config.durability, sync_profile)?;

    let barrier = Arc::new(Barrier::new(writers + 1));
    let mut handles = Vec::with_capacity(writers);
    for writer in 0..writers {
        let path = path.clone();
        let barrier = Arc::clone(&barrier);
        let row_count = rows_for_writer(config.total_rows, writers, writer);
        let batch_size = config.batch_size;
        let synchronous = config.durability.sqlite();
        let fullfsync = sync_profile.fullfsync();
        handles.push(std::thread::spawn(move || -> Result<Vec<u64>, String> {
            let mut connection = Connection::open(path).map_err(|e| e.to_string())?;
            connection
                .busy_timeout(Duration::from_secs(60))
                .map_err(|e| e.to_string())?;
            connection
                .execute_batch(&format!(
                    "PRAGMA synchronous={synchronous}; PRAGMA fullfsync={fullfsync}; \
                     PRAGMA checkpoint_fullfsync={fullfsync}; PRAGMA wal_autocheckpoint=0;"
                ))
                .map_err(|e| e.to_string())?;
            verify_sqlite_sync_profile(&connection, sync_profile)?;
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

    let started = Instant::now();
    barrier.wait();
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
            "PRAGMA synchronous={}; PRAGMA fullfsync={}; \
             PRAGMA checkpoint_fullfsync={}; PRAGMA wal_autocheckpoint=0;",
            config.durability.sqlite(),
            sync_profile.fullfsync(),
            sync_profile.fullfsync(),
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
        engine: sync_profile.engine(),
        sync_primitive: sync_profile.primitive(),
        writers,
        repetition,
        rows: config.total_rows,
        batch_size: config.batch_size,
        elapsed,
        checkpoint,
        latencies_ns: latencies,
        wal_syncs: None,
        grouped_commits: None,
        coordinated_batches: None,
        coordinated_commits: None,
        lock_wait_us: None,
        lock_hold_us: None,
        locked_prepare_us: None,
        wal_append_us: None,
        apply_us: None,
        sync_us: None,
        commits_per_sync: None,
        max_group_commits: None,
        synced_bytes: None,
        max_group_bytes: None,
        coalesce_us: None,
        leader_lock_wait_us: None,
    })
}

fn duration_ns(duration: Duration) -> u64 {
    duration.as_nanos().min(u64::MAX as u128) as u64
}

fn print_result(result: &RunResult) {
    let syncs = result.wal_syncs.map_or_else(String::new, |wal_syncs| {
        format!(
            "  wal_syncs={wal_syncs} avg={:.1}us commits/sync={:.2} max_group={} grouped={} coordinated={}/{}",
            result.sync_us.unwrap_or(0.0),
            result.commits_per_sync.unwrap_or(0.0),
            result.max_group_commits.unwrap_or(0),
            result.grouped_commits.unwrap_or(0),
            result.coordinated_commits.unwrap_or(0),
            result.coordinated_batches.unwrap_or(0),
        )
    });
    let critical = result.lock_hold_us.map_or_else(String::new, |lock_hold| {
        format!(
            "  lock(wait/hold)={:.1}/{lock_hold:.1} us prepare={:.1} append={:.1} apply={:.1} us",
            result.lock_wait_us.unwrap_or(0.0),
            result.locked_prepare_us.unwrap_or(0.0),
            result.wal_append_us.unwrap_or(0.0),
            result.apply_us.unwrap_or(0.0),
        )
    });
    println!(
        "  {:<18} writers={:<2} run={} {:>10.0} rows/s  p50={:>8.1} us  p95={:>8.1} us  p99={:>8.1} us  max={:>9.1} us  checkpoint={:.3} s  primitive={}{}{}",
        result.engine,
        result.writers,
        result.repetition,
        result.rows_per_second(),
        result.percentile_us(50),
        result.percentile_us(95),
        result.percentile_us(99),
        result.max_us(),
        result.checkpoint.as_secs_f64(),
        result.sync_primitive,
        syncs,
        critical,
    );
}

fn csv(results: &[RunResult], config: &Config) -> String {
    let mut out = String::from(
        "engine,sync_primitive,writers,repetition,rows,batch_size,durability,elapsed_seconds,rows_per_second,p50_us,p95_us,p99_us,max_us,checkpoint_seconds,wal_syncs,wal_sync_us,commits_per_sync,max_group_commits,wal_synced_bytes,max_group_bytes,grouped_commits,coordinated_batches,coordinated_commits,coalesce_us_per_commit,leader_lock_wait_us_per_commit,lock_wait_us,lock_hold_us,locked_prepare_us,wal_append_us,apply_us\n",
    );
    for result in results {
        out.push_str(&format!(
            "{},{},{},{},{},{},{},{:.9},{:.3},{:.3},{:.3},{:.3},{:.3},{:.9},{},{},{},{},{},{},{},{},{},{},{},{},{},{},{},{}\n",
            result.engine,
            result.sync_primitive,
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
            result
                .wal_syncs
                .map_or_else(String::new, |value| value.to_string()),
            result
                .sync_us
                .map_or_else(String::new, |value| format!("{value:.3}")),
            result
                .commits_per_sync
                .map_or_else(String::new, |value| format!("{value:.6}")),
            result
                .max_group_commits
                .map_or_else(String::new, |value| value.to_string()),
            result
                .synced_bytes
                .map_or_else(String::new, |value| value.to_string()),
            result
                .max_group_bytes
                .map_or_else(String::new, |value| value.to_string()),
            result
                .grouped_commits
                .map_or_else(String::new, |value| value.to_string()),
            result
                .coordinated_batches
                .map_or_else(String::new, |value| value.to_string()),
            result
                .coordinated_commits
                .map_or_else(String::new, |value| value.to_string()),
            result
                .coalesce_us
                .map_or_else(String::new, |value| format!("{value:.3}")),
            result
                .leader_lock_wait_us
                .map_or_else(String::new, |value| format!("{value:.3}")),
            result
                .lock_wait_us
                .map_or_else(String::new, |value| format!("{value:.3}")),
            result
                .lock_hold_us
                .map_or_else(String::new, |value| format!("{value:.3}")),
            result
                .locked_prepare_us
                .map_or_else(String::new, |value| format!("{value:.3}")),
            result
                .wal_append_us
                .map_or_else(String::new, |value| format!("{value:.3}")),
            result
                .apply_us
                .map_or_else(String::new, |value| format!("{value:.3}")),
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
    println!(
        "  SQLite sync: {:?}",
        config
            .sqlite_sync_profiles
            .iter()
            .map(|profile| profile.name())
            .collect::<Vec<_>>()
    );
    println!(
        "  Safe coalescing window: {} us",
        config.safe_group_commit_delay_us
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
                for &sync_profile in &config.sqlite_sync_profiles {
                    let sqlite = run_sqlite(&config, writers, repetition, sync_profile)?;
                    print_result(&sqlite);
                    results.push(sqlite);
                }
            } else {
                for &sync_profile in config.sqlite_sync_profiles.iter().rev() {
                    let sqlite = run_sqlite(&config, writers, repetition, sync_profile)?;
                    print_result(&sqlite);
                    results.push(sqlite);
                }
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
