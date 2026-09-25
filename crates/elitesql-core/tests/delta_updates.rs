//! Delta updates (`col = col ± value`) are replayed at commit over a newer
//! committed version of their row instead of failing with a write-write
//! conflict, as long as the statement's WHERE still holds there and the
//! transaction never returned the row to its caller.
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Barrier};

use elitesql_core::{Db, Error, QueryOutput, Value};

const CHECKOUT: &str =
    "UPDATE products SET stock = stock - ?, sold = sold + ? WHERE id = ? AND stock >= ?";

fn shop(stock: &[i64]) -> (tempfile::TempDir, std::path::PathBuf, Db) {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("shop.esql");
    let db = Db::create(&path).unwrap();
    db.query(
        "CREATE TABLE products (id int AUTO_INCREMENT PRIMARY KEY, name text NOT NULL, \
         stock int NOT NULL, sold int NOT NULL DEFAULT 0)",
    )
    .unwrap();
    db.query("CREATE INDEX products_stock ON products (stock)")
        .unwrap();
    db.query("CREATE TABLE order_lines (product_id int NOT NULL, qty int NOT NULL)")
        .unwrap();
    for (index, units) in stock.iter().enumerate() {
        db.query_params(
            "INSERT INTO products (name, stock) VALUES (?, ?)",
            &[Value::Text(format!("p{index}")), Value::Int64(*units)],
        )
        .unwrap();
    }
    (dir, path, db)
}

fn int(value: &Value) -> i64 {
    match value {
        Value::Int64(value) => *value,
        other => panic!("expected an int, got {other:?}"),
    }
}

/// `(stock, sold)` of product `id`.
fn product(db: &Db, id: i64) -> (i64, i64) {
    match db
        .query_params(
            "SELECT stock, sold FROM products WHERE id = ?",
            &[Value::Int64(id)],
        )
        .unwrap()
    {
        QueryOutput::Rows { rows, .. } => (int(&rows[0][0]), int(&rows[0][1])),
        other => panic!("unexpected output {other:?}"),
    }
}

fn affected(output: QueryOutput) -> u64 {
    match output {
        QueryOutput::Affected(count) => count,
        other => panic!("unexpected output {other:?}"),
    }
}

fn buy(txn: &mut elitesql_core::Txn, id: i64, qty: i64) -> u64 {
    affected(
        txn.query_params(
            CHECKOUT,
            &[
                Value::Int64(qty),
                Value::Int64(qty),
                Value::Int64(id),
                Value::Int64(qty),
            ],
        )
        .unwrap(),
    )
}

fn rebased(db: &Db) -> u64 {
    db.maintenance_stats().delta_rebased_rows
}

#[test]
fn concurrent_decrements_of_one_row_both_commit() {
    let (_dir, _, db) = shop(&[10]);
    let mut first = db.begin();
    let mut second = db.begin();
    assert_eq!(buy(&mut first, 1, 3), 1);
    assert_eq!(buy(&mut second, 1, 4), 1);
    first.commit().unwrap();
    second.commit().unwrap();
    assert_eq!(product(&db, 1), (3, 7));
    assert_eq!(rebased(&db), 1);
}

#[test]
fn a_guard_that_fails_on_the_newer_version_is_still_a_conflict() {
    let (_dir, _, db) = shop(&[5]);
    let mut first = db.begin();
    let mut second = db.begin();
    assert_eq!(buy(&mut first, 1, 3), 1);
    assert_eq!(buy(&mut second, 1, 3), 1);
    first.commit().unwrap();
    assert!(matches!(second.commit(), Err(Error::Conflict(_))));
    assert_eq!(product(&db, 1), (2, 3));
    assert_eq!(rebased(&db), 0);
}

#[test]
fn several_deltas_to_one_row_replay_in_statement_order() {
    let (_dir, _, db) = shop(&[8, 8]);
    // 8 - 5 = 3 committed first; then 3 - 2 = 1 passes and 1 - 2 fails.
    let mut first = db.begin();
    let mut second = db.begin();
    assert_eq!(buy(&mut first, 1, 5), 1);
    assert_eq!(buy(&mut second, 1, 2), 1);
    assert_eq!(buy(&mut second, 1, 2), 1);
    first.commit().unwrap();
    assert!(matches!(second.commit(), Err(Error::Conflict(_))));
    assert_eq!(product(&db, 1), (3, 5));

    // 8 - 1 = 7 committed first; then 7 - 2 - 2 = 3.
    let mut first = db.begin();
    let mut second = db.begin();
    assert_eq!(buy(&mut first, 2, 1), 1);
    assert_eq!(buy(&mut second, 2, 2), 1);
    assert_eq!(buy(&mut second, 2, 2), 1);
    first.commit().unwrap();
    second.commit().unwrap();
    assert_eq!(product(&db, 2), (3, 5));
}

