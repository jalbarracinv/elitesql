//! Phase 2 acceptance benchmark: SQL queries over a 1M-row dataset,
//! including indexed joins.
//!
//! Dataset: 10K users (unique email index) + 1M orders (user_id index,
//! ~100 orders per user). Run with: cargo bench -p elitesql-core --bench sql

use std::hint::black_box;

use criterion::{criterion_group, criterion_main, Criterion};
use elitesql_core::{AutoCompactionOptions, Db, DbOptions, Durability, QueryOutput, Value};
use tempfile::TempDir;

const USERS: usize = 10_000;
const ORDERS: usize = 1_000_000;

fn build_dataset() -> (TempDir, Db) {
    let dir = tempfile::tempdir().unwrap();
    let opts = DbOptions {
        durability: Durability::Fast,
        auto_compaction: AutoCompactionOptions::disabled(),
        ..DbOptions::default()
    };
    let db = Db::create_with(dir.path().join("bench.esql"), opts).unwrap();
    db.query("CREATE TABLE users (name text NOT NULL, email text NOT NULL)")
        .unwrap();
    db.query("CREATE UNIQUE INDEX ON users (email)").unwrap();
    db.query("CREATE TABLE orders (user_id text NOT NULL, amount int64, note text)")
        .unwrap();
    db.query("CREATE INDEX ON orders (user_id)").unwrap();

    // Bulk load through the API in batched transactions.
    let mut txn = db.begin();
    for i in 0..USERS {
        let mut r = elitesql_core::Record::new();
        r.insert("id".into(), elitesql_core::Value::Text(format!("u-{i:06}")));
        r.insert(
            "name".into(),
            elitesql_core::Value::Text(format!("user {i}")),
        );
        r.insert(
            "email".into(),
            elitesql_core::Value::Text(format!("user{i}@example.com")),
        );
        txn.insert("users", r).unwrap();
        if i % 5_000 == 4_999 {
            txn.commit().unwrap();
            txn = db.begin();
        }
    }
    txn.commit().unwrap();

    let mut txn = db.begin();
    for i in 0..ORDERS {
        let mut r = elitesql_core::Record::new();
        r.insert(
            "user_id".into(),
            elitesql_core::Value::Text(format!("u-{:06}", i % USERS)),
        );
        r.insert(
            "amount".into(),
            elitesql_core::Value::Int64((i % 997) as i64),
        );
        r.insert(
            "note".into(),
            elitesql_core::Value::Text(format!("order {i}")),
        );
        txn.insert("orders", r).unwrap();
        if i % 10_000 == 9_999 {
            txn.commit().unwrap();
            txn = db.begin();
        }
    }
    txn.commit().unwrap();
    db.checkpoint().unwrap();
    (dir, db)
}

fn rows_len(out: QueryOutput) -> usize {
    match out {
        QueryOutput::Rows { rows, .. } => rows.len(),
        other => panic!("unexpected {other:?}"),
    }
}

fn bench_sql(c: &mut Criterion) {
    let (_dir, db) = build_dataset();
    let mut g = c.benchmark_group("sql_1m");
    g.sample_size(10);

    g.bench_function("point_by_unique_index", |b| {
        let mut i = 0usize;
        b.iter(|| {
            i = (i + 7919) % USERS;
            let out = db
                .query(&format!(
                    "SELECT name FROM users WHERE email = 'user{i}@example.com'"
                ))
                .unwrap();
            assert_eq!(rows_len(black_box(out)), 1);
        })
    });

    g.bench_function("point_by_unique_index_bound", |b| {
        let mut i = 0usize;
        b.iter(|| {
            i = (i + 7919) % USERS;
            let out = db
                .query_params(
                    "SELECT name FROM users WHERE email = %s",
                    &[Value::Text(format!("user{i}@example.com"))],
                )
                .unwrap();
            assert_eq!(rows_len(black_box(out)), 1);
        })
    });

    g.bench_function("indexed_join_100_of_1m", |b| {
        let mut i = 0usize;
        b.iter(|| {
            i = (i + 7919) % USERS;
            let out = db
                .query(&format!(
                    "SELECT o.amount FROM users u \
                     JOIN orders o ON o.user_id = u.id \
                     WHERE u.email = 'user{i}@example.com' \
                     ORDER BY o.amount DESC LIMIT 10"
                ))
                .unwrap();
            assert_eq!(rows_len(black_box(out)), 10);
        })
    });

    g.bench_function("indexed_join_100_of_1m_bound", |b| {
        let mut i = 0usize;
        b.iter(|| {
            i = (i + 7919) % USERS;
            let out = db
                .query_params(
                    "SELECT o.amount FROM users u \
                     JOIN orders o ON o.user_id = u.id \
                     WHERE u.email = %s \
                     ORDER BY o.amount DESC LIMIT %s",
                    &[
                        Value::Text(format!("user{i}@example.com")),
                        Value::Int64(10),
                    ],
                )
                .unwrap();
            assert_eq!(rows_len(black_box(out)), 10);
        })
    });

    g.bench_function("full_scan_filter_1m", |b| {
        b.iter(|| {
            let out = db
                .query("SELECT note FROM orders WHERE amount = 996 LIMIT 5")
                .unwrap();
            assert_eq!(rows_len(black_box(out)), 5);
        })
    });

    g.finish();
}

criterion_group!(benches, bench_sql);
criterion_main!(benches);
