//! Repeated-SIGKILL mixed SQL stress test with a durable external oracle.
//!
//! The parent repeatedly starts a workload child, kills it at a random point,
//! reopens EliteSQL, resolves any commit whose acknowledgement was interrupted,
//! and compares the complete database against the recovered oracle.
//!
//! Reference run:
//!   cargo run --release -p elitesql-core --example crash_stress -- --duration 3h

use std::collections::BTreeMap;
use std::env;
use std::error::Error as StdError;
use std::fs::{self, File, OpenOptions};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Barrier, Mutex};
use std::thread;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use elitesql_core::{check, Db, DbOptions, Durability, QueryOutput, Value};
use serde::{Deserialize, Serialize};

const TABLE: &str = "stress_rows";
const DB_DIR: &str = "database.esql";
const SNAPSHOT_FILE: &str = "oracle.json";
const JOURNAL_FILE: &str = "oracle.log";
const METRICS_FILE: &str = "worker-metrics.json";

#[derive(Debug)]
struct Config {
    duration: Duration,
    workers: usize,
    initial_rows: usize,
    checkpoint_bytes: u64,
    min_kill: Duration,
    max_kill: Duration,
    check_every: u64,
    seed: u64,
    path: PathBuf,
    worker: bool,
    recover_only: bool,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            duration: Duration::from_secs(3 * 60 * 60),
            workers: 4,
            initial_rows: 100,
            checkpoint_bytes: 256 * 1024,
            min_kill: Duration::from_millis(200),
            max_kill: Duration::from_secs(3),
            check_every: 10,
            seed: 0xC2A5_45E5_2026_0807,
            path: default_path(),
            worker: false,
            recover_only: false,
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
                "--checkpoint-bytes" => {
                    config.checkpoint_bytes =
                        parse_bytes(&required(&mut args, "--checkpoint-bytes")?)?;
                }
                "--min-kill" => {
                    config.min_kill = parse_duration(&required(&mut args, "--min-kill")?)?;
                }
                "--max-kill" => {
                    config.max_kill = parse_duration(&required(&mut args, "--max-kill")?)?;
                }
                "--check-every" => {
                    config.check_every = parse_u64(&required(&mut args, "--check-every")?)?;
                }
                "--seed" => config.seed = parse_u64(&required(&mut args, "--seed")?)?,
                "--path" => config.path = PathBuf::from(required(&mut args, "--path")?),
                "--worker" => config.worker = true,
                "--recover-only" => config.recover_only = true,
                "--smoke" => {
                    config.duration = Duration::from_secs(8);
                    config.workers = 2;
                    config.initial_rows = 20;
                    config.checkpoint_bytes = 32 * 1024;
                    config.min_kill = Duration::from_millis(50);
                    config.max_kill = Duration::from_millis(350);
                    config.check_every = 2;
                }
                "-h" | "--help" => return Ok(None),
                _ => return Err(format!("unknown argument '{arg}'\n\n{}", usage())),
            }
        }
        if config.duration.is_zero() || config.min_kill.is_zero() || config.max_kill.is_zero() {
            return Err("durations must be greater than zero".into());
        }
        if config.workers == 0 || config.initial_rows == 0 || config.check_every == 0 {
            return Err("worker, row and check counts must be greater than zero".into());
        }
        if config.checkpoint_bytes == 0 {
            return Err("--checkpoint-bytes must be greater than zero".into());
        }
        if config.min_kill > config.max_kill {
            return Err("--min-kill must not be greater than --max-kill".into());
        }
        Ok(Some(config))
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
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

#[derive(Clone, Debug, Serialize, Deserialize)]
struct OracleSnapshot {
    next_tx: u64,
    rows: BTreeMap<String, ExpectedRow>,
}

#[derive(Clone, Copy, Debug, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
enum MutationKind {
    Insert,
    Update,
    Delete,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
struct Mutation {
    kind: MutationKind,
    id: String,
    before: Option<ExpectedRow>,
    after: Option<ExpectedRow>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(tag = "event", rename_all = "lowercase")]
enum JournalEvent {
    Intent { tx: u64, mutation: Mutation },
    Commit { tx: u64 },
}

#[derive(Clone, Copy, Debug, Default, Serialize, Deserialize)]
struct Metrics {
    pid: u32,
    selects: u64,
    inserts: u64,
    updates: u64,
    deletes: u64,
    compactions: u64,
}

impl Metrics {
    fn operations(self) -> u64 {
        self.selects + self.inserts + self.updates + self.deletes
    }