#[test]
fn a_row_the_transaction_read_is_not_replayed() {
    let (_dir, _, db) = shop(&[10, 10]);
    // Read before the delta update.
    let mut first = db.begin();
    let mut second = db.begin();
    second
        .query_params(
            "SELECT stock FROM products WHERE id = ?",
            &[Value::Int64(1)],
        )
        .unwrap();
    assert_eq!(buy(&mut second, 1, 1), 1);
    assert_eq!(buy(&mut first, 1, 1), 1);
    first.commit().unwrap();
    assert!(matches!(second.commit(), Err(Error::Conflict(_))));

    // Read after it: the caller saw the transaction's own value.
    let mut first = db.begin();
    let mut second = db.begin();
    assert_eq!(buy(&mut second, 2, 1), 1);
    second
        .query_params(
            "SELECT stock FROM products WHERE id = ?",
            &[Value::Int64(2)],
        )
        .unwrap();
    assert_eq!(buy(&mut first, 2, 1), 1);
    first.commit().unwrap();
    assert!(matches!(second.commit(), Err(Error::Conflict(_))));
    assert_eq!(product(&db, 1), (9, 1));
    assert_eq!(product(&db, 2), (9, 1));
    assert_eq!(rebased(&db), 0);
}

#[test]
fn any_other_write_to_the_row_keeps_the_conflict() {
    let (_dir, _, db) = shop(&[10, 10, 10]);
    // A plain assignment is not commutative.
    let mut first = db.begin();
    let mut second = db.begin();
    second
        .query("UPDATE products SET stock = 7 WHERE id = 1")
        .unwrap();
    assert_eq!(buy(&mut first, 1, 1), 1);
    first.commit().unwrap();
    assert!(matches!(second.commit(), Err(Error::Conflict(_))));

    // Nor is a delta followed by an assignment of another column.
    let mut first = db.begin();
    let mut second = db.begin();
    assert_eq!(buy(&mut second, 2, 1), 1);
    second
        .query("UPDATE products SET name = 'renamed' WHERE id = 2")
        .unwrap();
    assert_eq!(buy(&mut first, 2, 1), 1);
    first.commit().unwrap();
    assert!(matches!(second.commit(), Err(Error::Conflict(_))));

    // Nor is multiplication.
    let mut first = db.begin();
    let mut second = db.begin();
    second
        .query("UPDATE products SET stock = stock * 2 WHERE id = 3")
        .unwrap();
    assert_eq!(buy(&mut first, 3, 1), 1);
    first.commit().unwrap();
    assert!(matches!(second.commit(), Err(Error::Conflict(_))));
    assert_eq!(rebased(&db), 0);
}

#[test]
fn a_row_deleted_meanwhile_is_a_conflict() {
    let (_dir, _, db) = shop(&[10]);
    let mut second = db.begin();
    assert_eq!(buy(&mut second, 1, 1), 1);
    db.query("DELETE FROM products WHERE id = 1").unwrap();
    assert!(matches!(second.commit(), Err(Error::Conflict(_))));
}

#[test]
fn autocommit_deltas_replay_over_a_transaction() {
    let (_dir, _, db) = shop(&[10]);
    let mut restock = db.begin();
    restock
        .query("UPDATE products SET stock = stock + 5 WHERE id = 1")
        .unwrap();
    db.query_params(CHECKOUT, &[2, 2, 1, 2].map(Value::Int64))
        .unwrap();
    restock.commit().unwrap();
    assert_eq!(product(&db, 1), (13, 2));
}

