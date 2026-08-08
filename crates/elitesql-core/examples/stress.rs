//! Mixed, concurrent SQL stress test with an independent in-memory oracle.
//!
//! Run the reference workload from the workspace root:
//!   cargo run --release -p elitesql-core --example stress -- --duration 3m
//!
//! Use `--smoke` for a short harness check.

use std::collections::BTreeMap;
use std::env;
use std::error::Error as StdError;
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Barrier};
use std::thread;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use elitesql_core::{check, Db, DbOptions, Durability, QueryOutput, Value};

const TABLE: &str = "stress_rows";

#[derive(Debug)]
struct Config {
    duration: Duration,
    workers: usize,
    initial_rows: usize,
    durability: Durability,
    checkpoint_bytes: u64,
    seed: u64,
    path: PathBuf,
    progress_interval: Duration,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            duration: Duration::from_secs(3 * 60),
            workers: 4,
            initial_rows: 100,
            durability: Durability::Safe,
            checkpoint_bytes: 256 * 1024,
            seed: 0xE11E_5EED_2026_0807,
            path: default_path(),
            progress_interval: Duration::from_secs(10),
        }
    }
}

impl Config {
    fn parse() -> Result<Option<Self>, String> {
        let mut config = Self::default();
        let mut args = env::args().skip(1);
        while let Some(arg) = args.next() {
            match arg.as_str() {
                "--duration" => {
                    config.duration = parse_duration(&required(&mut args, "--duration")?)?;
                }
                "--workers" => {
                    config.workers = parse_usize(&required(&mut args, "--workers")?)?;
                }
                "--initial-rows" => {
                    config.initial_rows = parse_usize(&required(&mut args, "--initial-rows")?)?;
                }
                "--durability" => {
                    config.durability = parse_durability(&required(&mut args, "--durability")?)?;
                }
                "--checkpoint-bytes" => {
                    config.checkpoint_bytes =
                        parse_bytes(&required(&mut args, "--checkpoint-bytes")?)?;
                }
                "--seed" => {
                    config.seed = parse_u64(&required(&mut args, "--seed")?)?;
                }
                "--path" => config.path = PathBuf::from(required(&mut args, "--path")?),
                "--progress" => {
                    config.progress_interval = parse_duration(&required(&mut args, "--progress")?)?;
                }
                "--smoke" => {
                    config.duration = Duration::from_secs(2);
                    config.workers = 2;
                    config.initial_rows = 20;
                    config.checkpoint_bytes = 32 * 1024;
                    config.progress_interval = Duration::from_secs(1);
                }
                "-h" | "--help" => return Ok(None),
                _ => return Err(format!("unknown argument '{arg}'\n\n{}", usage())),
            }
        }
        if config.duration.is_zero() {
            return Err("--duration must be greater than zero".into());
        }
        if config.workers == 0 {
            return Err("--workers must be greater than zero".into());
        }
        if config.initial_rows == 0 {
            return Err("--initial-rows must be greater than zero".into());
        }
        if config.checkpoint_bytes == 0 {
            return Err("--checkpoint-bytes must be greater than zero".into());
        }
        if config.progress_interval.is_zero() {
            return Err("--progress must be greater than zero".into());
        }
        Ok(Some(config))
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct ExpectedRow {
    owner: i64,
    generation: i64,
    payload: String,
}

impl ExpectedRow {
    fn values(&self, id: &str) -> Vec<Value> {
        vec![
            Value::Text(id.to_owned()),
            Value::Int64(self.owner),
            Value::Int64(self.generation),
            Value::Text(self.payload.clone()),
        ]
    }
}

#[derive(Clone, Copy, Debug, Default)]
struct Counts {
    selects: u64,
    inserts: u64,
    updates: u64,
    deletes: u64,
}

impl Counts {
    fn total(self) -> u64 {
        self.selects + self.inserts + self.updates + self.deletes
    }

    fn add(&mut self, other: Self) {
        self.selects += other.selects;
        self.inserts += other.inserts;
        self.updates += other.updates;
        self.deletes += other.deletes;
    }
}

#[derive(Debug)]
struct WorkerResult {
    model: BTreeMap<String, ExpectedRow>,
    counts: Counts,
}

struct WorkerInput {
    db: Arc<Db>,
    worker: usize,
    model: BTreeMap<String, ExpectedRow>,
    seed: u64,
    initial_rows: usize,
    deadline: Instant,
    barrier: Arc<Barrier>,
    global_operations: Arc<AtomicU64>,
    stop: Arc<AtomicBool>,
}

#[derive(Debug)]
struct XorShift64(u64);

impl XorShift64 {
    fn new(seed: u64) -> Self {
        Self(seed.max(1))
    }

    fn next(&mut self) -> u64 {
        let mut x = self.0;
        x ^= x << 13;
        x ^= x >> 7;
        x ^= x << 17;
        self.0 = x;
        x
    }

    fn below(&mut self, upper: usize) -> usize {
        (self.next() % upper as u64) as usize
    }
}

fn usage() -> &'static str {
    "EliteSQL concurrent mixed-workload stress test\n\
     \n\
     Usage:\n\
       cargo run --release -p elitesql-core --example stress -- [OPTIONS]\n\
     \n\
     Workload:\n\
       50% SELECT, 15% INSERT, 20% UPDATE, 15% DELETE. Each worker owns\n\
       disjoint keys and checks every result against an in-memory model.\n\
     \n\
     Options:\n\
       --duration TIME          Duration such as 30s, 3m or 1h [default: 3m]\n\
       --workers N             Concurrent mixed-workload threads [default: 4]\n\
       --initial-rows N        Initial live rows per worker [default: 100]\n\
       --durability MODE       safe, balanced or fast [default: safe]\n\
       --checkpoint-bytes N    Automatic checkpoint threshold [default: 256k]\n\
       --seed N                Deterministic random seed\n\
       --path PATH             New database directory (existing paths are refused)\n\
       --progress TIME         Progress reporting interval [default: 10s]\n\
       --smoke                 Two-second harness check\n\
       -h, --help              Show this help"
}

fn required(args: &mut impl Iterator<Item = String>, flag: &str) -> Result<String, String> {
    args.next()
        .ok_or_else(|| format!("missing value after {flag}"))
}

fn parse_usize(value: &str) -> Result<usize, String> {
    value
        .replace('_', "")
        .parse()
        .map_err(|_| format!("invalid positive integer '{value}'"))
}

fn parse_u64(value: &str) -> Result<u64, String> {
    let normalized = value.replace('_', "");
    if let Some(hex) = normalized
        .strip_prefix("0x")
        .or_else(|| normalized.strip_prefix("0X"))
    {
        u64::from_str_radix(hex, 16).map_err(|_| format!("invalid seed '{value}'"))
    } else {
        normalized
            .parse()
            .map_err(|_| format!("invalid seed '{value}'"))
    }
}

fn parse_duration(value: &str) -> Result<Duration, String> {
    let normalized = value.trim().to_ascii_lowercase();
    let (number, multiplier_ms) = if let Some(number) = normalized.strip_suffix("ms") {
        (number, 1u64)
    } else if let Some(number) = normalized.strip_suffix('s') {
        (number, 1_000)
    } else if let Some(number) = normalized.strip_suffix('m') {
        (number, 60_000)
    } else if let Some(number) = normalized.strip_suffix('h') {
        (number, 3_600_000)
    } else {
        (normalized.as_str(), 1_000)
    };
    let amount = number
        .parse::<u64>()
        .map_err(|_| format!("invalid duration '{value}'"))?;
    amount
        .checked_mul(multiplier_ms)
        .map(Duration::from_millis)
        .ok_or_else(|| format!("duration '{value}' is too large"))
}

fn parse_bytes(value: &str) -> Result<u64, String> {
    let normalized = value.replace('_', "").to_ascii_lowercase();
    let (number, multiplier) = match normalized.as_bytes().last() {
        Some(b'k') => (&normalized[..normalized.len() - 1], 1024u64),
        Some(b'm') => (&normalized[..normalized.len() - 1], 1024 * 1024),
        Some(b'g') => (&normalized[..normalized.len() - 1], 1024 * 1024 * 1024),
        _ => (normalized.as_str(), 1),
    };
    number
        .parse::<u64>()
        .map_err(|_| format!("invalid byte count '{value}'"))?
        .checked_mul(multiplier)
        .ok_or_else(|| format!("byte count '{value}' is too large"))
}

fn parse_durability(value: &str) -> Result<Durability, String> {
    match value.to_ascii_lowercase().as_str() {
        "safe" => Ok(Durability::Safe),
        "balanced" => Ok(Durability::Balanced),
        "fast" => Ok(Durability::Fast),
        _ => Err(format!(
            "unknown durability '{value}'; expected safe, balanced or fast"
        )),
    }
}

fn durability_name(value: Durability) -> &'static str {
    match value {
        Durability::Safe => "safe",
        Durability::Balanced => "balanced",
        Durability::Fast => "fast",
    }
}

