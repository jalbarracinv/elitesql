//! Scalable, single-run comparison between EliteSQL and SQLite.
//!
//! Unlike the Criterion microbenchmarks, this program loads each database
//! only once. Both engines receive exactly the same deterministic rows and
//! transaction batch size. Durability profiles are mapped as follows:
//!
//! - fast:     EliteSQL Fast     <-> SQLite WAL + synchronous=OFF
//! - balanced: EliteSQL Balanced <-> SQLite WAL + synchronous=NORMAL
//! - safe:     EliteSQL Safe     <-> SQLite WAL + synchronous=FULL
//!
//! Examples:
//!   cargo bench -p elitesql-core --bench scale_vs_sqlite -- --rows 1m
//!   cargo bench -p elitesql-core --bench scale_vs_sqlite -- --rows 10m --durability fast
//!   cargo bench -p elitesql-core --bench scale_vs_sqlite -- --smoke

use std::env;
use std::error::Error;
use std::fs;
use std::hint::black_box;
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use elitesql_core::{
    Column, ColumnType, Db, DbOptions, Durability, MemoryOptions, Record, TableSchema, Value,
};
use rusqlite::{params, Connection};

const BODY: &str = "The quick brown fox jumps over the lazy dog; deterministic benchmark payload.";

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
    rows: usize,
    batch_size: usize,
    point_reads: usize,
    full_scans: usize,
    durability: DurabilityProfile,
    engine: EngineSelection,
    total_memory_mib: Option<usize>,
    index_delta_mib: Option<usize>,
    maintenance_mib: Option<usize>,
    memtable_mib: Option<usize>,
    bulk_sorted: bool,
    csv: Option<PathBuf>,
}

#[derive(Debug, Clone, Copy)]
enum EngineSelection {
    Both,
    EliteSql,
    Sqlite,
}

impl EngineSelection {
    fn parse(value: &str) -> Result<Self, String> {
        match value {
            "both" => Ok(Self::Both),
            "elitesql" => Ok(Self::EliteSql),
            "sqlite" => Ok(Self::Sqlite),
            _ => Err(format!(
                "invalid engine '{value}' (expected both, elitesql, or sqlite)"
            )),
        }
    }

    fn name(self) -> &'static str {
        match self {
            Self::Both => "EliteSQL, then SQLite",
            Self::EliteSql => "EliteSQL only",
            Self::Sqlite => "SQLite only",
        }
    }
}

impl Default for Config {
    fn default() -> Self {
        Self {
            rows: 1_000_000,
            batch_size: 10_000,
            point_reads: 100_000,
            full_scans: 1,
            durability: DurabilityProfile::Fast,
            engine: EngineSelection::Both,
            total_memory_mib: None,
            index_delta_mib: None,
            maintenance_mib: None,
            memtable_mib: None,
            bulk_sorted: false,
            csv: None,
        }
    }
}

impl Config {
    fn parse() -> Result<Option<Self>, String> {
        let mut config = Self::default();
        let mut args = env::args().skip(1);
        while let Some(arg) = args.next() {
            match arg.as_str() {
                "--rows" => {
                    config.rows = parse_count(&required_value(&mut args, "--rows")?)?;
                }
                "--batch-size" => {
                    config.batch_size = parse_count(&required_value(&mut args, "--batch-size")?)?;
                }
                "--point-reads" => {
                    config.point_reads = parse_count(&required_value(&mut args, "--point-reads")?)?;
                }
                "--full-scans" => {
                    config.full_scans = parse_count(&required_value(&mut args, "--full-scans")?)?;
                }
                "--durability" => {
                    config.durability =
                        DurabilityProfile::parse(&required_value(&mut args, "--durability")?)?;
                }
                "--engine" => {
                    config.engine =
                        EngineSelection::parse(&required_value(&mut args, "--engine")?)?;
                }
                "--total-memory-mib" => {
                    config.total_memory_mib = Some(parse_mib(&required_value(
                        &mut args,
                        "--total-memory-mib",
                    )?)?);
                }
                "--index-delta-mib" => {
                    config.index_delta_mib =
                        Some(parse_mib(&required_value(&mut args, "--index-delta-mib")?)?);
                }
                "--maintenance-mib" => {
                    config.maintenance_mib =
                        Some(parse_mib(&required_value(&mut args, "--maintenance-mib")?)?);
                }
                "--memtable-mib" => {
                    config.memtable_mib =
                        Some(parse_mib(&required_value(&mut args, "--memtable-mib")?)?);
                }
                "--bulk-sorted" => config.bulk_sorted = true,
                "--csv" => {
                    let path = PathBuf::from(required_value(&mut args, "--csv")?);
                    config.csv = Some(if path.is_absolute() {
                        path
                    } else {
                        Path::new(env!("CARGO_MANIFEST_DIR"))
                            .join("../..")
                            .join(path)
                    });
                }
                "--smoke" => {
                    config.rows = 10_000;
                    config.point_reads = 1_000;
                    config.full_scans = 1;
                }
                "--bench" => {
                    // Cargo may pass this conventional harness argument even
                    // though this benchmark has `harness = false`.
                }
                "-h" | "--help" => return Ok(None),
                _ => return Err(format!("unknown argument '{arg}'\n\n{}", usage())),
            }
        }

        if config.rows == 0 {
            return Err("--rows must be greater than zero".into());
        }
        if config.batch_size == 0 {
            return Err("--batch-size must be greater than zero".into());
        }
        if config.point_reads == 0 {
            return Err("--point-reads must be greater than zero".into());
        }
        if config.full_scans == 0 {
            return Err("--full-scans must be greater than zero".into());
        }
        Ok(Some(config))
    }
}