    fn add(&mut self, other: Self) {
        self.selects += other.selects;
        self.inserts += other.inserts;
        self.updates += other.updates;
        self.deletes += other.deletes;
        self.compactions += other.compactions;
    }
}

#[derive(Debug, Default)]
struct AtomicMetrics {
    selects: AtomicU64,
    inserts: AtomicU64,
    updates: AtomicU64,
    deletes: AtomicU64,
    compactions: AtomicU64,
}

impl AtomicMetrics {
    fn load(&self, pid: u32) -> Metrics {
        Metrics {
            pid,
            selects: self.selects.load(Ordering::Relaxed),
            inserts: self.inserts.load(Ordering::Relaxed),
            updates: self.updates.load(Ordering::Relaxed),
            deletes: self.deletes.load(Ordering::Relaxed),
            compactions: self.compactions.load(Ordering::Relaxed),
        }
    }
}

#[derive(Debug, Default)]
struct RecoveryStats {
    journal_commits: u64,
    resolved_committed: u64,
    resolved_aborted: u64,
    torn_journal_bytes: usize,
    committed_mutations: MutationCounts,
}

#[derive(Clone, Copy, Debug, Default)]
struct MutationCounts {
    inserts: u64,
    updates: u64,
    deletes: u64,
}

impl MutationCounts {
    fn record(&mut self, kind: MutationKind) {
        match kind {
            MutationKind::Insert => self.inserts += 1,
            MutationKind::Update => self.updates += 1,
            MutationKind::Delete => self.deletes += 1,
        }
    }

    fn add(&mut self, other: Self) {
        self.inserts += other.inserts;
        self.updates += other.updates;
        self.deletes += other.deletes;
    }

    fn total(self) -> u64 {
        self.inserts + self.updates + self.deletes
    }
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

    fn below(&mut self, upper: u64) -> u64 {
        self.next() % upper
    }
}

fn usage() -> &'static str {
    "EliteSQL repeated-SIGKILL mixed SQL stress test\n\
     \n\
     Usage:\n\
       cargo run --release -p elitesql-core --example crash_stress -- [OPTIONS]\n\
     \n\
     Options:\n\
       --duration TIME          Total test duration [default: 3h]\n\
       --workers N             Concurrent workload threads [default: 4]\n\
       --initial-rows N        Initial rows per worker [default: 100]\n\
       --checkpoint-bytes N    Automatic checkpoint threshold [default: 256k]\n\
       --min-kill TIME         Earliest random SIGKILL [default: 200ms]\n\
       --max-kill TIME         Latest random SIGKILL [default: 3s]\n\
       --check-every N         Offline integrity check every N crashes [default: 10]\n\
       --seed N                Deterministic controller/workload seed\n\
       --path PATH             New run directory (existing paths are refused)\n\
       --recover-only         Recover and finalize an interrupted existing run\n\
       --smoke                 Eight-second harness check\n\
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
        u64::from_str_radix(hex, 16).map_err(|_| format!("invalid integer '{value}'"))
    } else {
        normalized
            .parse()
            .map_err(|_| format!("invalid integer '{value}'"))
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
        "target/crash-stress-runs/elitesql-{timestamp}-{}",
        std::process::id()
    ))
}

fn db_path(run_dir: &Path) -> PathBuf {
    run_dir.join(DB_DIR)
}

fn snapshot_path(run_dir: &Path) -> PathBuf {
    run_dir.join(SNAPSHOT_FILE)
}

fn journal_path(run_dir: &Path) -> PathBuf {
    run_dir.join(JOURNAL_FILE)
}

fn metrics_path(run_dir: &Path) -> PathBuf {
    run_dir.join(METRICS_FILE)
}

fn row_id(worker: usize, sequence: u64) -> String {
    format!("w{worker:04}-row-{sequence:012}")
}

fn payload(worker: usize, generation: i64) -> String {
    format!(
        "worker={worker:04};generation={generation:012};\
         repeated SIGKILL mixed SQL payload for recovery and checksum validation"
    )
}

fn expected_row(worker: usize, generation: i64) -> ExpectedRow {
    ExpectedRow {
        owner: worker as i64,
        generation,
        payload: payload(worker, generation),
    }
}

fn atomic_write(path: &Path, bytes: &[u8]) -> Result<(), String> {
    let tmp = path.with_extension(format!("tmp-{}", std::process::id()));
    let mut file = File::create(&tmp).map_err(|e| e.to_string())?;
    file.write_all(bytes).map_err(|e| e.to_string())?;
    file.sync_all().map_err(|e| e.to_string())?;
    fs::rename(&tmp, path).map_err(|e| e.to_string())?;
    if let Some(parent) = path.parent() {
        File::open(parent)
            .and_then(|directory| directory.sync_all())
            .map_err(|e| e.to_string())?;
    }
    Ok(())
}

