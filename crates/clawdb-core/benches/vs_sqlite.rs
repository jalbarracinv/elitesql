//! Phase 0 acceptance benchmarks: clawdb vs SQLite on sequential inserts and
//! point reads by id.
//!
//! Fairness notes:
//! - clawdb runs with `Durability::Fast` (no per-commit fsync) and SQLite
//!   with WAL + synchronous=OFF: the same (non-durable) write path class.
//! - `sqlite_autocommit` commits per insert, like clawdb's per-op commit.
//!   `sqlite_single_txn` is SQLite's best case (one transaction), shown for
//!   transparency.
//!
//! Run with: cargo bench -p clawdb-core

use std::hint::black_box;

use clawdb_core::{Column, ColumnType, Db, DbOptions, Durability, Record, TableSchema, Value};
use criterion::{criterion_group, criterion_main, BatchSize, Criterion, Throughput};
use rusqlite::Connection;
use tempfile::TempDir;

const INSERT_N: usize = 1_000;
const READ_ROWS: usize = 10_000;
const BODY: &str = "The quick brown fox jumps over the lazy dog. Pack my box with \
five dozen liquor jugs. Sphinx of black quartz, judge my vow. How vexingly quick \
daft zebras jump!";

fn clawdb_new() -> (TempDir, Db) {
    let dir = tempfile::tempdir().unwrap();
    let opts = DbOptions {
        durability: Durability::Fast,
        ..DbOptions::default()
    };
    let db = Db::create_with(dir.path().join("bench.clawdb"), opts).unwrap();
    db.create_table(TableSchema::new(
        "docs",
        vec![
            Column::new("title", ColumnType::Text).not_null(),
            Column::new("body", ColumnType::Text),
            Column::new("score", ColumnType::Int64),
        ],
    ))
    .unwrap();
    (dir, db)
}

fn clawdb_record(i: usize) -> Record {
    let mut r = Record::new();
    r.insert("title".into(), Value::Text(format!("document number {i}")));
    r.insert("body".into(), Value::Text(BODY.into()));
    r.insert("score".into(), Value::Int64(i as i64));
    r
}

fn sqlite_new() -> (TempDir, Connection) {
    let dir = tempfile::tempdir().unwrap();
    let conn = Connection::open(dir.path().join("bench.sqlite3")).unwrap();
    conn.execute_batch(
        "PRAGMA journal_mode=WAL;\n\
         PRAGMA synchronous=OFF;\n\
         CREATE TABLE docs (id TEXT PRIMARY KEY, title TEXT NOT NULL, body TEXT, score INTEGER);",
    )
    .unwrap();
    (dir, conn)
}

fn sqlite_insert_rows(conn: &Connection, n: usize) {
    let mut stmt = conn
        .prepare("INSERT INTO docs (id, title, body, score) VALUES (?1, ?2, ?3, ?4)")
        .unwrap();
    for i in 0..n {
        stmt.execute(rusqlite::params![
            format!("row-{i:08}"),
            format!("document number {i}"),
            BODY,
            i as i64
        ])
        .unwrap();
    }
}

fn bench_inserts(c: &mut Criterion) {
    let mut g = c.benchmark_group("insert_1k_rows");
    g.throughput(Throughput::Elements(INSERT_N as u64));
    g.sample_size(10);

    g.bench_function("clawdb", |b| {
        b.iter_batched(
            clawdb_new,
            |(dir, db)| {
                for i in 0..INSERT_N {
                    db.insert("docs", clawdb_record(i)).unwrap();
                }
                (dir, db)
            },
            BatchSize::PerIteration,
        )
    });

    g.bench_function("clawdb_single_txn", |b| {
        b.iter_batched(
            clawdb_new,
            |(dir, db)| {
                let mut txn = db.begin();
                for i in 0..INSERT_N {
                    txn.insert("docs", clawdb_record(i)).unwrap();
                }
                txn.commit().unwrap();
                (dir, db)
            },
            BatchSize::PerIteration,
        )
    });

    g.bench_function("sqlite_autocommit", |b| {
        b.iter_batched(
            sqlite_new,
            |(dir, conn)| {
                sqlite_insert_rows(&conn, INSERT_N);
                (dir, conn)
            },
            BatchSize::PerIteration,
        )
    });

    g.bench_function("sqlite_single_txn", |b| {
        b.iter_batched(
            sqlite_new,
            |(dir, conn)| {
                conn.execute_batch("BEGIN").unwrap();
                sqlite_insert_rows(&conn, INSERT_N);
                conn.execute_batch("COMMIT").unwrap();
                (dir, conn)
            },
            BatchSize::PerIteration,
        )
    });

    g.finish();
}

fn bench_gets(c: &mut Criterion) {
    let mut g = c.benchmark_group("get_by_id");
    g.sample_size(30);

    let (_clawdb_dir, db) = clawdb_new();
    let mut ids = Vec::with_capacity(READ_ROWS);
    for i in 0..READ_ROWS {
        ids.push(db.insert("docs", clawdb_record(i)).unwrap());
    }
    let mut i = 0usize;
    g.bench_function("clawdb", |b| {
        b.iter(|| {
            // 7919 is prime and coprime with READ_ROWS: full pseudo-random cycle.
            i = (i + 7919) % READ_ROWS;
            let rec = db.get("docs", &ids[i]).unwrap().unwrap();
            black_box(rec);
        })
    });

    let (_sqlite_dir, conn) = sqlite_new();
    conn.execute_batch("BEGIN").unwrap();
    sqlite_insert_rows(&conn, READ_ROWS);
    conn.execute_batch("COMMIT").unwrap();
    let mut stmt = conn
        .prepare("SELECT title, body, score FROM docs WHERE id = ?1")
        .unwrap();
    let mut j = 0usize;
    g.bench_function("sqlite", |b| {
        b.iter(|| {
            j = (j + 7919) % READ_ROWS;
            let row: (String, String, i64) = stmt
                .query_row([format!("row-{j:08}")], |r| {
                    Ok((r.get(0)?, r.get(1)?, r.get(2)?))
                })
                .unwrap();
            black_box(row);
        })
    });

    g.finish();
}

criterion_group!(benches, bench_inserts, bench_gets);
criterion_main!(benches);