fn default_path() -> PathBuf {
    let timestamp = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs();
    let workspace = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../..")
        .canonicalize()
        .unwrap_or_else(|_| Path::new(env!("CARGO_MANIFEST_DIR")).join("../.."));
    workspace.join(format!(
        "target/stress-runs/elitesql-{timestamp}-{}.esql",
        std::process::id()
    ))
}

fn row_id(worker: usize, sequence: u64) -> String {
    format!("w{worker:04}-row-{sequence:012}")
}

fn payload(worker: usize, generation: i64) -> String {
    format!(
        "worker={worker:04};generation={generation:012};\
         deterministic mixed SQL stress payload for WAL, checkpoint and compaction validation"
    )
}

fn expected_row(worker: usize, generation: i64) -> ExpectedRow {
    ExpectedRow {
        owner: worker as i64,
        generation,
        payload: payload(worker, generation),
    }
}

fn seed_worker(
    db: &Db,
    worker: usize,
    rows: usize,
) -> Result<BTreeMap<String, ExpectedRow>, String> {
    let mut sql = String::from("INSERT INTO stress_rows (id, owner, generation, payload) VALUES ");
    let mut model = BTreeMap::new();
    for sequence in 0..rows as u64 {
        if sequence > 0 {
            sql.push_str(", ");
        }
        let id = row_id(worker, sequence);
        let row = expected_row(worker, 0);
        sql.push_str(&format!(
            "('{id}', {}, {}, '{}')",
            row.owner, row.generation, row.payload
        ));
        model.insert(id, row);
    }
    match db.query(&sql).map_err(|e| e.to_string())? {
        QueryOutput::Inserted { ids } if ids.len() == rows => Ok(model),
        other => Err(format!(
            "unexpected seed result for worker {worker}: {other:?}"
        )),
    }
}

