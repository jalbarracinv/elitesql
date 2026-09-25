//! Snapshots registered without the state lock must stay readable while
//! writers commit and maintenance checkpoints and compacts underneath them.
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use elitesql_core::{Column, ColumnType, Db, Error, Record, TableSchema, Value};

const ACCOUNTS: i64 = 24;
const OPENING: i64 = 1_000;

fn balance(record: &Record) -> i64 {
    match record["balance"] {
        Value::Int64(value) => value,
        ref other => panic!("unexpected balance {other:?}"),
    }
}

fn total_and_rows(rows: &[(String, Record)]) -> (i64, Vec<(String, i64)>) {
    let mut pairs: Vec<(String, i64)> = rows
        .iter()
        .map(|(id, record)| (id.clone(), balance(record)))
        .collect();
    pairs.sort();
    (pairs.iter().map(|(_, value)| value).sum(), pairs)
}

#[test]
fn snapshots_survive_concurrent_commits_checkpoints_and_compaction() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("bank.esql");
    let db = Arc::new(Db::create(&path).unwrap());
    db.create_table(TableSchema::new(
        "accounts",
        vec![Column::new("balance", ColumnType::Int64).not_null()],
    ))
    .unwrap();
    let mut ids = Vec::new();
    for _ in 0..ACCOUNTS {
        let mut record = Record::new();
        record.insert("balance", Value::Int64(OPENING));
        ids.push(db.insert("accounts", record).unwrap());
    }
    let ids = Arc::new(ids);
    let expected = ACCOUNTS * OPENING;
    let stop = Arc::new(AtomicBool::new(false));
    let transfers = Arc::new(AtomicU64::new(0));
    let checks = Arc::new(AtomicU64::new(0));
    let maintenance_runs = Arc::new(AtomicU64::new(0));

    let mut handles = Vec::new();
    for writer in 0..4u64 {
        let (db, ids, stop, transfers) = (db.clone(), ids.clone(), stop.clone(), transfers.clone());
        handles.push(std::thread::spawn(move || {
            let mut seed = writer.wrapping_mul(0x9E37_79B9_7F4A_7C15) | 1;
            while !stop.load(Ordering::Relaxed) {
                seed ^= seed << 13;
                seed ^= seed >> 7;
                seed ^= seed << 17;
                let from = &ids[(seed % ACCOUNTS as u64) as usize];
                let to = &ids[((seed >> 20) % ACCOUNTS as u64) as usize];
                if from == to {
                    continue;
                }
                let amount = (seed >> 40) as i64 % 50;
                let mut txn = db.begin();
                let from_balance = balance(&txn.get("accounts", from).unwrap().unwrap());
                let to_balance = balance(&txn.get("accounts", to).unwrap().unwrap());
                let mut debit = Record::new();
                debit.insert("balance", Value::Int64(from_balance - amount));
                let mut credit = Record::new();
                credit.insert("balance", Value::Int64(to_balance + amount));
                txn.update("accounts", from, debit).unwrap();
                txn.update("accounts", to, credit).unwrap();
                match txn.commit() {
                    Ok(_) => {
                        transfers.fetch_add(1, Ordering::Relaxed);
                    }
                    Err(Error::Conflict(_)) => {}
                    Err(error) => panic!("transfer failed: {error}"),
                }
            }
        }));
    }
    {
        let (db, stop, runs) = (db.clone(), stop.clone(), maintenance_runs.clone());
        handles.push(std::thread::spawn(move || {
            while !stop.load(Ordering::Relaxed) {
                db.checkpoint().unwrap();
                db.compact().unwrap();
                runs.fetch_add(1, Ordering::Relaxed);
                // Leave the commit lock to the writers between runs.
                std::thread::sleep(Duration::from_millis(20));
            }
        }));
    }
    for _ in 0..4 {
        let (db, ids, stop, checks, runs) = (
            db.clone(),
            ids.clone(),
            stop.clone(),
            checks.clone(),
            maintenance_runs.clone(),
        );
        handles.push(std::thread::spawn(move || {
            while !stop.load(Ordering::Relaxed) {
                let snapshot = db.snapshot();
                let (first_total, first_rows) =
                    total_and_rows(&db.scan_at(&snapshot, "accounts").unwrap());
                assert_eq!(first_total, expected, "snapshot scan broke the invariant");
                // Hold the snapshot until a whole compaction has started and
                // finished after it was taken, with commits landing meanwhile.
                let seen = runs.load(Ordering::Acquire);
                while runs.load(Ordering::Acquire) < seen + 2 && !stop.load(Ordering::Relaxed) {
                    std::thread::sleep(Duration::from_millis(1));
                }
                let mut again: Vec<(String, i64)> = ids
                    .iter()
                    .map(|id| {
                        let record = db.get_at(&snapshot, "accounts", id).unwrap().unwrap();
                        (id.clone(), balance(&record))
                    })
                    .collect();
                again.sort();
                assert_eq!(again, first_rows, "a snapshot changed under compaction");
                checks.fetch_add(1, Ordering::Relaxed);
            }
        }));
    }

    let started = Instant::now();
    while started.elapsed() < Duration::from_secs(4) {
        std::thread::sleep(Duration::from_millis(50));
    }
    stop.store(true, Ordering::Relaxed);
    for handle in handles {
        handle.join().unwrap();
    }
    eprintln!(
        "transfers {} snapshot checks {} maintenance runs {}",
        transfers.load(Ordering::Relaxed),
        checks.load(Ordering::Relaxed),
        maintenance_runs.load(Ordering::Relaxed)
    );
    assert!(transfers.load(Ordering::Relaxed) > 100, "too few transfers");
    assert!(
        checks.load(Ordering::Relaxed) > 10,
        "too few snapshot checks"
    );
    assert!(
        maintenance_runs.load(Ordering::Relaxed) > 2,
        "too few compactions"
    );

    let (total, _) = total_and_rows(&db.scan("accounts").unwrap());
    assert_eq!(total, expected);
    drop(db);
    let reopened = Db::open(&path).unwrap();
    let (total, rows) = total_and_rows(&reopened.scan("accounts").unwrap());
    assert_eq!(total, expected);
    assert_eq!(rows.len(), ACCOUNTS as usize);
}
