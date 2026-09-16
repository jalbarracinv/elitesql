//! SQL inside a transaction must use the same access paths as autocommit
//! (physical id, indexed equality) while still reading at the transaction's
//! snapshot with its staged writes overlaid.
use elitesql_core::{Db, QueryOutput, Value};
use tempfile::TempDir;

fn rows(output: QueryOutput) -> Vec<Vec<Value>> {
    match output {
        QueryOutput::Rows { rows, .. } => rows,
        other => panic!("expected rows, got {other:?}"),
    }
}

fn affected(output: QueryOutput) -> u64 {
    match output {
        QueryOutput::Affected(n) => n,
        other => panic!("expected affected count, got {other:?}"),
    }
}

fn text(s: &str) -> Value {
    Value::Text(s.into())
}

fn new_db() -> (TempDir, Db) {
    let dir = tempfile::tempdir().unwrap();
    let db = Db::create(dir.path().join("t.esql")).unwrap();
    db.query(
        "CREATE TABLE items (id int AUTO_INCREMENT PRIMARY KEY, owner text NOT NULL, qty int NOT NULL)",
    )
    .unwrap();
    db.query("CREATE INDEX ON items (owner)").unwrap();
    for i in 0..40 {
        db.query_params(
            "INSERT INTO items (owner, qty) VALUES (?, ?)",
            &[text(&format!("u{}", i % 4)), Value::Int64(i)],
        )
        .unwrap();
    }
    (dir, db)
}

fn ids_of(rows: Vec<Vec<Value>>) -> Vec<i64> {
    let mut ids: Vec<i64> = rows
        .into_iter()
        .map(|row| match &row[0] {
            Value::Int64(id) => *id,
            other => panic!("id column should be int, got {other:?}"),
        })
        .collect();
    ids.sort_unstable();
    ids
}

#[test]
fn transactional_lookup_reads_the_snapshot_not_the_latest_index() {
    let (_dir, db) = new_db();
    let mut txn = db.begin();
    // Touch the table inside the transaction first so its snapshot is what
    // the following concurrent commits must be invisible to.
    let before = ids_of(rows(
        txn.query_params("SELECT id FROM items WHERE owner = ?", &[text("u1")])
            .unwrap(),
    ));
    assert_eq!(before, vec![2, 6, 10, 14, 18, 22, 26, 30, 34, 38]);

    // Concurrent autocommit writers: move a row out of u1, move one into u1,
    // insert a new u1 row and delete a u1 row.
    db.query("UPDATE items SET owner = 'u9' WHERE id = 2")
        .unwrap();
    db.query("UPDATE items SET owner = 'u1' WHERE id = 3")
        .unwrap();
    db.query("INSERT INTO items (owner, qty) VALUES ('u1', 100)")
        .unwrap();
    db.query("DELETE FROM items WHERE id = 38").unwrap();
    assert_eq!(
        ids_of(rows(
            db.query_params("SELECT id FROM items WHERE owner = ?", &[text("u1")])
                .unwrap()
        )),
        vec![3, 6, 10, 14, 18, 22, 26, 30, 34, 41],
        "autocommit sees the latest state"
    );

    // The transaction keeps seeing its snapshot: id 2 still u1, id 3 still
    // u3, no id 41, id 38 still present.
    let inside = ids_of(rows(
        txn.query_params("SELECT id FROM items WHERE owner = ?", &[text("u1")])
            .unwrap(),
    ));
    assert_eq!(inside, before);
    let u9 = rows(
        txn.query_params("SELECT id FROM items WHERE owner = ?", &[text("u9")])
            .unwrap(),
    );
    assert!(u9.is_empty(), "u9 was created after the snapshot");
    txn.rollback();
}