fn pick_id(model: &BTreeMap<String, ExpectedRow>, rng: &mut XorShift64) -> Option<String> {
    if model.is_empty() {
        None
    } else {
        model.keys().nth(rng.below(model.len())).cloned()
    }
}

fn rows(output: QueryOutput, context: &str) -> Result<(Vec<String>, Vec<Vec<Value>>), String> {
    match output {
        QueryOutput::Rows { columns, rows } => Ok((columns, rows)),
        other => Err(format!("{context}: expected SELECT rows, got {other:?}")),
    }
}

fn select_one(
    db: &Db,
    worker: usize,
    model: &BTreeMap<String, ExpectedRow>,
    rng: &mut XorShift64,
    sequence: u64,
) -> Result<(), String> {
    let existing = !model.is_empty() && rng.below(10) != 0;
    let id = if existing {
        pick_id(model, rng).expect("model checked non-empty")
    } else {
        row_id(worker, sequence.saturating_add(1_000_000_000_000))
    };
    let (columns, found) = rows(
        db.query(&format!(
            "SELECT id, owner, generation, payload FROM {TABLE} WHERE id = '{id}'"
        ))
        .map_err(|e| e.to_string())?,
        "point SELECT",
    )?;
    let expected_columns = ["id", "owner", "generation", "payload"];
    if columns != expected_columns {
        return Err(format!(
            "point SELECT returned unexpected columns: {columns:?}"
        ));
    }
    let expected = model
        .get(&id)
        .map(|row| vec![row.values(&id)])
        .unwrap_or_default();
    if found != expected {
        return Err(format!(
            "worker {worker}: point SELECT mismatch for '{id}': found {found:?}, expected {expected:?}"
        ));
    }
    Ok(())
}