fn write_json<T: Serialize>(path: &Path, value: &T) -> Result<(), String> {
    let bytes = serde_json::to_vec_pretty(value).map_err(|e| e.to_string())?;
    atomic_write(path, &bytes)
}

fn load_snapshot(run_dir: &Path) -> Result<OracleSnapshot, String> {
    let bytes = fs::read(snapshot_path(run_dir)).map_err(|e| e.to_string())?;
    serde_json::from_slice(&bytes).map_err(|e| format!("invalid oracle snapshot: {e}"))
}

fn write_snapshot(run_dir: &Path, snapshot: &OracleSnapshot) -> Result<(), String> {
    write_json(&snapshot_path(run_dir), snapshot)
}

fn encode_event(event: &JournalEvent) -> Result<Vec<u8>, String> {
    let json = serde_json::to_vec(event).map_err(|e| e.to_string())?;
    let checksum = crc32fast::hash(&json);
    let mut encoded = format!("{checksum:08x}\t").into_bytes();
    encoded.extend(json);
    encoded.push(b'\n');
    Ok(encoded)
}

fn append_event(journal: &Arc<Mutex<File>>, event: &JournalEvent) -> Result<(), String> {
    let encoded = encode_event(event)?;
    let mut file = journal.lock().map_err(|_| "journal mutex poisoned")?;
    file.write_all(&encoded).map_err(|e| e.to_string())?;
    file.sync_data().map_err(|e| e.to_string())
}

fn read_journal(run_dir: &Path) -> Result<(Vec<JournalEvent>, usize, u64), String> {
    let path = journal_path(run_dir);
    let data = fs::read(&path).map_err(|e| e.to_string())?;
    let valid_len = data
        .iter()
        .rposition(|byte| *byte == b'\n')
        .map_or(0, |position| position + 1);
    let torn_bytes = data.len() - valid_len;
    let mut events = Vec::new();
    let mut max_tx = 0;
    for (line_number, line) in data[..valid_len].split(|byte| *byte == b'\n').enumerate() {
        if line.is_empty() {
            continue;
        }
        let Some(tab) = line.iter().position(|byte| *byte == b'\t') else {
            return Err(format!(
                "oracle journal line {} has no checksum",
                line_number + 1
            ));
        };
        let checksum_text = std::str::from_utf8(&line[..tab]).map_err(|_| {
            format!(
                "oracle journal line {} has invalid checksum text",
                line_number + 1
            )
        })?;
        let expected = u32::from_str_radix(checksum_text, 16).map_err(|_| {
            format!(
                "oracle journal line {} has invalid checksum",
                line_number + 1
            )
        })?;
        let json = &line[tab + 1..];
        let actual = crc32fast::hash(json);
        if actual != expected {
            return Err(format!(
                "oracle journal line {} checksum mismatch",
                line_number + 1
            ));
        }
        let event: JournalEvent = serde_json::from_slice(json)
            .map_err(|e| format!("oracle journal line {}: {e}", line_number + 1))?;
        let tx = match &event {
            JournalEvent::Intent { tx, .. } | JournalEvent::Commit { tx } => *tx,
        };
        max_tx = max_tx.max(tx);
        events.push(event);
    }
    Ok((events, torn_bytes, max_tx))
}

fn reset_journal(run_dir: &Path) -> Result<(), String> {
    let file = OpenOptions::new()
        .write(true)
        .truncate(true)
        .open(journal_path(run_dir))
        .map_err(|e| e.to_string())?;
    file.sync_all().map_err(|e| e.to_string())
}

fn apply_mutation(
    model: &mut BTreeMap<String, ExpectedRow>,
    mutation: &Mutation,
) -> Result<(), String> {
    let current = model.get(&mutation.id).cloned();
    if current != mutation.before {
        return Err(format!(
            "oracle precondition mismatch for '{}': current={current:?}, journal={:?}",
            mutation.id, mutation.before
        ));
    }
    match &mutation.after {
        Some(row) => {
            model.insert(mutation.id.clone(), row.clone());
        }
        None => {
            model.remove(&mutation.id);
        }
    }
    Ok(())
}

fn query_rows(output: QueryOutput, context: &str) -> Result<Vec<Vec<Value>>, String> {
    match output {
        QueryOutput::Rows { rows, .. } => Ok(rows),
        other => Err(format!("{context}: expected rows, got {other:?}")),
    }
}

