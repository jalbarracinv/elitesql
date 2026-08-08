//! Crash injection with real process kills (SIGKILL).
//!
//! The parent test re-spawns this same test binary as a worker (selected via
//! env var). The worker opens the database in `Safe` durability and commits
//! two-record transactions in a loop, appending each acknowledged sequence
//! number to an fsync'd ack log AFTER the commit returns. The parent kills
//! the worker at a random point, reopens the database and verifies:
//!
//! 1. The database always opens (recovery never wedges).
//! 2. Every acknowledged commit is present (durability of `Safe`).
//! 3. Every commit is atomic: its two records exist together or not at all,
//!    even when the kill landed mid-write.
//!
//! Iterations reuse the same database directory, so recovery itself gets
//! re-crashed and re-recovered across rounds. Override the round count with
//! ELITESQL_CRASH_ITERS (default 15; the Phase 1 acceptance runs use more).

use std::collections::HashSet;
use std::fs::OpenOptions;
use std::io::Write;
use std::path::Path;
use std::time::Duration;

use elitesql_core::{
    check, Column, ColumnType, Db, DbOptions, Durability, Record, TableSchema, Value,
};

const ENV_WORKER: &str = "ELITESQL_CRASH_WORKER_DIR";

fn worker_opts() -> DbOptions {
    DbOptions {
        durability: Durability::Safe,
        // Small memtable so checkpoints (manifest publishes, WAL rotations)
        // happen often and get crashed too.
        memtable_max_bytes: 32 * 1024,
        ..DbOptions::default()
    }
}

/// Worker mode: runs only when spawned by the parent with the env var set.
#[test]
fn crash_worker() {
    let Ok(dir) = std::env::var(ENV_WORKER) else {
        return; // normal test runs skip this
    };
    let db = Db::open_or_create_with(&dir, worker_opts()).unwrap();
    for table in ["left", "right"] {
        if !db.tables().contains(&table.to_string()) {
            db.create_table(TableSchema::new(
                table,
                vec![Column::new("n", ColumnType::Int64).not_null()],
            ))
            .unwrap();
        }
    }

    // Resume the sequence after the last acked commit.
    let ack_path = Path::new(&dir).join("ack.log");
    let mut seq: i64 = std::fs::read_to_string(&ack_path)
        .ok()
        .and_then(|s| s.lines().filter_map(|l| l.trim().parse::<i64>().ok()).max())
        .unwrap_or(0);
    let mut ack = OpenOptions::new()
        .create(true)
        .append(true)
        .open(&ack_path)
        .unwrap();

    loop {
        seq += 1;
        let mut txn = db.begin();
        let mut rec = Record::new();
        rec.insert("id".into(), Value::Text(format!("L-{seq:08}")));
        rec.insert("n".into(), Value::Int64(seq));
        txn.insert("left", rec).unwrap();
        let mut rec = Record::new();
        rec.insert("id".into(), Value::Text(format!("R-{seq:08}")));
        rec.insert("n".into(), Value::Int64(seq));
        txn.insert("right", rec).unwrap();
        txn.commit().unwrap();
        // Only after the commit is acknowledged: record it durably.
        ack.write_all(format!("{seq}\n").as_bytes()).unwrap();
        ack.sync_data().unwrap();
    }
}

#[test]
fn kill9_recovers_to_last_committed_state() {
    if std::env::var(ENV_WORKER).is_ok() {
        return; // don't recurse inside the worker process
    }
    let iterations: u32 = std::env::var("ELITESQL_CRASH_ITERS")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(15);

    let tmp = tempfile::tempdir().unwrap();
    let db_dir = tmp.path().join("crash.esql");
    let exe = std::env::current_exe().unwrap();
    let mut rng: u64 = 0x1234_5678_9ABC_DEF1;

    for round in 0..iterations {
        let mut child = std::process::Command::new(&exe)
            .args(["--exact", "crash_worker", "--nocapture"])
            .env(ENV_WORKER, db_dir.as_os_str())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .spawn()
            .unwrap();

        // Kill at a pseudo-random point: sometimes during startup/recovery,
        // sometimes mid-commit-stream.
        rng ^= rng << 13;
        rng ^= rng >> 7;
        rng ^= rng << 17;
        let sleep_ms = 20 + (rng % 180);
        std::thread::sleep(Duration::from_millis(sleep_ms));
        child.kill().unwrap(); // SIGKILL on unix
        child.wait().unwrap();

        // 1. The database must open after every crash.
        let db = Db::open_with(&db_dir, worker_opts())
            .unwrap_or_else(|e| panic!("round {round}: recovery failed: {e}"));

        let acked: Vec<i64> = std::fs::read_to_string(db_dir.join("ack.log"))
            .unwrap_or_default()
            .lines()
            .filter_map(|l| l.trim().parse().ok())
            .collect();

        // A first-round kill may land between the fixture's two independent
        // CREATE TABLE operations. That is a valid pre-workload state: no
        // transaction has been acknowledged yet, and the next worker will
        // finish creating the missing table.
        let tables = db.tables();
        if !tables.iter().any(|table| table == "left")
            || !tables.iter().any(|table| table == "right")
        {
            assert!(
                acked.is_empty(),
                "round {round}: an acknowledged commit exists but a fixture table is missing"
            );
            drop(db);
            continue;
        }

        // 2. Every acknowledged commit survived.
        for seq in &acked {
            let l = db.get("left", &format!("L-{seq:08}")).unwrap();
            let r = db.get("right", &format!("R-{seq:08}")).unwrap();
            assert!(
                l.is_some() && r.is_some(),
                "round {round}: acked commit {seq} lost (left={}, right={})",
                l.is_some(),
                r.is_some()
            );
        }

        // 3. Atomicity: every present seq has BOTH halves, acked or not.
        let left_seqs: HashSet<i64> = db
            .scan("left")
            .unwrap()
            .iter()
            .map(|(_, r)| match &r["n"] {
                Value::Int64(n) => *n,
                other => panic!("bad value {other:?}"),
            })
            .collect();
        let right_seqs: HashSet<i64> = db
            .scan("right")
            .unwrap()
            .iter()
            .map(|(_, r)| match &r["n"] {
                Value::Int64(n) => *n,
                other => panic!("bad value {other:?}"),
            })
            .collect();
        assert_eq!(
            left_seqs, right_seqs,
            "round {round}: torn commit visible — halves differ"
        );

        drop(db); // release the lock for the next worker
    }

    // After surviving all rounds, the files themselves must validate.
    let report = check(&db_dir).unwrap();
    assert!(report.is_ok(), "post-crash check: {:?}", report.errors);
}