fn select_owner(
    db: &Db,
    worker: usize,
    model: &BTreeMap<String, ExpectedRow>,
) -> Result<(), String> {
    let (_, found) = rows(
        db.query(&format!(
            "SELECT id, owner, generation, payload FROM {TABLE} \
             WHERE owner = {worker} ORDER BY id"
        ))
        .map_err(|e| e.to_string())?,
        "owner SELECT",
    )?;
    let expected = model
        .iter()
        .map(|(id, row)| row.values(id))
        .collect::<Vec<_>>();
    if found != expected {
        return Err(format!(
            "worker {worker}: unindexed owner SELECT mismatch: found {} rows, expected {}",
            found.len(),
            expected.len()
        ));
    }
    Ok(())
}

fn insert_one(
    db: &Db,
    worker: usize,
    model: &mut BTreeMap<String, ExpectedRow>,
    sequence: &mut u64,
) -> Result<(), String> {
    let id = row_id(worker, *sequence);
    *sequence += 1;
    let row = expected_row(worker, 0);
    let output = db
        .query(&format!(
            "INSERT INTO {TABLE} (id, owner, generation, payload) \
             VALUES ('{id}', {}, {}, '{}')",
            row.owner, row.generation, row.payload
        ))
        .map_err(|e| e.to_string())?;
    if output
        != (QueryOutput::Inserted {
            ids: vec![id.clone()],
        })
    {
        return Err(format!(
            "worker {worker}: unexpected INSERT result: {output:?}"
        ));
    }
    model.insert(id, row);
    Ok(())
}

fn update_one(
    db: &Db,
    worker: usize,
    model: &mut BTreeMap<String, ExpectedRow>,
    rng: &mut XorShift64,
) -> Result<bool, String> {
    let Some(id) = pick_id(model, rng) else {
        return Ok(false);
    };
    let generation = model[&id].generation + 1;
    let new_payload = payload(worker, generation);
    let output = db
        .query(&format!(
            "UPDATE {TABLE} SET generation = {generation}, payload = '{new_payload}' \
             WHERE id = '{id}'"
        ))
        .map_err(|e| e.to_string())?;
    if output != QueryOutput::Affected(1) {
        return Err(format!(
            "worker {worker}: UPDATE '{id}' returned {output:?}"
        ));
    }
    let row = model.get_mut(&id).expect("selected from model");
    row.generation = generation;
    row.payload = new_payload;
    Ok(true)
}

fn delete_one(
    db: &Db,
    worker: usize,
    model: &mut BTreeMap<String, ExpectedRow>,
    rng: &mut XorShift64,
) -> Result<bool, String> {
    let Some(id) = pick_id(model, rng) else {
        return Ok(false);
    };
    let output = db
        .query(&format!("DELETE FROM {TABLE} WHERE id = '{id}'"))
        .map_err(|e| e.to_string())?;
    if output != QueryOutput::Affected(1) {
        return Err(format!(
            "worker {worker}: DELETE '{id}' returned {output:?}"
        ));
    }
    model.remove(&id);
    Ok(true)
}

fn run_worker(input: WorkerInput) -> Result<WorkerResult, String> {
    let WorkerInput {
        db,
        worker,
        mut model,
        seed,
        initial_rows,
        deadline,
        barrier,
        global_operations,
        stop,
    } = input;
    let mut rng = XorShift64::new(seed ^ (worker as u64).wrapping_mul(0x9E37_79B9_7F4A_7C15));
    let mut sequence = initial_rows as u64;
    let mut counts = Counts::default();
    barrier.wait();

    while !stop.load(Ordering::Relaxed) && Instant::now() < deadline {
        match rng.below(100) {
            0..=44 => {
                select_one(&db, worker, &model, &mut rng, sequence)?;
                counts.selects += 1;
            }
            45..=49 => {
                select_owner(&db, worker, &model)?;
                counts.selects += 1;
            }
            50..=64 => {
                insert_one(&db, worker, &mut model, &mut sequence)?;
                counts.inserts += 1;
            }
            65..=84 => {
                if update_one(&db, worker, &mut model, &mut rng)? {
                    counts.updates += 1;
                } else {
                    insert_one(&db, worker, &mut model, &mut sequence)?;
                    counts.inserts += 1;
                }
            }
            _ => {
                if delete_one(&db, worker, &mut model, &mut rng)? {
                    counts.deletes += 1;
                } else {
                    insert_one(&db, worker, &mut model, &mut sequence)?;
                    counts.inserts += 1;
                }
            }
        }
        global_operations.fetch_add(1, Ordering::Relaxed);
    }
    Ok(WorkerResult { model, counts })
}