#[test]
fn transactional_lookup_overlays_staged_writes() {
    let (_dir, db) = new_db();
    let mut txn = db.begin();
    txn.query("INSERT INTO items (owner, qty) VALUES ('u1', 500)")
        .unwrap();
    txn.query("UPDATE items SET owner = 'u1' WHERE id = 1")
        .unwrap();
    txn.query("UPDATE items SET owner = 'u7' WHERE id = 6")
        .unwrap();
    txn.query("DELETE FROM items WHERE id = 10").unwrap();
    let inside = ids_of(rows(
        txn.query_params("SELECT id FROM items WHERE owner = ?", &[text("u1")])
            .unwrap(),
    ));
    assert_eq!(inside, vec![1, 2, 14, 18, 22, 26, 30, 34, 38, 41]);
    // Nothing published yet.
    assert_eq!(
        ids_of(rows(
            db.query_params("SELECT id FROM items WHERE owner = ?", &[text("u1")])
                .unwrap()
        )),
        vec![2, 6, 10, 14, 18, 22, 26, 30, 34, 38]
    );
    txn.commit().unwrap();
    assert_eq!(
        ids_of(rows(
            db.query_params("SELECT id FROM items WHERE owner = ?", &[text("u1")])
                .unwrap()
        )),
        vec![1, 2, 14, 18, 22, 26, 30, 34, 38, 41]
    );
}

#[test]
fn transactional_update_and_delete_by_indexed_column_touch_only_matches() {
    let (_dir, db) = new_db();
    let mut txn = db.begin();
    assert_eq!(
        affected(
            txn.query_params(
                "UPDATE items SET qty = qty + 1000 WHERE owner = ?",
                &[text("u2")]
            )
            .unwrap()
        ),
        10
    );
    assert_eq!(
        affected(
            txn.query_params("DELETE FROM items WHERE owner = ?", &[text("u3")])
                .unwrap()
        ),
        10
    );
    // Unique-index point lookup on the declared identity column.
    assert_eq!(
        affected(
            txn.query_params(
                "UPDATE items SET qty = 7 WHERE id = ? AND owner = ?",
                &[Value::Int64(5), text("u0")]
            )
            .unwrap()
        ),
        1
    );
    txn.commit().unwrap();
    let qty: Vec<Vec<Value>> = rows(
        db.query("SELECT sum(qty) FROM items WHERE owner = 'u2'")
            .unwrap(),
    );
    assert_eq!(
        qty[0][0],
        Value::Int64((2..40).step_by(4).sum::<i64>() + 10_000)
    );
    assert_eq!(
        rows(db.query("SELECT count(*) FROM items").unwrap())[0][0],
        Value::Int64(30)
    );
    assert_eq!(
        rows(db.query("SELECT qty FROM items WHERE id = 5").unwrap())[0][0],
        Value::Int64(7)
    );
}

#[test]
fn transactional_lookup_matches_a_scan_under_concurrent_churn() {
    // Randomised cross-check: a long transaction keeps asking through the
    // index while autocommit writers churn the same column; the answer must
    // equal a predicate scan through the same transaction every time.
    let (_dir, db) = new_db();
    let mut txn = db.begin();
    let mut seed = 0x9e3779b97f4a7c15u64;
    for round in 0..60 {
        seed ^= seed << 13;
        seed ^= seed >> 7;
        seed ^= seed << 17;
        let id = (seed % 40) as i64 + 1;
        let owner = format!("u{}", (seed >> 8) % 6);
        match round % 3 {
            0 => {
                db.query_params(
                    "UPDATE items SET owner = ? WHERE id = ?",
                    &[text(&owner), Value::Int64(id)],
                )
                .unwrap();
            }
            1 => {
                db.query_params(
                    "INSERT INTO items (owner, qty) VALUES (?, 1)",
                    &[text(&owner)],
                )
                .unwrap();
            }
            _ => {
                db.query_params("DELETE FROM items WHERE id = ?", &[Value::Int64(id)])
                    .unwrap();
            }
        }
        if round % 5 == 0 {
            txn.query_params(
                "UPDATE items SET owner = ? WHERE id = ?",
                &[text("staged"), Value::Int64(((seed >> 16) % 40) as i64 + 1)],
            )
            .ok();
        }
        for probe in ["u0", "u1", "u2", "u3", "u4", "u5", "staged"] {
            let indexed = ids_of(rows(
                txn.query_params("SELECT id FROM items WHERE owner = ?", &[text(probe)])
                    .unwrap(),
            ));
            // A closed range has no equality conjunct, so the planner scans.
            let scanned = ids_of(rows(
                txn.query_params(
                    "SELECT id FROM items WHERE owner >= ? AND owner <= ?",
                    &[text(probe), text(probe)],
                )
                .unwrap(),
            ));
            assert_eq!(indexed, scanned, "round {round}, owner {probe}");
        }
    }
    txn.rollback();
}
