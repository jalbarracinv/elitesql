//! Identity sequences must never re-issue a value, and derived indexes must
//! survive updates that leave their column untouched.
use std::collections::HashSet;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use elitesql_core::{Db, Error, QueryOutput, Value};

fn rows(output: QueryOutput) -> Vec<Vec<Value>> {
    match output {
        QueryOutput::Rows { rows, .. } => rows,
        other => panic!("expected rows, got {other:?}"),
    }
}

#[test]
fn compaction_never_reissues_identities_reserved_meanwhile() {
    let dir = tempfile::tempdir().unwrap();
    let db = Arc::new(Db::create(dir.path().join("ids.esql")).unwrap());
    db.query("CREATE TABLE events (id int AUTO_INCREMENT PRIMARY KEY, kind text NOT NULL)")
        .unwrap();
    for _ in 0..200 {
        db.query("INSERT INTO events (kind) VALUES ('seed')")
            .unwrap();
    }
    // Enough superseded versions for compaction to have work to do.
    for i in 0..200 {
        db.query_params(
            "UPDATE events SET kind = 'x' WHERE id = ?",
            &[Value::Int64(i + 1)],
        )
        .unwrap();
    }
    let stop = Arc::new(AtomicBool::new(false));
    let writer_db = db.clone();
    let writer_stop = stop.clone();
    let writer = std::thread::spawn(move || {
        let mut inserted = 0u64;
        let mut unique_violations = 0u64;
        while !writer_stop.load(Ordering::Relaxed) {
            match writer_db.query("INSERT INTO events (kind) VALUES ('live')") {
                Ok(_) => inserted += 1,
                Err(Error::UniqueViolation { .. }) => unique_violations += 1,
                Err(error) => panic!("unexpected insert error: {error}"),
            }
        }
        (inserted, unique_violations)
    });
    let started = Instant::now();
    let mut compactions = 0;
    while started.elapsed() < Duration::from_secs(3) {
        db.compact().unwrap();
        compactions += 1;
    }
    stop.store(true, Ordering::Relaxed);
    let (inserted, unique_violations) = writer.join().unwrap();
    assert!(compactions >= 2, "compaction must have run repeatedly");
    assert!(inserted > 0);
    assert_eq!(
        unique_violations, 0,
        "identity values reserved during a rewrite were re-issued"
    );
    let ids: Vec<i64> = rows(db.query("SELECT id FROM events LIMIT 100000").unwrap())
        .into_iter()
        .map(|row| match row[0] {
            Value::Int64(id) => id,
            _ => unreachable!(),
        })
        .collect();
    let distinct: HashSet<i64> = ids.iter().copied().collect();
    assert_eq!(
        distinct.len(),
        ids.len(),
        "duplicate identity values stored"
    );
}

#[test]
fn updates_that_leave_indexed_columns_untouched_keep_their_entries() {
    let dir = tempfile::tempdir().unwrap();
    let db = Db::create(dir.path().join("idx.esql")).unwrap();
    db.query(
        "CREATE TABLE products (id int AUTO_INCREMENT PRIMARY KEY, category text NOT NULL, \
         description text NOT NULL, stock int NOT NULL, embedding vector(4))",
    )
    .unwrap();
    db.query("CREATE INDEX ON products (category)").unwrap();
    db.create_text_index("products", "description").unwrap();
    db.create_vector_index("products", "embedding", Default::default())
        .unwrap();
    db.query(
        "INSERT INTO products (category, description, stock, embedding) VALUES \
         ('garden', 'bamboo planter for the terrace', 10, '[1, 0, 0, 0]'), \
         ('kitchen', 'steel kettle that whistles', 5, '[0, 1, 0, 0]')",
    )
    .unwrap();
    for _ in 0..50 {
        db.query("UPDATE products SET stock = stock - 1 WHERE id = 1")
            .unwrap();
    }
    db.query("UPDATE products SET category = 'patio' WHERE id = 1")
        .unwrap();
    assert_eq!(
        rows(
            db.query("SELECT id FROM products WHERE category = 'patio'")
                .unwrap()
        ),
        vec![vec![Value::Int64(1)]]
    );
    assert!(rows(
        db.query("SELECT id FROM products WHERE category = 'garden'")
            .unwrap()
    )
    .is_empty());
    let hits = db
        .search_text("products", "description", "bamboo", 10, None)
        .unwrap();
    assert_eq!(hits.len(), 1);
    assert_eq!(
        hits[0].record["stock"],
        Value::Int64(-40),
        "text hit reads the latest row"
    );
    let hits = db
        .search_vector(
            "products",
            "embedding",
            &[1.0, 0.0, 0.0, 0.0],
            1,
            &Default::default(),
        )
        .unwrap();
    assert_eq!(hits[0].record["category"], Value::Text("patio".into()));
    db.query("UPDATE products SET description = 'clay planter for the terrace' WHERE id = 1")
        .unwrap();
    assert!(db
        .search_text("products", "description", "bamboo", 10, None)
        .unwrap()
        .is_empty());
    assert_eq!(
        db.search_text("products", "description", "clay", 10, None)
            .unwrap()
            .len(),
        1
    );
}