fn validate_full_state(db: &Db, expected: &BTreeMap<String, ExpectedRow>) -> Result<(), String> {
    let (columns, found) = rows(
        db.query(&format!(
            "SELECT id, owner, generation, payload FROM {TABLE} ORDER BY id"
        ))
        .map_err(|e| e.to_string())?,
        "final SELECT",
    )?;
    let expected_columns = ["id", "owner", "generation", "payload"];
    if columns != expected_columns {
        return Err(format!(
            "final SELECT returned unexpected columns: {columns:?}"
        ));
    }
    let expected_rows = expected
        .iter()
        .map(|(id, row)| row.values(id))
        .collect::<Vec<_>>();
    if found != expected_rows {
        let first_difference = found
            .iter()
            .zip(&expected_rows)
            .position(|(actual, expected)| actual != expected)
            .unwrap_or(found.len().min(expected_rows.len()));
        return Err(format!(
            "final state mismatch at row {first_difference}: found {} rows, expected {}",
            found.len(),
            expected_rows.len()
        ));
    }
    Ok(())
}

fn format_bytes(bytes: u64) -> String {
    if bytes >= 1024 * 1024 {
        format!("{:.2} MiB", bytes as f64 / (1024.0 * 1024.0))
    } else if bytes >= 1024 {
        format!("{:.2} KiB", bytes as f64 / 1024.0)
    } else {
        format!("{bytes} B")
    }
}

fn directory_size(path: &Path) -> u64 {
    fn visit(path: &Path, total: &mut u64) {
        let Ok(metadata) = fs::symlink_metadata(path) else {
            return;
        };
        if metadata.is_file() {
            *total = total.saturating_add(metadata.len());
        } else if metadata.is_dir() {
            if let Ok(entries) = fs::read_dir(path) {
                for entry in entries.flatten() {
                    visit(&entry.path(), total);
                }
            }
        }
    }
    let mut total = 0;
    visit(path, &mut total);
    total
}