fn read_one(db: &Db, id: &str) -> Result<Option<ExpectedRow>, String> {
    let rows = query_rows(
        db.query(&format!(
            "SELECT owner, generation, payload FROM {TABLE} WHERE id = '{id}'"
        ))
        .map_err(|e| e.to_string())?,
        "oracle point read",
    )?;
    match rows.as_slice() {
        [] => Ok(None),
        [row] => match row.as_slice() {
            [Value::Int64(owner), Value::Int64(generation), Value::Text(payload)] => {
                Ok(Some(ExpectedRow {
                    owner: *owner,
                    generation: *generation,
                    payload: payload.clone(),
                }))
            }
            other => Err(format!("unexpected values for '{id}': {other:?}")),
        },
        _ => Err(format!(
            "primary key lookup for '{id}' returned {} rows",
            rows.len()
        )),
    }
}

fn validate_full_state(db: &Db, expected: &BTreeMap<String, ExpectedRow>) -> Result<(), String> {
    let found = query_rows(
        db.query(&format!(
            "SELECT id, owner, generation, payload FROM {TABLE} ORDER BY id"
        ))
        .map_err(|e| e.to_string())?,
        "full validation",
    )?;
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
            "database/model mismatch at row {first_difference}: found {}, expected {}",
            found.len(),
            expected_rows.len()
        ));
    }
    Ok(())
}

fn recover_cycle(config: &Config, cycle: u64) -> Result<RecoveryStats, String> {
    let mut snapshot = load_snapshot(&config.path)?;
    let (events, torn_journal_bytes, max_tx) = read_journal(&config.path)?;
    let mut pending = BTreeMap::<u64, Mutation>::new();
    let mut stats = RecoveryStats {
        torn_journal_bytes,
        ..RecoveryStats::default()
    };

    for event in events {
        match event {
            JournalEvent::Intent { tx, mutation } => {
                if pending.insert(tx, mutation).is_some() {
                    return Err(format!("duplicate intent for transaction {tx}"));
                }
            }
            JournalEvent::Commit { tx } => {
                let mutation = pending
                    .remove(&tx)
                    .ok_or_else(|| format!("commit without intent for transaction {tx}"))?;
                apply_mutation(&mut snapshot.rows, &mutation)?;
                stats.committed_mutations.record(mutation.kind);
                stats.journal_commits += 1;
            }
        }
    }

    let db = Db::open_with(
        db_path(&config.path),
        DbOptions {
            durability: Durability::Safe,
            memtable_max_bytes: config.checkpoint_bytes,
            ..DbOptions::default()
        },
    )
    .map_err(|e| format!("cycle {cycle}: database recovery failed: {e}"))?;

    for (tx, mutation) in pending {
        let actual = read_one(&db, &mutation.id)?;
        if actual == mutation.after {
            apply_mutation(&mut snapshot.rows, &mutation)?;
            stats.committed_mutations.record(mutation.kind);
            stats.resolved_committed += 1;
        } else if actual == mutation.before {
            stats.resolved_aborted += 1;
        } else {
            return Err(format!(
                "cycle {cycle}: unresolved transaction {tx} left impossible state for '{}': \
                 actual={actual:?}, before={:?}, after={:?}",
                mutation.id, mutation.before, mutation.after
            ));
        }
    }
    validate_full_state(&db, &snapshot.rows)
        .map_err(|e| format!("cycle {cycle}: post-recovery validation: {e}"))?;
    snapshot.next_tx = snapshot.next_tx.max(max_tx.saturating_add(1));
    drop(db);

    if cycle.is_multiple_of(config.check_every) {
        let report = check(db_path(&config.path)).map_err(|e| e.to_string())?;
        if !report.is_ok() || !report.warnings.is_empty() {
            return Err(format!(
                "cycle {cycle}: offline check errors={:?}, warnings={:?}",
                report.errors, report.warnings
            ));
        }
    }

    write_snapshot(&config.path, &snapshot)?;
    reset_journal(&config.path)?;
    Ok(stats)
}

