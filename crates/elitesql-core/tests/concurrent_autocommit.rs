//! Autocommit writes must not surface `Conflict` to the caller.
//!
//! These tests are `#[ignore]`d because they FAIL against the current engine.
//! They document a gap between the manual and the behaviour, not a regression.
//!
//! The manual states that autocommit `UPDATE`/`DELETE` handle optimistic
//! conflict retries on their own, and that callers only need to retry inside
//! explicit transactions. Measured, that promise does not hold: with
//! `WRITE_RETRIES = 3` back-to-back attempts and no wait between them,
//! contending writers collide again in lockstep and 4 of 24 concurrent updates
//! to a single row surface `Conflict`.
//!
//! The obvious fix — backoff with jitter and a deadline instead of a fixed
//! attempt count — makes these pass, but it also makes
//! `sql_update_arithmetic::concurrent_decrements_do_not_lose_updates_or_go_negative`
//! fail about once in twelve runs. That test only breaks if a decrement is
//! applied twice, which means a `commit()` reported `Conflict` after the write
//! had already landed. A longer retry window does not create that; it exposes
//! it. Widening retries before commit is idempotent under conflict would trade
//! a visible error for silent lost or duplicated updates.
//!
//! So the retry budget stays as it is until commit is proven idempotent. Run
//! these with `cargo test -- --ignored` when working on that.

use std::sync::Arc;
use std::thread;

use elitesql_core::{Db, QueryOutput, Value};

fn abrir(dir: &tempfile::TempDir) -> Arc<Db> {
    let db = Db::open_or_create(dir.path().join("concurrencia.esql")).expect("open");
    Arc::new(db)
}

fn contar(db: &Db, sql: &str) -> i64 {
    match db.query(sql).expect("count") {
        QueryOutput::Rows { rows, .. } => match &rows[0][0] {
            Value::Int64(n) => *n,
            other => panic!("expected int, got {other:?}"),
        },
        other => panic!("expected rows, got {other:?}"),
    }
}

#[test]
#[ignore = "documents an unfixed contract gap; see module docs"]
fn concurrent_autocommit_updates_on_one_row_never_conflict() {
    let dir = tempfile::tempdir().expect("tempdir");
    let db = abrir(&dir);

    db.query("CREATE TABLE contador (nombre text NOT NULL, valor int NOT NULL DEFAULT 0)")
        .expect("create");
    db.query("INSERT INTO contador (nombre, valor) VALUES ('a', 0)")
        .expect("insert");

    // One row, many writers: the worst case for optimistic concurrency and the
    // shape a web app hits when several requests save the same record.
    const HILOS: usize = 24;
    let hilos: Vec<_> = (0..HILOS)
        .map(|i| {
            let db = Arc::clone(&db);
            thread::spawn(move || {
                db.query_params(
                    "UPDATE contador SET valor = %s WHERE nombre = 'a'",
                    &[Value::Int64(i as i64)],
                )
            })
        })
        .collect();

    let fallos: Vec<String> = hilos
        .into_iter()
        .filter_map(|h| h.join().expect("thread").err())
        .map(|e| e.to_string())
        .collect();

    assert!(
        fallos.is_empty(),
        "autocommit UPDATE surfaced {} conflicts out of {HILOS}: {:?}",
        fallos.len(),
        fallos,
    );
}

#[test]
#[ignore = "documents an unfixed contract gap; see module docs"]
fn concurrent_autocommit_arithmetic_updates_all_land() {
    let dir = tempfile::tempdir().expect("tempdir");
    let db = abrir(&dir);

    db.query("CREATE TABLE contador (nombre text NOT NULL, valor int NOT NULL DEFAULT 0)")
        .expect("create");
    db.query("INSERT INTO contador (nombre, valor) VALUES ('a', 0)")
        .expect("insert");

    // `valor = valor + 1` is read-modify-write, so a lost retry is visible in
    // the final total rather than only in an error.
    const HILOS: usize = 16;
    let hilos: Vec<_> = (0..HILOS)
        .map(|_| {
            let db = Arc::clone(&db);
            thread::spawn(move || db.query("UPDATE contador SET valor = valor + 1 WHERE nombre = 'a'"))
        })
        .collect();

    for h in hilos {
        h.join().expect("thread").expect("autocommit update must not conflict");
    }

    assert_eq!(
        contar(&db, "SELECT valor FROM contador WHERE nombre = 'a'"),
        HILOS as i64,
        "every increment must survive the retries",
    );
}

#[test]
#[ignore = "documents an unfixed contract gap; see module docs"]
fn concurrent_autocommit_deletes_never_conflict() {
    let dir = tempfile::tempdir().expect("tempdir");
    let db = abrir(&dir);

    db.query("CREATE TABLE cola (grupo text NOT NULL, n int NOT NULL)")
        .expect("create");
    for i in 0..24 {
        db.query_params(
            "INSERT INTO cola (grupo, n) VALUES ('g', %s)",
            &[Value::Int64(i)],
        )
        .expect("insert");
    }

    // Each thread deletes a different row, but they all commit against the same
    // table version, so they contend on publication.
    let hilos: Vec<_> = (0..24)
        .map(|i| {
            let db = Arc::clone(&db);
            thread::spawn(move || {
                db.query_params("DELETE FROM cola WHERE n = %s", &[Value::Int64(i)])
            })
        })
        .collect();

    let fallos: Vec<String> = hilos
        .into_iter()
        .filter_map(|h| h.join().expect("thread").err())
        .map(|e| e.to_string())
        .collect();

    assert!(fallos.is_empty(), "autocommit DELETE conflicted: {fallos:?}");
    assert_eq!(contar(&db, "SELECT count(*) AS n FROM cola"), 0);
}