fn main() -> Result<(), Box<dyn StdError>> {
    let config = match Config::parse() {
        Ok(Some(config)) => config,
        Ok(None) => {
            println!("{}", usage());
            return Ok(());
        }
        Err(error) => return Err(error.into()),
    };

    if config.path.exists() {
        return Err(format!(
            "refusing to overwrite existing stress database: {}",
            config.path.display()
        )
        .into());
    }
    if let Some(parent) = config.path.parent() {
        fs::create_dir_all(parent)?;
    }

    println!("EliteSQL mixed SQL stress test");
    println!("  path:             {}", config.path.display());
    println!("  duration:         {:.3} s", config.duration.as_secs_f64());
    println!("  workers:          {}", config.workers);
    println!("  durability:       {}", durability_name(config.durability));
    println!(
        "  checkpoint:       {}",
        format_bytes(config.checkpoint_bytes)
    );
    println!("  initial rows:      {} per worker", config.initial_rows);
    println!("  operation mix:    50% SELECT / 15% INSERT / 20% UPDATE / 15% DELETE");
    println!("  seed:             0x{:016x}", config.seed);

    let db = Db::create_with(
        &config.path,
        DbOptions {
            durability: config.durability,
            memtable_max_bytes: config.checkpoint_bytes,
            ..DbOptions::default()
        },
    )?;
    db.query(
        "CREATE TABLE stress_rows (\
         owner int NOT NULL, generation int NOT NULL, payload text NOT NULL)",
    )?;

    let mut initial_models = Vec::with_capacity(config.workers);
    for worker in 0..config.workers {
        initial_models.push(seed_worker(&db, worker, config.initial_rows)?);
    }

    let db = Arc::new(db);
    let barrier = Arc::new(Barrier::new(config.workers + 1));
    let global_operations = Arc::new(AtomicU64::new(0));
    let stop = Arc::new(AtomicBool::new(false));
    let started = Instant::now();
    let deadline = started + config.duration;
    let mut handles = Vec::with_capacity(config.workers);
    for (worker, model) in initial_models.into_iter().enumerate() {
        let db = Arc::clone(&db);
        let barrier = Arc::clone(&barrier);
        let global_operations = Arc::clone(&global_operations);
        let worker_stop = Arc::clone(&stop);
        let stop_on_error = Arc::clone(&stop);
        let seed = config.seed;
        let initial_rows = config.initial_rows;
        handles.push(thread::spawn(move || {
            let result = run_worker(WorkerInput {
                db,
                worker,
                model,
                seed,
                initial_rows,
                deadline,
                barrier,
                global_operations,
                stop: worker_stop,
            });
            if result.is_err() {
                stop_on_error.store(true, Ordering::Relaxed);
            }
            result
        }));
    }
    barrier.wait();

    let mut last_operations = 0u64;
    let mut last_progress = started;
    while !stop.load(Ordering::Relaxed) && Instant::now() < deadline {
        let remaining = deadline.saturating_duration_since(Instant::now());
        thread::sleep(config.progress_interval.min(remaining));
        let now = Instant::now();
        let operations = global_operations.load(Ordering::Relaxed);
        let interval = now.duration_since(last_progress).as_secs_f64();
        let rate = (operations - last_operations) as f64 / interval.max(f64::EPSILON);
        println!(
            "  progress: {:>6.1}/{:.1} s  operations={operations}  current={rate:.0} ops/s",
            now.duration_since(started).as_secs_f64(),
            config.duration.as_secs_f64()
        );
        last_operations = operations;
        last_progress = now;
    }

    let mut expected = BTreeMap::new();
    let mut counts = Counts::default();
    for (worker, handle) in handles.into_iter().enumerate() {
        let result = handle
            .join()
            .map_err(|_| format!("worker {worker} panicked"))??;
        counts.add(result.counts);
        for (id, row) in result.model {
            if expected.insert(id.clone(), row).is_some() {
                return Err(format!("duplicate model id '{id}'").into());
            }
        }
    }
    let elapsed = started.elapsed();

    println!("\nOnline validation...");
    validate_full_state(&db, &expected)?;
    db.checkpoint()?;
    validate_full_state(&db, &expected)?;
    println!("Compacting and validating again...");
    db.compact()?;
    validate_full_state(&db, &expected)?;
    let maintenance = db.maintenance_stats();
    drop(db);

    println!("Closing database and running offline integrity check...");
    let report = check(&config.path)?;
    if !report.is_ok() {
        return Err(format!("offline integrity errors: {:?}", report.errors).into());
    }
    if !report.warnings.is_empty() {
        return Err(format!("offline integrity warnings: {:?}", report.warnings).into());
    }

    println!("Reopening database and comparing every row...");
    let reopened = Db::open(&config.path)?;
    validate_full_state(&reopened, &expected)?;
    drop(reopened);

    let operations = counts.total();
    println!("\nPASS: no corruption or logical inconsistency detected");
    println!("  elapsed:          {:.3} s", elapsed.as_secs_f64());
    println!("  operations:       {operations}");
    println!(
        "  throughput:       {:.0} ops/s",
        operations as f64 / elapsed.as_secs_f64()
    );
    println!("  SELECT:           {}", counts.selects);
    println!("  INSERT:           {}", counts.inserts);
    println!("  UPDATE:           {}", counts.updates);
    println!("  DELETE:           {}", counts.deletes);
    println!("  final live rows:  {}", expected.len());
    println!("  checkpoints:      {}", maintenance.checkpoints);
    println!(
        "  checkpoint time: {:.3} s",
        maintenance.checkpoint_time.as_secs_f64()
    );
    println!(
        "  database size:    {}",
        format_bytes(directory_size(&config.path))
    );
    println!("  retained at:      {}", config.path.display());
    Ok(())
}
