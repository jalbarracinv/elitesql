//! Kill a real process while concurrent Safe transactions share WAL syncs.
//! The acknowledgement log is written only after commit returns. Every ack
//! must survive; both rows of every transaction must recover together.
//! This is process-crash coverage, not a physical power-cut experiment.

use std::collections::HashSet;
use std::fs::OpenOptions;
use std::io::Write;
use std::path::Path;
use std::sync::{Arc, Barrier};
use std::time::{Duration, Instant};

use elitesql_core::{Column, ColumnType, Db, DbOptions, Durability, Record, TableSchema, Value};

const ENV_DIR: &str = "ELITESQL_GROUP_CRASH_WORKER_DIR";
const ENV_ROUND: &str = "ELITESQL_GROUP_CRASH_ROUND";
const WRITERS: usize = 8;

fn options() -> DbOptions {
    DbOptions {
        durability: Durability::Safe,
        memtable_max_bytes: 32 * 1024,
        ..DbOptions::default()
    }
}

fn row_id(table: &str, n: i64) -> String {
    format!("{table}-{n:012}")
}

fn acknowledgements(path: &Path) -> Vec<(i64, u64)> {
    let text = std::fs::read_to_string(path).unwrap_or_default();
    text.split_inclusive('\n')
        .filter(|line| line.ends_with('\n'))
        .filter_map(|line| {
            let (n, group) = line.trim().split_once(' ')?;
            Some((n.parse().ok()?, group.parse().ok()?))
        })
        .collect()
}

#[test]
fn grouped_crash_worker() {
    let Ok(directory) = std::env::var(ENV_DIR) else {
        return;
    };
    let round: i64 = std::env::var(ENV_ROUND).unwrap().parse().unwrap();
    let db = Arc::new(Db::open_with(&directory, options()).unwrap());
    let ready = Arc::new(Barrier::new(WRITERS));
    let mut threads = Vec::new();
    for writer in 0..WRITERS {
        let db = db.clone();
        let ready = ready.clone();
        let ack_path = Path::new(&directory).join(format!("ack-{writer}.log"));
        threads.push(std::thread::spawn(move || {
            let mut ack = OpenOptions::new()
                .create(true)
                .append(true)
                .open(ack_path)
                .unwrap();
            ready.wait();
            for sequence in 1..100_000_i64 {
                let n = round * 10_000_000 + writer as i64 * 100_000 + sequence;
                let mut tx = db.begin();
                for table in ["left", "right"] {
                    let mut record = Record::new();
                    record.insert("id", Value::Text(row_id(table, n)));
                    record.insert("n", Value::Int64(n));
                    tx.insert(table, record).unwrap();
                }
                tx.commit().unwrap();
                let group = db.maintenance_stats().wal_sync_max_group_commits;
                writeln!(ack, "{n} {group}").unwrap();
                ack.sync_data().unwrap();
                if writer == 0 && sequence % 20 == 0 {
                    db.checkpoint().unwrap();
                }
            }
        }));
    }
    for thread in threads {
        thread.join().unwrap();
    }
}

#[test]
fn acknowledged_group_commits_survive_sigkill_and_remain_atomic() {
    if std::env::var(ENV_DIR).is_ok() {
        return;
    }
    let rounds: usize = std::env::var("ELITESQL_GROUP_CRASH_ITERS")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(12);
    assert!(rounds > 0);
    let tmp = tempfile::tempdir().unwrap();
    let path = tmp.path().join("group-crash.esql");
    {
        let db = Db::create_with(&path, options()).unwrap();
        for table in ["left", "right"] {
            db.create_table(TableSchema::new(
                table,
                vec![Column::new("n", ColumnType::Int64)],
            ))
            .unwrap();
        }
    }
    let mut grouped_rounds = 0;
    let mut total_acknowledged = 0;
    for round in 0..rounds {
        let mut child = std::process::Command::new(std::env::current_exe().unwrap())
            .args(["--exact", "grouped_crash_worker", "--nocapture"])
            .env(ENV_DIR, &path)
            .env(ENV_ROUND, round.to_string())
            .stdout(std::process::Stdio::null())
            .spawn()
            .unwrap();
        let deadline = Instant::now() + Duration::from_secs(15);
        loop {
            let new_acks: Vec<_> = (0..WRITERS)
                .flat_map(|w| acknowledgements(&path.join(format!("ack-{w}.log"))))
                .filter(|(n, _)| *n / 10_000_000 == round as i64)
                .collect();
            if new_acks.len() >= 32 && new_acks.iter().any(|(_, group)| *group > 1) {
                grouped_rounds += 1;
                total_acknowledged += new_acks.len();
                break;
            }
            if child.try_wait().unwrap().is_some() || Instant::now() >= deadline {
                let _ = child.kill();
                let _ = child.wait();
                panic!("round {round}: worker did not acknowledge a real shared sync group");
            }
            std::thread::sleep(Duration::from_millis(5));
        }
        std::thread::sleep(Duration::from_millis((round as u64 * 17) % 41));
        child.kill().unwrap();
        child.wait().unwrap();
        let db = Db::open_with(&path, options()).unwrap();
        for writer in 0..WRITERS {
            for (n, _) in acknowledgements(&path.join(format!("ack-{writer}.log"))) {
                for table in ["left", "right"] {
                    assert!(
                        db.get(table, &row_id(table, n)).unwrap().is_some(),
                        "round {round}: acknowledged {n} missing from {table}"
                    );
                }
            }
        }
        let sequences = |table| -> HashSet<i64> {
            db.scan(table)
                .unwrap()
                .iter()
                .map(|(_, record)| match record.get("n").unwrap() {
                    Value::Int64(n) => *n,
                    other => panic!("invalid sequence: {other:?}"),
                })
                .collect()
        };
        assert_eq!(
            sequences("left"),
            sequences("right"),
            "round {round}: torn transaction"
        );
    }
    assert_eq!(grouped_rounds, rounds);
    eprintln!("SIGKILL: {rounds} rounds with confirmed shared syncs; at least {total_acknowledged} new acknowledgements verified");
}
