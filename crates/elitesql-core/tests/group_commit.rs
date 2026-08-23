use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Barrier};

use elitesql_core::{Column, ColumnType, Db, DbOptions, Durability, Record, TableSchema, Value};

#[test]
fn concurrent_safe_commits_share_syncs_and_survive_reopen() {
    const WRITERS: usize = 16;

    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("group-commit.esql");
    let db = Arc::new(
        Db::create_with(
            &path,
            DbOptions {
                durability: Durability::Safe,
                memtable_max_bytes: 64 * 1024 * 1024,
                ..DbOptions::default()
            },
        )
        .unwrap(),
    );
    db.create_table(TableSchema::new(
        "docs",
        vec![Column::new("writer", ColumnType::Int64)],
    ))
    .unwrap();

    let ready = Arc::new(Barrier::new(WRITERS));
    let mut handles = Vec::new();
    for writer in 0..WRITERS {
        let db = db.clone();
        let ready = ready.clone();
        handles.push(std::thread::spawn(move || {
            let mut transaction = db.begin();
            let mut record = Record::new();
            record.insert("id".into(), Value::Text(format!("writer-{writer:02}")));
            record.insert("writer".into(), Value::Int64(writer as i64));
            transaction.insert("docs", record).unwrap();
            ready.wait();
            transaction.commit().unwrap();
        }));
    }
    for handle in handles {
        handle.join().unwrap();
    }

    let stats = db.maintenance_stats();
    assert_eq!(stats.commits, WRITERS as u64);
    assert!(
        stats.wal_syncs < stats.commits,
        "concurrent Safe commits did not group: {stats:?}"
    );
    assert!(
        stats.grouped_commits >= 2,
        "no commit observed a shared sync group: {stats:?}"
    );
    assert_eq!(db.scan("docs").unwrap().len(), WRITERS);
    drop(db);

    let reopened = Db::open(&path).unwrap();
    assert_eq!(reopened.scan("docs").unwrap().len(), WRITERS);
}

#[test]
fn safe_group_commits_remain_durable_across_concurrent_checkpoints() {
    const WRITERS: usize = 8;
    const COMMITS_PER_WRITER: usize = 20;

    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("group-commit-checkpoint.esql");
    let db = Arc::new(
        Db::create_with(
            &path,
            DbOptions {
                durability: Durability::Safe,
                memtable_max_bytes: 64 * 1024 * 1024,
                ..DbOptions::default()
            },
        )
        .unwrap(),
    );
    db.create_table(TableSchema::new(
        "docs",
        vec![Column::new("writer", ColumnType::Int64)],
    ))
    .unwrap();

    let ready = Arc::new(Barrier::new(WRITERS + 1));
    let remaining = Arc::new(AtomicUsize::new(WRITERS));
    let mut handles = Vec::new();
    for writer in 0..WRITERS {
        let db = db.clone();
        let ready = ready.clone();
        let remaining = remaining.clone();
        handles.push(std::thread::spawn(move || {
            ready.wait();
            for commit in 0..COMMITS_PER_WRITER {
                let mut record = Record::new();
                record.insert(
                    "id".into(),
                    Value::Text(format!("writer-{writer:02}-{commit:03}")),
                );
                record.insert("writer".into(), Value::Int64(writer as i64));
                db.insert("docs", record).unwrap();
            }
            remaining.fetch_sub(1, Ordering::Release);
        }));
    }

    let checkpoint_db = db.clone();
    let checkpoint_remaining = remaining.clone();
    let checkpoint_ready = ready.clone();
    let checkpointer = std::thread::spawn(move || {
        checkpoint_ready.wait();
        while checkpoint_remaining.load(Ordering::Acquire) > 0 {
            checkpoint_db.checkpoint().unwrap();
            std::thread::yield_now();
        }
        checkpoint_db.checkpoint().unwrap();
    });

    for handle in handles {
        handle.join().unwrap();
    }
    checkpointer.join().unwrap();
    assert_eq!(db.scan("docs").unwrap().len(), WRITERS * COMMITS_PER_WRITER);
    drop(db);

    let reopened = Db::open(&path).unwrap();
    assert_eq!(
        reopened.scan("docs").unwrap().len(),
        WRITERS * COMMITS_PER_WRITER
    );
}