fn seed_database(config: &Config) -> Result<(), String> {
    if config.path.exists() {
        return Err(format!(
            "refusing to overwrite existing crash-stress run: {}",
            config.path.display()
        ));
    }
    fs::create_dir_all(&config.path).map_err(|e| e.to_string())?;
    let db = Db::create_with(
        db_path(&config.path),
        DbOptions {
            durability: Durability::Safe,
            memtable_max_bytes: config.checkpoint_bytes,
            ..DbOptions::default()
        },
    )
    .map_err(|e| e.to_string())?;
    db.query(
        "CREATE TABLE stress_rows (\
         owner int NOT NULL, generation int NOT NULL, payload text NOT NULL)",
    )
    .map_err(|e| e.to_string())?;

    let mut model = BTreeMap::new();
    for worker in 0..config.workers {
        let mut sql =
            String::from("INSERT INTO stress_rows (id, owner, generation, payload) VALUES ");
        for sequence in 0..config.initial_rows as u64 {
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
        db.query(&sql).map_err(|e| e.to_string())?;
    }
    db.checkpoint().map_err(|e| e.to_string())?;
    validate_full_state(&db, &model)?;
    drop(db);

    write_snapshot(
        &config.path,
        &OracleSnapshot {
            next_tx: 1,
            rows: model,
        },
    )?;
    let journal = File::create(journal_path(&config.path)).map_err(|e| e.to_string())?;
    journal.sync_all().map_err(|e| e.to_string())?;
    Ok(())
}

fn pick_id(model: &BTreeMap<String, ExpectedRow>, rng: &mut XorShift64) -> Option<String> {
    if model.is_empty() {
        None
    } else {
        model
            .keys()
            .nth(rng.below(model.len() as u64) as usize)
            .cloned()
    }
}

fn select_one(
    db: &Db,
    worker: usize,
    model: &BTreeMap<String, ExpectedRow>,
    rng: &mut XorShift64,
    sequence: u64,
) -> Result<(), String> {
    let id = if !model.is_empty() && rng.below(10) != 0 {
        pick_id(model, rng).expect("model checked non-empty")
    } else {
        row_id(worker, sequence.saturating_add(1_000_000_000_000))
    };
    let actual = read_one(db, &id)?;
    let expected = model.get(&id).cloned();
    if actual != expected {
        return Err(format!(
            "worker {worker}: point SELECT mismatch for '{id}': actual={actual:?}, expected={expected:?}"
        ));
    }
    Ok(())
}

fn select_owner(
    db: &Db,
    worker: usize,
    model: &BTreeMap<String, ExpectedRow>,
) -> Result<(), String> {
    let found = query_rows(
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
            "worker {worker}: owner SELECT found {} rows, expected {}",
            found.len(),
            expected.len()
        ));
    }
    Ok(())
}

fn commit_mutation(
    db: &Db,
    journal: &Arc<Mutex<File>>,
    next_tx: &AtomicU64,
    mutation: Mutation,
) -> Result<(), String> {
    let tx = next_tx.fetch_add(1, Ordering::Relaxed);
    append_event(
        journal,
        &JournalEvent::Intent {
            tx,
            mutation: mutation.clone(),
        },
    )?;

    let output = match mutation.kind {
        MutationKind::Insert => {
            let row = mutation.after.as_ref().expect("insert has after state");
            db.query(&format!(
                "INSERT INTO {TABLE} (id, owner, generation, payload) \
                 VALUES ('{}', {}, {}, '{}')",
                mutation.id, row.owner, row.generation, row.payload
            ))
        }
        MutationKind::Update => {
            let row = mutation.after.as_ref().expect("update has after state");
            db.query(&format!(
                "UPDATE {TABLE} SET generation = {}, payload = '{}' WHERE id = '{}'",
                row.generation, row.payload, mutation.id
            ))
        }
        MutationKind::Delete => {
            db.query(&format!("DELETE FROM {TABLE} WHERE id = '{}'", mutation.id))
        }
    }
    .map_err(|e| e.to_string())?;

    let expected_output = match mutation.kind {
        MutationKind::Insert => QueryOutput::Inserted {
            ids: vec![mutation.id.clone()],
        },
        MutationKind::Update | MutationKind::Delete => QueryOutput::Affected(1),
    };
    if output != expected_output {
        return Err(format!(
            "transaction {tx} returned {output:?}, expected {expected_output:?}"
        ));
    }
    append_event(journal, &JournalEvent::Commit { tx })
}

struct WorkloadShared {
    db: Arc<Db>,
    journal: Arc<Mutex<File>>,
    next_tx: Arc<AtomicU64>,
    metrics: Arc<AtomicMetrics>,
    stop: Arc<AtomicBool>,
    error: Arc<Mutex<Option<String>>>,
    barrier: Arc<Barrier>,
    seed: u64,
}

