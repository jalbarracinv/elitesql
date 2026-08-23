//! Acceptance benchmarks: EliteSQL vs SQLite on cold and sustained sequential
//! inserts plus point reads by id.
//!
//! Fairness notes:
//! - elitesql runs with `Durability::Fast` (no per-commit fsync) and SQLite
//!   with WAL + synchronous=OFF: the same (non-durable) write path class.
//! - `sqlite_autocommit` commits per insert, like elitesql's per-op commit.
//! - `*_single_txn` creates a fresh database per iteration and therefore
//!   measures the first transaction after create.
//! - `*_single_txn_steady` warms one database per Criterion sample, then times
//!   consecutive transactions so setup and first-use effects stay out of the
//!   sustained result.
//!
//! Run with: cargo bench -p elitesql-core

use std::hint::black_box;
use std::time::{Duration, Instant};

use criterion::{criterion_group, criterion_main, BatchSize, Criterion, Throughput};
use elitesql_core::{
    AutoCompactionOptions, Column, ColumnType, Db, DbOptions, Durability, Record, TableSchema,
    Value,
};
use rusqlite::Connection;
use tempfile::TempDir;

const INSERT_N: usize = 1_000;
const STEADY_WARM_TRANSACTIONS: usize = 8;
const READ_ROWS: usize = 10_000;
const BODY: &str = "The quick brown fox jumps over the lazy dog. Pack my box with \
five dozen liquor jugs. Sphinx of black quartz, judge my vow. How vexingly quick \
daft zebras jump!";

fn elitesql_new() -> (TempDir, Db) {
    let dir = tempfile::tempdir().unwrap();
    let opts = DbOptions {
        durability: Durability::Fast,
        auto_compaction: AutoCompactionOptions::disabled(),
        ..DbOptions::default()
    };
    let db = Db::create_with(dir.path().join("bench.esql"), opts).unwrap();
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

fn elitesql_record(i: usize) -> Record {
    let mut r = Record::new();
    r.insert("title".into(), Value::Text(format!("document number {i}")));
    r.insert("body".into(), Value::Text(BODY.into()));
    r.insert("score".into(), Value::Int64(i as i64));
    r
}

fn elitesql_record_with_id(i: usize) -> Record {
    let mut record = elitesql_record(i);
    record.insert("id".into(), Value::Text(format!("row-{i:08}")));
    record
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

fn sqlite_insert_rows_from(conn: &Connection, start: usize, n: usize) {
    let mut stmt = conn
        .prepare("INSERT INTO docs (id, title, body, score) VALUES (?1, ?2, ?3, ?4)")
        .unwrap();
    for i in start..start + n {
        stmt.execute(rusqlite::params![
            format!("row-{i:08}"),
            format!("document number {i}"),
            BODY,
            i as i64
        ])
        .unwrap();
    }
}

fn sqlite_insert_rows(conn: &Connection, n: usize) {
    sqlite_insert_rows_from(conn, 0, n);
}

fn elitesql_transaction(db: &Db, start: usize, explicit_ids: bool) {
    let mut txn = db.begin();
    for i in start..start + INSERT_N {
        let record = if explicit_ids {
            elitesql_record_with_id(i)
        } else {
            elitesql_record(i)
        };
        txn.insert("docs", record).unwrap();
    }
    txn.commit().unwrap();
}

fn warm_elitesql(db: &Db, explicit_ids: bool) -> usize {
    let mut next = 0usize;
    for _ in 0..STEADY_WARM_TRANSACTIONS {
        elitesql_transaction(db, next, explicit_ids);
        next += INSERT_N;
    }
    next
}

fn warm_sqlite(conn: &Connection) -> usize {
    let mut next = 0usize;
    for _ in 0..STEADY_WARM_TRANSACTIONS {
        conn.execute_batch("BEGIN").unwrap();
        sqlite_insert_rows_from(conn, next, INSERT_N);
        conn.execute_batch("COMMIT").unwrap();
        next += INSERT_N;
    }
    next
}

fn bench_elitesql_steady(iterations: u64, explicit_ids: bool) -> Duration {
    let (_dir, db) = elitesql_new();
    let mut next = warm_elitesql(&db, explicit_ids);
    let mut measured = Duration::ZERO;
    for _ in 0..iterations {
        let started = Instant::now();
        elitesql_transaction(&db, next, explicit_ids);
        measured += started.elapsed();
        next += INSERT_N;
    }
    measured
}

fn bench_sqlite_steady(iterations: u64) -> Duration {
    let (_dir, conn) = sqlite_new();
    let mut next = warm_sqlite(&conn);
    let mut measured = Duration::ZERO;
    for _ in 0..iterations {
        let started = Instant::now();
        conn.execute_batch("BEGIN").unwrap();
        sqlite_insert_rows_from(&conn, next, INSERT_N);
        conn.execute_batch("COMMIT").unwrap();
        measured += started.elapsed();
        next += INSERT_N;
    }
    measured
}

fn bench_inserts(c: &mut Criterion) {
    let mut g = c.benchmark_group("insert_1k_rows");
    g.throughput(Throughput::Elements(INSERT_N as u64));
    g.sample_size(10);

    g.bench_function("elitesql", |b| {
        b.iter_batched(
            elitesql_new,
            |(dir, db)| {
                for i in 0..INSERT_N {
                    db.insert("docs", elitesql_record(i)).unwrap();
                }
                (dir, db)
            },
            BatchSize::PerIteration,
        )
    });

    g.bench_function("elitesql_single_txn", |b| {
        b.iter_batched(
            elitesql_new,
            |(dir, db)| {
                let mut txn = db.begin();
                for i in 0..INSERT_N {
                    txn.insert("docs", elitesql_record(i)).unwrap();
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

    g.bench_function("elitesql_single_txn_steady", |b| {
        b.iter_custom(|iterations| bench_elitesql_steady(iterations, false))
    });

    g.bench_function("elitesql_single_txn_explicit_steady", |b| {
        b.iter_custom(|iterations| bench_elitesql_steady(iterations, true))
    });

    g.bench_function("sqlite_single_txn_steady", |b| {
        b.iter_custom(bench_sqlite_steady)
    });

    g.finish();
}

fn bench_gets(c: &mut Criterion) {
    let mut g = c.benchmark_group("get_by_id");
    g.sample_size(30);

    let (_elitesql_dir, db) = elitesql_new();
    let mut ids = Vec::with_capacity(READ_ROWS);
    for i in 0..READ_ROWS {
        ids.push(db.insert("docs", elitesql_record(i)).unwrap());
    }
    let mut i = 0usize;
    g.bench_function("elitesql", |b| {
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