#[derive(Debug)]
struct ResultRow {
    engine: &'static str,
    ingest_wall: Duration,
    final_checkpoint_wall: Duration,
    maintenance_drain_wall: Duration,
    checkpoint_work: Duration,
    checkpoint_count: u64,
    promotion_work: Duration,
    promotion_count: u64,
    total_load: Duration,
    point_reads: Duration,
    full_scans: Duration,
    disk_bytes: u64,
}

fn usage() -> &'static str {
    "Scalable EliteSQL vs SQLite benchmark\n\
     \n\
     Usage:\n\
       cargo bench -p elitesql-core --bench scale_vs_sqlite -- [OPTIONS]\n\
     \n\
     Options:\n\
       --rows N             Rows to load; accepts k/m suffixes [default: 1m]\n\
       --batch-size N       Rows committed per transaction [default: 10k]\n\
       --point-reads N      Primary-key reads per engine [default: 100k]\n\
       --full-scans N       Unindexed scans per engine [default: 1]\n\
       --durability MODE    fast, balanced, or safe [default: fast]\n\
       --engine ENGINE      both, elitesql, or sqlite [default: both]\n\
       --total-memory-mib N EliteSQL logical memory envelope\n\
       --index-delta-mib N  EliteSQL mutable-index pool\n\
       --maintenance-mib N  EliteSQL maintenance pool\n\
       --memtable-mib N     EliteSQL automatic checkpoint threshold\n\
       --bulk-sorted        Use EliteSQL's direct sorted bulk-load path\n\
       --csv PATH           Write structured single-run results\n\
       --smoke              Use 10k rows and 1k point reads\n\
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

fn parse_mib(value: &str) -> Result<usize, String> {
    let mib = parse_count(value)?;
    if mib == 0 {
        return Err("memory values must be greater than zero".into());
    }
    Ok(mib)
}

fn row_id(i: usize) -> String {
    format!("row-{i:010}")
}

fn title(i: usize) -> String {
    format!("document number {i}")
}

fn elitesql_record(i: usize) -> Record {
    let mut record = Record::new();
    record.insert("id".into(), Value::Text(row_id(i)));
    record.insert("title".into(), Value::Text(title(i)));
    record.insert("body".into(), Value::Text(BODY.into()));
    record.insert("score".into(), Value::Int64(i as i64));
    record
}

fn point_read_ids(rows: usize, reads: usize) -> Vec<String> {
    let mut index = 0usize;
    (0..reads)
        .map(|_| {
            index = (index + 7_919) % rows;
            row_id(index)
        })
        .collect()
}

fn print_progress(engine: &str, completed: usize, rows: usize, next_percent: &mut usize) {
    let percent = completed.saturating_mul(100) / rows;
    if percent >= *next_percent || completed == rows {
        println!("  {engine}: loaded {completed}/{rows} rows ({percent}%)");
        let _ = io::stdout().flush();
        while *next_percent <= percent {
            *next_percent += 10;
        }
    }
}

