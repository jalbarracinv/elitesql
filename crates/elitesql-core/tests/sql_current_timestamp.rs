use elitesql_core::{Db, Error, QueryOutput, Value};

fn rows(output: QueryOutput) -> Vec<Vec<Value>> {
    match output {
        QueryOutput::Rows { rows, .. } => rows,
        other => panic!("expected rows, got {other:?}"),
    }
}

#[test]
fn current_timestamp_defaults_and_now_are_stable_per_statement() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("timestamps.esql");
    {
        let db = Db::create(&path).unwrap();
        db.query(
            "CREATE TABLE events (name text NOT NULL, created_at timestamp NOT NULL DEFAULT CURRENT_TIMESTAMP, touched_at timestamp)",
        )
        .unwrap();
        let inserted = rows(
            db.query(
                "INSERT INTO events (name, touched_at) VALUES ('one', NOW()), ('two', CURRENT_TIMESTAMP) RETURNING created_at, touched_at",
            )
            .unwrap(),
        );
        assert_eq!(inserted.len(), 2);
        assert_eq!(inserted[0][0], inserted[1][0]);
        assert_eq!(inserted[0][1], inserted[1][1]);
        assert_eq!(inserted[0][0], inserted[0][1]);

        db.query("UPDATE events SET touched_at = NOW()").unwrap();
        let updated = rows(
            db.query("SELECT touched_at FROM events ORDER BY name")
                .unwrap(),
        );
        assert_eq!(updated[0][0], updated[1][0]);
        db.checkpoint().unwrap();
    }

    let db = Db::open(&path).unwrap();
    let stored = rows(
        db.query("SELECT created_at FROM events ORDER BY name")
            .unwrap(),
    );
    assert!(matches!(stored[0][0], Value::Timestamp(value) if value > 0));
}

#[test]
fn current_timestamp_default_requires_timestamp_type() {
    let dir = tempfile::tempdir().unwrap();
    let db = Db::create(dir.path().join("bad-default.esql")).unwrap();
    assert!(matches!(
        db.query("CREATE TABLE bad (name text DEFAULT CURRENT_TIMESTAMP)"),
        Err(Error::Sql(message)) if message.contains("requires a timestamp")
    ));
}