#[test]
fn replayed_rows_keep_indexes_right_and_survive_reopening() {
    let (_dir, path, db) = shop(&[10]);
    let mut first = db.begin();
    let mut second = db.begin();
    assert_eq!(buy(&mut first, 1, 3), 1);
    assert_eq!(buy(&mut second, 1, 4), 1);
    first.commit().unwrap();
    second.commit().unwrap();
    assert_eq!(rebased(&db), 1);

    let ids_with_stock = |db: &Db, stock: i64| -> usize {
        match db
            .query_params(
                "SELECT id FROM products WHERE stock = ?",
                &[Value::Int64(stock)],
            )
            .unwrap()
        {
            QueryOutput::Rows { rows, .. } => rows.len(),
            other => panic!("unexpected output {other:?}"),
        }
    };
    // Only the final value is indexed: neither the snapshot's 10 nor the 7
    // and 6 the two transactions computed on their own.
    for (stock, expected) in [(3, 1), (10, 0), (7, 0), (6, 0)] {
        assert_eq!(ids_with_stock(&db, stock), expected, "stock = {stock}");
    }
    drop(db);
    let db = Db::open(&path).unwrap();
    assert_eq!(product(&db, 1), (3, 7));
    assert_eq!(ids_with_stock(&db, 3), 1);
    db.checkpoint().unwrap();
    drop(db);
    let db = Db::open(&path).unwrap();
    assert_eq!(product(&db, 1), (3, 7));
}

/// Many checkouts of the same few rows with stock to spare: none of them may
/// fail, and every unit is accounted for.
#[test]
fn contended_checkouts_with_stock_to_spare_never_conflict() {
    const THREADS: usize = 8;
    const PER_THREAD: usize = 200;
    let (_dir, _, db) = shop(&[1_000_000, 1_000_000, 1_000_000]);
    let db = Arc::new(db);
    let barrier = Arc::new(Barrier::new(THREADS));
    let conflicts = Arc::new(AtomicU64::new(0));
    let handles: Vec<_> = (0..THREADS)
        .map(|thread| {
            let (db, barrier, conflicts) = (db.clone(), barrier.clone(), conflicts.clone());
            std::thread::spawn(move || {
                barrier.wait();
                for round in 0..PER_THREAD {
                    let id = ((thread + round) % 3) as i64 + 1;
                    let mut txn = db.begin();
                    assert_eq!(buy(&mut txn, id, 1), 1);
                    txn.query_params(
                        "INSERT INTO order_lines (product_id, qty) VALUES (?, 1)",
                        &[Value::Int64(id)],
                    )
                    .unwrap();
                    if let Err(error) = txn.commit() {
                        assert!(matches!(error, Error::Conflict(_)), "{error}");
                        conflicts.fetch_add(1, Ordering::Relaxed);
                    }
                }
            })
        })
        .collect();
    for handle in handles {
        handle.join().unwrap();
    }
    assert_eq!(conflicts.load(Ordering::Relaxed), 0);
    let mut sold = 0;
    for id in 1..=3 {
        let (stock, sold_here) = product(&db, id);
        assert_eq!(stock + sold_here, 1_000_000);
        sold += sold_here;
    }
    assert_eq!(sold as usize, THREADS * PER_THREAD);
}

/// Scarce stock: the guard must hold across replays, so exactly the units
/// available are sold and stock never goes negative.
#[test]
fn contended_checkouts_of_scarce_stock_sell_exactly_what_exists() {
    const THREADS: usize = 8;
    const STOCK: i64 = 150;
    let (_dir, _, db) = shop(&[STOCK]);
    let db = Arc::new(db);
    let barrier = Arc::new(Barrier::new(THREADS));
    let bought = Arc::new(AtomicU64::new(0));
    let handles: Vec<_> = (0..THREADS)
        .map(|_| {
            let (db, barrier, bought) = (db.clone(), barrier.clone(), bought.clone());
            std::thread::spawn(move || {
                barrier.wait();
                loop {
                    let mut txn = db.begin();
                    if buy(&mut txn, 1, 2) == 0 {
                        return;
                    }
                    match txn.commit() {
                        Ok(_) => {
                            bought.fetch_add(2, Ordering::Relaxed);
                        }
                        Err(Error::Conflict(_)) => {}
                        Err(error) => panic!("{error}"),
                    }
                }
            })
        })
        .collect();
    for handle in handles {
        handle.join().unwrap();
    }
    assert_eq!(product(&db, 1), (0, STOCK));
    assert_eq!(bought.load(Ordering::Relaxed) as i64, STOCK);
}