fn worker_loop(
    worker: usize,
    mut model: BTreeMap<String, ExpectedRow>,
    mut sequence: u64,
    shared: WorkloadShared,
) {
    let WorkloadShared {
        db,
        journal,
        next_tx,
        metrics,
        stop,
        error,
        barrier,
        seed,
    } = shared;
    let mut rng = XorShift64::new(seed ^ (worker as u64).wrapping_mul(0x9E37_79B9_7F4A_7C15));
    barrier.wait();

    let result = (|| -> Result<(), String> {
        while !stop.load(Ordering::Relaxed) {
            match rng.below(1_000) {
                0..=449 => {
                    select_one(&db, worker, &model, &mut rng, sequence)?;
                    metrics.selects.fetch_add(1, Ordering::Relaxed);
                }
                450..=499 => {
                    select_owner(&db, worker, &model)?;
                    metrics.selects.fetch_add(1, Ordering::Relaxed);
                }
                500..=649 => {
                    let id = row_id(worker, sequence);
                    sequence += 1;
                    let row = expected_row(worker, 0);
                    let mutation = Mutation {
                        kind: MutationKind::Insert,
                        id: id.clone(),
                        before: None,
                        after: Some(row.clone()),
                    };
                    commit_mutation(&db, &journal, &next_tx, mutation)?;
                    model.insert(id, row);
                    metrics.inserts.fetch_add(1, Ordering::Relaxed);
                }
                650..=849 => {
                    let Some(id) = pick_id(&model, &mut rng) else {
                        continue;
                    };
                    let before = model[&id].clone();
                    let after = expected_row(worker, before.generation + 1);
                    let mutation = Mutation {
                        kind: MutationKind::Update,
                        id: id.clone(),
                        before: Some(before),
                        after: Some(after.clone()),
                    };
                    commit_mutation(&db, &journal, &next_tx, mutation)?;
                    model.insert(id, after);
                    metrics.updates.fetch_add(1, Ordering::Relaxed);
                }
                850..=997 => {
                    let Some(id) = pick_id(&model, &mut rng) else {
                        continue;
                    };
                    let before = model[&id].clone();
                    let mutation = Mutation {
                        kind: MutationKind::Delete,
                        id: id.clone(),
                        before: Some(before),
                        after: None,
                    };
                    commit_mutation(&db, &journal, &next_tx, mutation)?;
                    model.remove(&id);
                    metrics.deletes.fetch_add(1, Ordering::Relaxed);
                }
                _ => {
                    db.compact().map_err(|e| e.to_string())?;
                    metrics.compactions.fetch_add(1, Ordering::Relaxed);
                }
            }
        }
        Ok(())
    })();

    if let Err(message) = result {
        stop.store(true, Ordering::Relaxed);
        if let Ok(mut slot) = error.lock() {
            if slot.is_none() {
                *slot = Some(format!("worker {worker}: {message}"));
            }
        }
    }
}

fn next_sequence(model: &BTreeMap<String, ExpectedRow>, worker: usize, minimum: u64) -> u64 {
    let prefix = format!("w{worker:04}-row-");
    model
        .keys()
        .filter_map(|id| id.strip_prefix(&prefix))
        .filter_map(|sequence| sequence.parse::<u64>().ok())
        .max()
        .map_or(minimum, |sequence| sequence.saturating_add(1).max(minimum))
}

fn run_worker(config: &Config) -> Result<(), String> {
    let snapshot = load_snapshot(&config.path)?;
    let db = Arc::new(
        Db::open_with(
            db_path(&config.path),
            DbOptions {
                durability: Durability::Safe,
                memtable_max_bytes: config.checkpoint_bytes,
                ..DbOptions::default()
            },
        )
        .map_err(|e| e.to_string())?,
    );
    let journal = Arc::new(Mutex::new(
        OpenOptions::new()
            .append(true)
            .open(journal_path(&config.path))
            .map_err(|e| e.to_string())?,
    ));
    let next_tx = Arc::new(AtomicU64::new(snapshot.next_tx));
    let metrics = Arc::new(AtomicMetrics::default());
    let stop = Arc::new(AtomicBool::new(false));
    let error = Arc::new(Mutex::new(None));
    let barrier = Arc::new(Barrier::new(config.workers + 1));
    let pid = std::process::id();
    write_json(&metrics_path(&config.path), &metrics.load(pid))?;

    let mut by_worker = vec![BTreeMap::new(); config.workers];
    for (id, row) in snapshot.rows {
        let owner =
            usize::try_from(row.owner).map_err(|_| format!("invalid negative owner for '{id}'"))?;
        if owner >= config.workers {
            return Err(format!("owner {owner} for '{id}' exceeds worker count"));
        }
        by_worker[owner].insert(id, row);
    }

    let mut handles = Vec::with_capacity(config.workers);
    for (worker, model) in by_worker.into_iter().enumerate() {
        let sequence = next_sequence(&model, worker, config.initial_rows as u64);
        let shared = WorkloadShared {
            db: Arc::clone(&db),
            journal: Arc::clone(&journal),
            next_tx: Arc::clone(&next_tx),
            metrics: Arc::clone(&metrics),
            stop: Arc::clone(&stop),
            error: Arc::clone(&error),
            barrier: Arc::clone(&barrier),
            seed: config.seed ^ (snapshot.next_tx.rotate_left(17)),
        };
        handles.push(thread::spawn(move || {
            worker_loop(worker, model, sequence, shared)
        }));
    }
    barrier.wait();

    loop {
        thread::sleep(Duration::from_millis(250));
        write_json(&metrics_path(&config.path), &metrics.load(pid))?;
        if stop.load(Ordering::Relaxed) {
            break;
        }
    }
    for handle in handles {
        handle
            .join()
            .map_err(|_| "workload thread panicked".to_string())?;
    }
    let message = error
        .lock()
        .map_err(|_| "error mutex poisoned")?
        .clone()
        .unwrap_or_else(|| "worker stopped unexpectedly".into());
    Err(message)
}