fn run_elitesql(config: &Config, ids: &[String]) -> Result<ResultRow, Box<dyn Error>> {
    println!("\nEliteSQL");
    let dir = tempfile::tempdir()?;
    let mut options = DbOptions {
        durability: config.durability.elitesql(),
        // Keep canonical segment compaction from altering historical runs.
        auto_compaction: elitesql_core::AutoCompactionOptions::disabled(),
        ..DbOptions::default()
    };
    let mib = 1024 * 1024;
    let mut memory = MemoryOptions::default();
    if let Some(value) = config.total_memory_mib {
        memory.total_memory_bytes = value.saturating_mul(mib);
    }
    if let Some(value) = config.index_delta_mib {
        memory.index_delta_pool_bytes = value.saturating_mul(mib);
    }
    if let Some(value) = config.maintenance_mib {
        memory.maintenance_pool_bytes = value.saturating_mul(mib);
    }
    if let Some(value) = config.memtable_mib {
        options.memtable_max_bytes = value.saturating_mul(mib) as u64;
    }
    options.memory = memory;
    let db = Db::create_with(dir.path().join("bench.esql"), options)?;
    db.create_table(TableSchema::new(
        "docs",
        vec![
            Column::new("title", ColumnType::Text).not_null(),
            Column::new("body", ColumnType::Text),
            Column::new("score", ColumnType::Int64),
        ],
    ))?;

    let maintenance_before = db.maintenance_stats();
    let load_started = Instant::now();
    let mut stage_wall = Duration::ZERO;
    let mut commit_wall = Duration::ZERO;
    if config.bulk_sorted {
        let bulk_started = Instant::now();
        let loaded = db.bulk_insert_sorted("docs", (0..config.rows).map(elitesql_record))?;
        if loaded != config.rows {
            return Err(format!("EliteSQL bulk load wrote {loaded} rows").into());
        }
        commit_wall = bulk_started.elapsed();
        println!("  EliteSQL bulk: loaded {loaded}/{} rows", config.rows);
    } else {
        let mut next_percent = 10;
        for start in (0..config.rows).step_by(config.batch_size) {
            let end = (start + config.batch_size).min(config.rows);
            let stage_started = Instant::now();
            let mut transaction = db.begin();
            for i in start..end {
                transaction.insert("docs", elitesql_record(i))?;
            }
            stage_wall += stage_started.elapsed();
            let commit_started = Instant::now();
            transaction.commit()?;
            commit_wall += commit_started.elapsed();
            print_progress("EliteSQL", end, config.rows, &mut next_percent);
        }
    }
    let ingest_wall = load_started.elapsed();
    let checkpoint_started = Instant::now();
    db.checkpoint()?;
    let final_checkpoint_wall = checkpoint_started.elapsed();
    let drain_started = Instant::now();
    db.wait_for_primary_compaction().unwrap();
    let maintenance_drain_wall = drain_started.elapsed();
    let total_load = load_started.elapsed();
    let maintenance_after = db.maintenance_stats();
    let checkpoint_work = maintenance_after
        .checkpoint_time
        .saturating_sub(maintenance_before.checkpoint_time);
    let checkpoint_count = maintenance_after
        .checkpoints
        .saturating_sub(maintenance_before.checkpoints);
    let promotion_work = maintenance_after
        .primary_run_compaction_time
        .saturating_sub(maintenance_before.primary_run_compaction_time);
    let promotion_count = maintenance_after
        .primary_run_compactions
        .saturating_sub(maintenance_before.primary_run_compactions);
    let commits = maintenance_after
        .commits
        .saturating_sub(maintenance_before.commits);
    let commit_total = maintenance_after
        .commit_time
        .saturating_sub(maintenance_before.commit_time);
    let commit_lock_wait = maintenance_after
        .commit_lock_wait_time
        .saturating_sub(maintenance_before.commit_lock_wait_time);
    let commit_prepare = maintenance_after
        .commit_prepare_time
        .saturating_sub(maintenance_before.commit_prepare_time);
    let commit_wal = maintenance_after
        .commit_wal_time
        .saturating_sub(maintenance_before.commit_wal_time);
    let commit_apply = maintenance_after
        .commit_apply_time
        .saturating_sub(maintenance_before.commit_apply_time);
    println!(
        "  load phases: {:.3} s staging + {:.3} s commit/bulk calls (automatic primary flushes may overlap)",
        stage_wall.as_secs_f64(),
        commit_wall.as_secs_f64()
    );
    println!(
        "  commit internals: {:.3} s total ({}), {:.3} s lock wait, {:.3} s prepare, {:.3} s WAL, {:.3} s apply, {:.3} s other/admission wait",
        commit_total.as_secs_f64(),
        commits,
        commit_lock_wait.as_secs_f64(),
        commit_prepare.as_secs_f64(),
        commit_wal.as_secs_f64(),
        commit_apply.as_secs_f64(),
        commit_total
            .saturating_sub(commit_lock_wait)
            .saturating_sub(commit_prepare)
            .saturating_sub(commit_wal)
            .saturating_sub(commit_apply)
            .as_secs_f64(),
    );
    println!(
        "  primary LSM: {} runs, {} promotions ({:.3} s worker time), {:.2} MiB checkpoint runs, {:.2} MiB read + {:.2} MiB written by promotions",
        maintenance_after.primary_runs,
        promotion_count,
        promotion_work.as_secs_f64(),
        maintenance_after.primary_checkpoint_bytes_written as f64 / mib as f64,
        maintenance_after.primary_run_compaction_bytes_read as f64 / mib as f64,
        maintenance_after.primary_run_compaction_bytes_written as f64 / mib as f64,
    );

    validate_elitesql_row(&db, config.rows - 1)?;
    warm_elitesql(&db, ids)?;
    let point_started = Instant::now();
    for id in ids {
        black_box(
            db.get("docs", id)?
                .ok_or_else(|| format!("EliteSQL point read did not find {id}"))?,
        );
    }
    let point_reads = point_started.elapsed();

    let target = Value::Int64((config.rows - 1) as i64);
    let scan_started = Instant::now();
    for _ in 0..config.full_scans {
        let matches = db.find_eq("docs", "score", &target)?;
        if matches.len() != 1 || matches[0].0 != row_id(config.rows - 1) {
            return Err(format!(
                "EliteSQL full scan returned {} rows instead of the expected row",
                matches.len()
            )
            .into());
        }
        black_box(matches);
    }
    let full_scans = scan_started.elapsed();
    let memory = db.global_memory_stats();
    println!(
        "  logical memory: {:.0} MiB configured; query peak {:.2}/{:.0} MiB, index-delta peak {:.2}/{:.0} MiB, maintenance peak {:.2}/{:.0} MiB ({} consolidations)",
        memory.total_bytes as f64 / mib as f64,
        memory.query_peak_bytes as f64 / mib as f64,
        memory.query_capacity_bytes as f64 / mib as f64,
        memory.index_delta_peak_bytes as f64 / mib as f64,
        memory.index_delta_capacity_bytes as f64 / mib as f64,
        memory.maintenance_peak_bytes as f64 / mib as f64,
        memory.maintenance_capacity_bytes as f64 / mib as f64,
        memory.index_consolidations,
    );
    let disk_bytes = directory_size(dir.path())?;

    Ok(ResultRow {
        engine: if config.bulk_sorted {
            "EliteBulk"
        } else {
            "EliteSQL"
        },
        ingest_wall,
        final_checkpoint_wall,
        maintenance_drain_wall,
        checkpoint_work,
        checkpoint_count,
        promotion_work,
        promotion_count,
        total_load,
        point_reads,
        full_scans,
        disk_bytes,
    })
}

