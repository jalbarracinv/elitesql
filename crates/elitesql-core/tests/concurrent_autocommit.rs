//! Autocommit writes must not surface `Conflict` to the caller.
//!
//! The manual states that autocommit `UPDATE`/`DELETE` handle optimistic
//! conflict retries on their own, and that callers only need to retry inside
//! explicit transactions. These tests hold the engine to that promise under
//! the worst case for optimistic concurrency: many writers, one row.
//!
//! Two engine changes make them pass. First, `exec_update`/`exec_delete` take
//! their transaction snapshot BEFORE reading the row set; reading first left
//! a window where a concurrent commit landed inside the snapshot, invisible
//! to the write-write check, and was silently overwritten (measured: 135 of
//! 3200 retried increments lost). Second, the retry loop backs off with
//! jitter under a time budget (`WRITE_RETRY_BUDGET`) instead of a fixed
//! attempt count, so colliding writers stop retrying in lockstep.

use std::sync::Arc;
use std::thread;

use elitesql_core::{Db, DbOptions, Durability, Error, QueryOutput, Value};

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
            thread::spawn(move || {
                db.query("UPDATE contador SET valor = valor + 1 WHERE nombre = 'a'")
            })
        })
        .collect();

    for h in hilos {
        h.join()
            .expect("thread")
            .expect("autocommit update must not conflict");
    }

    assert_eq!(
        contar(&db, "SELECT valor FROM contador WHERE nombre = 'a'"),
        HILOS as i64,
        "every increment must survive the retries",
    );
}

#[test]
fn hammered_increments_never_lose_updates() {
    let dir = tempfile::tempdir().expect("tempdir");
    // Fast durability removes the per-commit fsync, so the commit rate — and
    // with it the pressure on the snapshot-before-read ordering — goes way up.
    // This is the load that exposed the lost-update window described in the
    // module docs.
    let opts = DbOptions {
        durability: Durability::Fast,
        ..DbOptions::default()
    };
    let db = Arc::new(Db::create_with(dir.path().join("martillo.esql"), opts).expect("create"));
    db.query("CREATE TABLE contador (valor int NOT NULL)")
        .expect("create");
    db.query("INSERT INTO contador (id, valor) VALUES ('c', 0)")
        .expect("insert");

    const HILOS: usize = 16;
    const VUELTAS: usize = 200;
    let hilos: Vec<_> = (0..HILOS)
        .map(|_| {
            let db = Arc::clone(&db);
            thread::spawn(move || {
                for _ in 0..VUELTAS {
                    // Caller-side retry on Conflict, exactly as the manual
                    // instructs: with it, an increment may be delayed but must
                    // never be lost.
                    loop {
                        match db.query("UPDATE contador SET valor = valor + 1 WHERE id = 'c'") {
                            Ok(_) => break,
                            Err(Error::Conflict(_)) => continue,
                            Err(error) => panic!("unexpected error: {error}"),
                        }
                    }
                }
            })
        })
        .collect();
    for hilo in hilos {
        hilo.join().expect("thread");
    }

    let esperado = (HILOS * VUELTAS) as i64;
    let valor = contar(&db, "SELECT valor FROM contador WHERE id = 'c'");
    assert_eq!(valor, esperado, "lost {} increments", esperado - valor);
}

#[test]
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

    assert!(
        fallos.is_empty(),
        "autocommit DELETE conflicted: {fallos:?}"
    );
    assert_eq!(contar(&db, "SELECT count(*) AS n FROM cola"), 0);
}