fn spawn_worker(config: &Config) -> Result<Child, String> {
    Command::new(env::current_exe().map_err(|e| e.to_string())?)
        .args([
            "--worker",
            "--path",
            &config.path.to_string_lossy(),
            "--workers",
            &config.workers.to_string(),
            "--initial-rows",
            &config.initial_rows.to_string(),
            "--checkpoint-bytes",
            &config.checkpoint_bytes.to_string(),
            "--seed",
            &config.seed.to_string(),
        ])
        .stdin(Stdio::null())
        .stdout(Stdio::inherit())
        .stderr(Stdio::inherit())
        .spawn()
        .map_err(|e| e.to_string())
}

fn wait_then_kill(child: &mut Child, delay: Duration) -> Result<(), String> {
    let deadline = Instant::now() + delay;
    while Instant::now() < deadline {
        if let Some(status) = child.try_wait().map_err(|e| e.to_string())? {
            return Err(format!("workload process exited before SIGKILL: {status}"));
        }
        thread::sleep(
            Duration::from_millis(10).min(deadline.saturating_duration_since(Instant::now())),
        );
    }
    child
        .kill()
        .map_err(|e| format!("cannot SIGKILL workload: {e}"))?;
    let status = child.wait().map_err(|e| e.to_string())?;
    if status.success() {
        return Err("workload exited successfully instead of being killed".into());
    }
    Ok(())
}

fn read_cycle_metrics(config: &Config, pid: u32) -> Metrics {
    fs::read(metrics_path(&config.path))
        .ok()
        .and_then(|bytes| serde_json::from_slice::<Metrics>(&bytes).ok())
        .filter(|metrics| metrics.pid == pid)
        .unwrap_or_default()
}

fn random_kill_delay(config: &Config, rng: &mut XorShift64, remaining: Duration) -> Duration {
    let min_ms = config.min_kill.as_millis() as u64;
    let max_ms = config.max_kill.as_millis() as u64;
    let selected_ms = min_ms + rng.below(max_ms - min_ms + 1);
    Duration::from_millis(selected_ms).min(remaining)
}

