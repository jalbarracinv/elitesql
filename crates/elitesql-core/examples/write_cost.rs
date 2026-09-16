//! Does a commit cost more as more is written between publications?
//!
//! Prints the time of successive blocks of autocommit inserts into a table
//! with three secondary indexes, one of them unique. A flat column is the
//! answer we want; a rising one means something in the commit path is
//! proportional to what has already been written.
//!
//! `cargo run --release -p elitesql-core --example write_cost [blocks] [rows]`
use std::time::Instant;

use elitesql_core::{Db, DbOptions, Durability, MemoryOptions, Value};

fn main() {
    let mut args = std::env::args().skip(1);
    let blocks: usize = args.next().and_then(|a| a.parse().ok()).unwrap_or(16);
    let rows: i64 = args.next().and_then(|a| a.parse().ok()).unwrap_or(12_000);
    let dir = tempfile::tempdir().expect("temp dir");
    let db = Db::create_with(
        dir.path().join("write.esql"),
        DbOptions {
            durability: Durability::Balanced,
            memory: MemoryOptions {
                index_delta_pool_bytes: 24 * 1024 * 1024,
                ..MemoryOptions::default()
            },
            ..DbOptions::default()
        },
    )
    .expect("create");
    db.query(
        "CREATE TABLE events (token text NOT NULL, owner int64 NOT NULL, \
         topic text NOT NULL, note text NOT NULL)",
    )
    .expect("ddl");
    db.query("CREATE UNIQUE INDEX ON events (token)")
        .expect("unique index");
    db.query("CREATE INDEX ON events (owner)").expect("index");
    db.query("CREATE INDEX ON events (topic)").expect("index");

    // `update` keeps the table bounded, so the derived overlays fill while
    // segment and run churn stays light: that isolates what a commit costs
    // from what background maintenance costs.
    let updates = args.next().is_some_and(|mode| mode == "update");
    const LIVE: i64 = 4_000;
    if updates {
        for n in 0..LIVE {
            db.query_params(
                "INSERT INTO events (token, owner, topic, note) VALUES (?, ?, ?, ?)",
                &[
                    Value::Text(format!("tok-{n:09}")),
                    Value::Int64(n % 500),
                    Value::Text(format!("topic-{}", n % 40)),
                    Value::Text(format!("note {n}")),
                ],
            )
            .expect("seed");
        }
        db.checkpoint().expect("checkpoint");
    }

    println!("{:>10}{:>12}{:>12}", "writes", "us/write", "us/read");
    let mut written = if updates { LIVE } else { 0 };
    let mut done = 0i64;
    for _ in 0..blocks {
        let started = Instant::now();
        for n in done..done + rows {
            if updates {
                db.query_params(
                    "UPDATE events SET note = ?, owner = ? WHERE token = ?",
                    &[
                        Value::Text(format!("note {n}")),
                        Value::Int64(n % 500),
                        Value::Text(format!("tok-{:09}", n % LIVE)),
                    ],
                )
                .expect("update");
                continue;
            }
            db.query_params(
                "INSERT INTO events (token, owner, topic, note) VALUES (?, ?, ?, ?)",
                &[
                    Value::Text(format!("tok-{n:09}")),
                    Value::Int64(n % 500),
                    Value::Text(format!("topic-{}", n % 40)),
                    Value::Text(format!("note {n}")),
                ],
            )
            .expect("insert");
        }
        let insert = started.elapsed().as_secs_f64() * 1e6 / rows as f64;
        done += rows;
        if !updates {
            written += rows;
        }
        let started = Instant::now();
        for n in 0..2_000i64 {
            db.query_params(
                "SELECT owner FROM events WHERE token = ?",
                &[Value::Text(format!("tok-{:09}", (n * 7) % written))],
            )
            .expect("read");
        }
        let read = started.elapsed().as_secs_f64() * 1e6 / 2_000.0;
        println!("{done:>10}{insert:>12.1}{read:>12.1}");
    }
}