fn validate_elitesql_row(db: &Db, i: usize) -> Result<(), Box<dyn Error>> {
    let id = row_id(i);
    let record = db
        .get("docs", &id)?
        .ok_or_else(|| format!("EliteSQL validation did not find {id}"))?;
    if record.get("title") != Some(&Value::Text(title(i)))
        || record.get("score") != Some(&Value::Int64(i as i64))
    {
        return Err("EliteSQL validation returned incorrect values".into());
    }
    Ok(())
}

fn warm_elitesql(db: &Db, ids: &[String]) -> Result<(), Box<dyn Error>> {
    for id in ids.iter().take(1_000) {
        black_box(db.get("docs", id)?.ok_or("EliteSQL warmup read failed")?);
    }
    Ok(())
}

fn run_sqlite(config: &Config, ids: &[String]) -> Result<ResultRow, Box<dyn Error>> {
    println!("\nSQLite");
    let dir = tempfile::tempdir()?;
    let mut connection = Connection::open(dir.path().join("bench.sqlite3"))?;
    connection.execute_batch(&format!(
        "PRAGMA journal_mode=WAL;\n\
         PRAGMA synchronous={};\n\
         PRAGMA wal_autocheckpoint=0;\n\
         CREATE TABLE docs (\n\
           id TEXT PRIMARY KEY,\n\
           title TEXT NOT NULL,\n\
           body TEXT,\n\
           score INTEGER\n\
         );",
        config.durability.sqlite()
    ))?;

    let load_started = Instant::now();
    let mut execute_wall = Duration::ZERO;
    let mut commit_wall = Duration::ZERO;
    let mut next_percent = 10;
    for start in (0..config.rows).step_by(config.batch_size) {
        let end = (start + config.batch_size).min(config.rows);
        let transaction = connection.transaction()?;
        {
            let mut statement = transaction
                .prepare("INSERT INTO docs (id, title, body, score) VALUES (?1, ?2, ?3, ?4)")?;
            let execute_started = Instant::now();
            for i in start..end {
                statement.execute(params![row_id(i), title(i), BODY, i as i64])?;
            }
            execute_wall += execute_started.elapsed();
        }
        let commit_started = Instant::now();
        transaction.commit()?;
        commit_wall += commit_started.elapsed();
        print_progress("SQLite", end, config.rows, &mut next_percent);
    }
    println!(
        "  transaction phases: {:.3} s execute + {:.3} s commit calls",
        execute_wall.as_secs_f64(),
        commit_wall.as_secs_f64()
    );
    let ingest_wall = load_started.elapsed();
    let checkpoint_started = Instant::now();
    connection.execute_batch("PRAGMA wal_checkpoint(TRUNCATE);")?;
    let final_checkpoint_wall = checkpoint_started.elapsed();
    let total_load = ingest_wall + final_checkpoint_wall;

    validate_sqlite_row(&connection, config.rows - 1)?;
    let mut point_statement =
        connection.prepare("SELECT title, body, score FROM docs WHERE id = ?1")?;
    for id in ids.iter().take(1_000) {
        black_box(read_sqlite_row(&mut point_statement, id)?);
    }
    let point_started = Instant::now();
    for id in ids {
        black_box(read_sqlite_row(&mut point_statement, id)?);
    }
    let point_reads = point_started.elapsed();
    drop(point_statement);

    let target_score = (config.rows - 1) as i64;
    let mut scan_statement =
        connection.prepare("SELECT id, title, body, score FROM docs WHERE score = ?1")?;
    let scan_started = Instant::now();
    for _ in 0..config.full_scans {
        let mut rows = scan_statement.query([target_score])?;
        let mut found = 0usize;
        while let Some(row) = rows.next()? {
            let id: String = row.get(0)?;
            let title: String = row.get(1)?;
            let body: String = row.get(2)?;
            let score: i64 = row.get(3)?;
            if id != row_id(config.rows - 1)
                || title != crate::title(config.rows - 1)
                || body != BODY
                || score != target_score
            {
                return Err("SQLite full scan returned incorrect values".into());
            }
            found += 1;
            black_box((&id, &title, &body, score));
        }
        if found != 1 {
            return Err(format!("SQLite full scan returned {found} rows instead of one").into());
        }
    }
    let full_scans = scan_started.elapsed();
    drop(scan_statement);
    let disk_bytes = directory_size(dir.path())?;

    Ok(ResultRow {
        engine: "SQLite",
        ingest_wall,
        final_checkpoint_wall,
        maintenance_drain_wall: Duration::ZERO,
        checkpoint_work: final_checkpoint_wall,
        checkpoint_count: 1,
        promotion_work: Duration::ZERO,
        promotion_count: 0,
        total_load,
        point_reads,
        full_scans,
        disk_bytes,
    })
}

