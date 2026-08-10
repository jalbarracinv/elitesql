use elitesql_core::{Db, Error, QueryOutput, Value};

#[test]
fn varchar_longtext_and_enum_validate_without_losing_text_storage() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("text-compat.esql");
    {
        let db = Db::create(&path).unwrap();
        db.query(
            "CREATE TABLE docs (code varchar(4) NOT NULL, body longtext, status enum('draft', 'sent', 'completed') NOT NULL DEFAULT 'draft')",
        )
        .unwrap();
        db.query("INSERT INTO docs (code, body) VALUES ('ñ123', 'unbounded body')")
            .unwrap();
        assert!(matches!(
            db.query("INSERT INTO docs (code) VALUES ('12345')"),
            Err(Error::SchemaViolation(message)) if message.contains("at most 4")
        ));
        assert!(matches!(
            db.query("INSERT INTO docs (code, status) VALUES ('ok', 'unknown')"),
            Err(Error::SchemaViolation(message)) if message.contains("enum value")
        ));
        db.checkpoint().unwrap();
    }

    let db = Db::open(&path).unwrap();
    match db.query("SELECT code, body, status FROM docs").unwrap() {
        QueryOutput::Rows { rows, .. } => assert_eq!(
            rows[0],
            vec![
                Value::Text("ñ123".into()),
                Value::Text("unbounded body".into()),
                Value::Text("draft".into()),
            ]
        ),
        other => panic!("expected rows, got {other:?}"),
    }
}

#[test]
fn invalid_text_constraints_fail_during_ddl() {
    let dir = tempfile::tempdir().unwrap();
    let db = Db::create(dir.path().join("invalid.esql")).unwrap();
    assert!(db.query("CREATE TABLE bad (code varchar(0))").is_err());
    assert!(db.query("CREATE TABLE bad (state enum())").is_err());
    assert!(matches!(
        db.query("CREATE TABLE bad (state enum('a', 'a'))"),
        Err(Error::SchemaViolation(message)) if message.contains("duplicate")
    ));
    assert!(matches!(
        db.query("CREATE TABLE bad (state enum('a') DEFAULT 'b')"),
        Err(Error::SchemaViolation(message)) if message.contains("allowed enum")
    ));
}
