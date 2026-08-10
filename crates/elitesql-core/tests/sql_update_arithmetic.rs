use std::sync::Arc;
use std::thread;

use elitesql_core::{Db, Error, QueryOutput, Value};

fn affected(output: QueryOutput) -> u64 {
    match output {
        QueryOutput::Affected(value) => value,
        other => panic!("expected affected count, got {other:?}"),
    }
}

fn credit(db: &Db) -> i64 {
    match db.query("SELECT credits FROM users").unwrap() {
        QueryOutput::Rows { rows, .. } => match rows[0][0] {
            Value::Int64(value) => value,
            ref other => panic!("expected int, got {other:?}"),
        },
        other => panic!("expected rows, got {other:?}"),
    }
}

#[test]
fn conditional_decrement_is_atomic_and_reports_insufficient_credit() {
    let dir = tempfile::tempdir().unwrap();
    let db = Db::create(dir.path().join("credits.esql")).unwrap();
    db.query("CREATE TABLE users (credits int NOT NULL)")
        .unwrap();
    db.query("INSERT INTO users (id, credits) VALUES ('u1', 3)")
        .unwrap();

    assert_eq!(
        affected(
            db.query_params(
                "UPDATE users SET credits = credits - %s WHERE id = %s AND credits >= %s",
                &[Value::Int64(2), Value::Text("u1".into()), Value::Int64(2)],
            )
            .unwrap()
        ),
        1
    );
    assert_eq!(credit(&db), 1);
    assert_eq!(
        affected(
            db.query_params(
                "UPDATE users SET credits = credits - %s WHERE id = %s AND credits >= %s",
                &[Value::Int64(2), Value::Text("u1".into()), Value::Int64(2)],
            )
            .unwrap()
        ),
        0
    );
    assert_eq!(credit(&db), 1);
}

#[test]
fn arithmetic_detects_overflow_division_by_zero_and_invalid_shapes() {
    let dir = tempfile::tempdir().unwrap();
    let db = Db::create(dir.path().join("errors.esql")).unwrap();
    db.query("CREATE TABLE values_table (n int NOT NULL, f float64 NOT NULL, label text NOT NULL)")
        .unwrap();
    db.query(&format!(
        "INSERT INTO values_table (id, n, f, label) VALUES ('v1', {}, 4.0, 'x')",
        i64::MAX
    ))
    .unwrap();

    assert!(matches!(
        db.query("UPDATE values_table SET n = n + 1 WHERE id = 'v1'"),
        Err(Error::Sql(message)) if message.contains("overflow")
    ));
    assert!(matches!(
        db.query("UPDATE values_table SET n = n / 0 WHERE id = 'v1'"),
        Err(Error::Sql(message)) if message.contains("division by zero")
    ));
    assert!(matches!(
        db.query("UPDATE values_table SET label = label + 'x' WHERE id = 'v1'"),
        Err(Error::Sql(message)) if message.contains("numeric")
    ));
    assert!(matches!(
        db.query("UPDATE values_table SET n = f + 1 WHERE id = 'v1'"),
        Err(Error::Sql(message)) if message.contains("from itself")
    ));
    assert_eq!(credit_like(&db, "n"), i64::MAX);
}

fn credit_like(db: &Db, column: &str) -> i64 {
    let sql = format!("SELECT {column} FROM values_table");
    match db.query(&sql).unwrap() {
        QueryOutput::Rows { rows, .. } => match rows[0][0] {
            Value::Int64(value) => value,
            ref other => panic!("expected int, got {other:?}"),
        },
        other => panic!("expected rows, got {other:?}"),
    }
}

#[test]
fn concurrent_decrements_do_not_lose_updates_or_go_negative() {
    let dir = tempfile::tempdir().unwrap();
    let db = Arc::new(Db::create(dir.path().join("concurrent-credits.esql")).unwrap());
    db.query("CREATE TABLE users (credits int NOT NULL)")
        .unwrap();
    db.query("INSERT INTO users (id, credits) VALUES ('u1', 24)")
        .unwrap();

    let mut workers = Vec::new();
    for _ in 0..24 {
        let db = db.clone();
        workers.push(thread::spawn(move || loop {
            match db
                .query("UPDATE users SET credits = credits - 1 WHERE id = 'u1' AND credits >= 1")
            {
                Ok(output) => {
                    assert_eq!(affected(output), 1);
                    break;
                }
                Err(Error::Conflict(_)) => continue,
                Err(error) => panic!("unexpected decrement error: {error}"),
            }
        }));
    }
    for worker in workers {
        worker.join().unwrap();
    }
    assert_eq!(credit(&db), 0);
    assert_eq!(
        affected(
            db.query("UPDATE users SET credits = credits - 1 WHERE id = 'u1' AND credits >= 1")
                .unwrap()
        ),
        0
    );
}