fn read_sqlite_row(
    statement: &mut rusqlite::Statement<'_>,
    id: &str,
) -> Result<(String, String, i64), rusqlite::Error> {
    statement.query_row([id], |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)))
}

fn validate_sqlite_row(connection: &Connection, i: usize) -> Result<(), Box<dyn Error>> {
    let (actual_title, score): (String, i64) = connection.query_row(
        "SELECT title, score FROM docs WHERE id = ?1",
        [row_id(i)],
        |row| Ok((row.get(0)?, row.get(1)?)),
    )?;
    if actual_title != title(i) || score != i as i64 {
        return Err("SQLite validation returned incorrect values".into());
    }
    Ok(())
}

fn directory_size(path: &Path) -> io::Result<u64> {
    let mut total = 0u64;
    for entry in fs::read_dir(path)? {
        let entry = entry?;
        let metadata = entry.metadata()?;
        if metadata.is_dir() {
            total += directory_size(&entry.path())?;
        } else if metadata.is_file() {
            total += metadata.len();
        }
    }
    Ok(total)
}

fn format_duration_per_operation(duration: Duration, operations: usize) -> String {
    let nanos = duration.as_secs_f64() * 1_000_000_000.0 / operations as f64;
    if nanos >= 1_000_000.0 {
        format!("{:.3} ms/op", nanos / 1_000_000.0)
    } else if nanos >= 1_000.0 {
        format!("{:.3} us/op", nanos / 1_000.0)
    } else {
        format!("{nanos:.1} ns/op")
    }
}