fn run_parent(config: &Config) -> Result<(), String> {
    seed_database(config)?;
    println!("EliteSQL repeated-SIGKILL mixed SQL stress test");
    println!("  run directory:    {}", config.path.display());
    println!("  duration:         {:.1} s", config.duration.as_secs_f64());
    println!("  workers:          {}", config.workers);
    println!("  durability:       safe");
    println!("  checkpoint:       {} bytes", config.checkpoint_bytes);
    println!(
        "  SIGKILL window:   {:.0}..{:.0} ms",
        config.min_kill.as_secs_f64() * 1_000.0,
        config.max_kill.as_secs_f64() * 1_000.0
    );
    println!("  offline check:    every {} crashes", config.check_every);
    println!("  seed:             0x{:016x}", config.seed);

    let started = Instant::now();
    let deadline = started + config.duration;
    let mut rng = XorShift64::new(config.seed ^ 0xA11C_E5A5_0000_0001);
    let mut cycles = 0u64;
    let mut totals = Metrics::default();
    let mut journal_commits = 0u64;
    let mut resolved_committed = 0u64;
    let mut resolved_aborted = 0u64;
    let mut torn_journal_bytes = 0usize;
    let mut committed_mutations = MutationCounts::default();

    while Instant::now() < deadline {
        cycles += 1;
        let remaining = deadline.saturating_duration_since(Instant::now());
        let delay = random_kill_delay(config, &mut rng, remaining);
        let mut child = spawn_worker(config)?;
        let pid = child.id();
        wait_then_kill(&mut child, delay)?;
        totals.add(read_cycle_metrics(config, pid));

        let recovered = recover_cycle(config, cycles)?;
        journal_commits += recovered.journal_commits;
        resolved_committed += recovered.resolved_committed;
        resolved_aborted += recovered.resolved_aborted;
        torn_journal_bytes += recovered.torn_journal_bytes;
        committed_mutations.add(recovered.committed_mutations);

        if cycles == 1 || cycles.is_multiple_of(10) || Instant::now() >= deadline {
            let snapshot = load_snapshot(&config.path)?;
            println!(
                "  cycle={cycles:<6} elapsed={:>8.1}s ops>={:<9} live_rows={:<6} \
                 journal_commits={journal_commits:<8} reconciled={resolved_committed}/{resolved_aborted}",
                started.elapsed().as_secs_f64(),
                totals.operations(),
                snapshot.rows.len()
            );
        }
    }

    let snapshot = load_snapshot(&config.path)?;
    let db = Db::open_with(
        db_path(&config.path),
        DbOptions {
            durability: Durability::Safe,
            memtable_max_bytes: config.checkpoint_bytes,
            ..DbOptions::default()
        },
    )
    .map_err(|e| e.to_string())?;
    validate_full_state(&db, &snapshot.rows)?;
    db.checkpoint().map_err(|e| e.to_string())?;
    validate_full_state(&db, &snapshot.rows)?;
    db.compact().map_err(|e| e.to_string())?;
    validate_full_state(&db, &snapshot.rows)?;
    drop(db);
    let report = check(db_path(&config.path)).map_err(|e| e.to_string())?;
    if !report.is_ok() || !report.warnings.is_empty() {
        return Err(format!(
            "final offline check errors={:?}, warnings={:?}",
            report.errors, report.warnings
        ));
    }
    let reopened = Db::open(db_path(&config.path)).map_err(|e| e.to_string())?;
    validate_full_state(&reopened, &snapshot.rows)?;
    drop(reopened);

    println!("\nPASS: every SIGKILL recovery matched the durable oracle");
    println!(
        "  elapsed:               {:.3} s",
        started.elapsed().as_secs_f64()
    );
    println!("  SIGKILL/recovery:      {cycles}");
    println!(
        "  observed operations:   {} (lower bound)",
        totals.operations()
    );
    println!("  SELECT:                {} (lower bound)", totals.selects);
    println!("  INSERT:                {} (lower bound)", totals.inserts);
    println!("  UPDATE:                {} (lower bound)", totals.updates);
    println!("  DELETE:                {} (lower bound)", totals.deletes);
    println!(
        "  compactions:           {} (lower bound)",
        totals.compactions
    );
    println!("  journaled commits:     {journal_commits}");
    println!("  uncertain -> committed:{resolved_committed}");
    println!("  uncertain -> aborted:  {resolved_aborted}");
    println!(
        "  committed mutations:  {} (INSERT {}, UPDATE {}, DELETE {})",
        committed_mutations.total(),
        committed_mutations.inserts,
        committed_mutations.updates,
        committed_mutations.deletes
    );
    println!("  torn oracle tail bytes:{torn_journal_bytes}");
    println!("  final live rows:       {}", snapshot.rows.len());
    println!("  retained at:           {}", config.path.display());
    Ok(())
}

fn recover_and_finalize(config: &Config) -> Result<(), String> {
    if !config.path.is_dir() {
        return Err(format!(
            "crash-stress run does not exist: {}",
            config.path.display()
        ));
    }
    let recovered = recover_cycle(config, config.check_every)?;
    let snapshot = load_snapshot(&config.path)?;
    let db = Db::open_with(
        db_path(&config.path),
        DbOptions {
            durability: Durability::Safe,
            memtable_max_bytes: config.checkpoint_bytes,
            ..DbOptions::default()
        },
    )
    .map_err(|e| e.to_string())?;
    validate_full_state(&db, &snapshot.rows)?;
    db.checkpoint().map_err(|e| e.to_string())?;
    validate_full_state(&db, &snapshot.rows)?;
    db.compact().map_err(|e| e.to_string())?;
    validate_full_state(&db, &snapshot.rows)?;
    drop(db);

    let report = check(db_path(&config.path)).map_err(|e| e.to_string())?;
    if !report.is_ok() || !report.warnings.is_empty() {
        return Err(format!(
            "final offline check errors={:?}, warnings={:?}",
            report.errors, report.warnings
        ));
    }
    let reopened = Db::open(db_path(&config.path)).map_err(|e| e.to_string())?;
    validate_full_state(&reopened, &snapshot.rows)?;
    drop(reopened);

    println!("PASS: interrupted run recovered and finalized");
    println!("  journaled commits:      {}", recovered.journal_commits);
    println!("  uncertain -> committed: {}", recovered.resolved_committed);
    println!("  uncertain -> aborted:   {}", recovered.resolved_aborted);
    println!("  torn oracle tail bytes: {}", recovered.torn_journal_bytes);
    println!("  final live rows:        {}", snapshot.rows.len());
    println!("  retained at:            {}", config.path.display());
    Ok(())
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
    if config.worker {
        run_worker(&config).map_err(Into::into)
    } else if config.recover_only {
        recover_and_finalize(&config).map_err(Into::into)
    } else {
        run_parent(&config).map_err(Into::into)
    }
}