fn print_results(config: &Config, elitesql: &ResultRow, sqlite: &ResultRow) {
    println!("\nResults");
    println!(
        "{:<10} {:>11} {:>11} {:>11} {:>11} {:>12} {:>15} {:>12}",
        "engine",
        "ingest wall",
        "final cp",
        "drain",
        "total load",
        "rows/s",
        "point read",
        "full scan"
    );
    for result in [elitesql, sqlite] {
        println!(
            "{:<10} {:>9.3} s {:>9.3} s {:>9.3} s {:>9.3} s {:>12.0} {:>15} {:>10.3} s",
            result.engine,
            result.ingest_wall.as_secs_f64(),
            result.final_checkpoint_wall.as_secs_f64(),
            result.maintenance_drain_wall.as_secs_f64(),
            result.total_load.as_secs_f64(),
            config.rows as f64 / result.total_load.as_secs_f64(),
            format_duration_per_operation(result.point_reads, config.point_reads),
            result.full_scans.as_secs_f64() / config.full_scans as f64,
        );
        println!(
            "{:<10} checkpoint work: {:.3} s ({}), promotion work: {:.3} s ({}), disk: {:.2} MiB",
            "",
            result.checkpoint_work.as_secs_f64(),
            result.checkpoint_count,
            result.promotion_work.as_secs_f64(),
            result.promotion_count,
            result.disk_bytes as f64 / 1_048_576.0
        );
    }

    println!("\nSQLite time / EliteSQL time (>1 means EliteSQL is faster)");
    println!(
        "  ingest wall: {:.3}x",
        sqlite.ingest_wall.as_secs_f64() / elitesql.ingest_wall.as_secs_f64()
    );
    println!(
        "  final checkpoint: {:.3}x",
        sqlite.final_checkpoint_wall.as_secs_f64() / elitesql.final_checkpoint_wall.as_secs_f64()
    );
    println!(
        "  total load: {:.3}x",
        sqlite.total_load.as_secs_f64() / elitesql.total_load.as_secs_f64()
    );
    println!(
        "  point read: {:.3}x",
        sqlite.point_reads.as_secs_f64() / elitesql.point_reads.as_secs_f64()
    );
    println!(
        "  full scan:  {:.3}x",
        sqlite.full_scans.as_secs_f64() / elitesql.full_scans.as_secs_f64()
    );
}

fn print_single_result(config: &Config, result: &ResultRow) {
    println!("\nResult");
    println!(
        "{:<10} {:>11} {:>11} {:>11} {:>11} {:>12} {:>15} {:>12}",
        "engine",
        "ingest wall",
        "final cp",
        "drain",
        "total load",
        "rows/s",
        "point read",
        "full scan"
    );
    println!(
        "{:<10} {:>9.3} s {:>9.3} s {:>9.3} s {:>9.3} s {:>12.0} {:>15} {:>10.3} s",
        result.engine,
        result.ingest_wall.as_secs_f64(),
        result.final_checkpoint_wall.as_secs_f64(),
        result.maintenance_drain_wall.as_secs_f64(),
        result.total_load.as_secs_f64(),
        config.rows as f64 / result.total_load.as_secs_f64(),
        format_duration_per_operation(result.point_reads, config.point_reads),
        result.full_scans.as_secs_f64() / config.full_scans as f64,
    );
    println!(
        "{:<10} checkpoint work: {:.3} s ({}), promotion work: {:.3} s ({}), disk: {:.2} MiB",
        "",
        result.checkpoint_work.as_secs_f64(),
        result.checkpoint_count,
        result.promotion_work.as_secs_f64(),
        result.promotion_count,
        result.disk_bytes as f64 / 1_048_576.0
    );
}

fn csv(config: &Config, results: &[ResultRow]) -> String {
    let mut output = String::from(
        "engine,rows,batch_size,point_reads,full_scans,durability,bulk_sorted,total_memory_mib,index_delta_mib,maintenance_mib,memtable_mib,ingest_wall_seconds,final_checkpoint_seconds,maintenance_drain_seconds,checkpoint_work_seconds,checkpoint_count,promotion_work_seconds,promotion_count,total_load_seconds,rows_per_second,point_reads_seconds,point_read_us,full_scans_seconds,full_scan_seconds,disk_bytes\n",
    );
    let optional = |value: Option<usize>| value.map_or_else(String::new, |value| value.to_string());
    for result in results {
        output.push_str(&format!(
            "{},{},{},{},{},{},{},{},{},{},{},{:.9},{:.9},{:.9},{:.9},{},{:.9},{},{:.9},{:.3},{:.9},{:.3},{:.9},{:.9},{}\n",
            result.engine,
            config.rows,
            config.batch_size,
            config.point_reads,
            config.full_scans,
            config.durability.name(),
            config.bulk_sorted,
            optional(config.total_memory_mib),
            optional(config.index_delta_mib),
            optional(config.maintenance_mib),
            optional(config.memtable_mib),
            result.ingest_wall.as_secs_f64(),
            result.final_checkpoint_wall.as_secs_f64(),
            result.maintenance_drain_wall.as_secs_f64(),
            result.checkpoint_work.as_secs_f64(),
            result.checkpoint_count,
            result.promotion_work.as_secs_f64(),
            result.promotion_count,
            result.total_load.as_secs_f64(),
            config.rows as f64 / result.total_load.as_secs_f64(),
            result.point_reads.as_secs_f64(),
            result.point_reads.as_secs_f64() * 1_000_000.0 / config.point_reads as f64,
            result.full_scans.as_secs_f64(),
            result.full_scans.as_secs_f64() / config.full_scans as f64,
            result.disk_bytes,
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

    println!("EliteSQL vs SQLite scalable benchmark");
    println!("  EliteSQL:     {}", env!("CARGO_PKG_VERSION"));
    println!("  SQLite:       {} (rusqlite bundled)", rusqlite::version());
    println!("  rows:         {}", config.rows);
    println!("  batch size:   {}", config.batch_size);
    println!(
        "  point reads:  {} (warm cache, plus {} untimed warmups)",
        config.point_reads,
        config.point_reads.min(1_000)
    );
    println!(
        "  full scans:   {} (unindexed score lookup)",
        config.full_scans
    );
    println!(
        "  durability:   EliteSQL {} <-> SQLite WAL + synchronous={} ",
        config.durability.name(),
        config.durability.sqlite()
    );
    println!("  engines:      {}", config.engine.name());
    println!(
        "  EliteSQL load: {}",
        if config.bulk_sorted {
            "direct sorted bulk"
        } else {
            "batched transactions"
        }
    );
    println!(
        "  load timing:  ingest wall, final checkpoint, maintenance drain and worker time separated"
    );

    let ids = point_read_ids(config.rows, config.point_reads);
    let results = match config.engine {
        EngineSelection::Both => {
            let elitesql = run_elitesql(&config, &ids)?;
            let sqlite = run_sqlite(&config, &ids)?;
            print_results(&config, &elitesql, &sqlite);
            vec![elitesql, sqlite]
        }
        EngineSelection::EliteSql => {
            let result = run_elitesql(&config, &ids)?;
            print_single_result(&config, &result);
            vec![result]
        }
        EngineSelection::Sqlite => {
            let result = run_sqlite(&config, &ids)?;
            print_single_result(&config, &result);
            vec![result]
        }
    };
    if let Some(path) = &config.csv {
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent)?;
        }
        fs::write(path, csv(&config, &results))?;
        println!("\nCSV: {}", path.display());
    }
    Ok(())
}
