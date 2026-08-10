use elitesql_core::{Db, Error, QueryOutput, Value};

#[test]
fn insert_ignore_and_on_conflict_only_suppress_uniqueness_errors() {
    let dir = tempfile::tempdir().unwrap();
    let db = Db::create(dir.path().join("ignore.esql")).unwrap();
    db.query("CREATE TABLE users (user_id int AUTO_INCREMENT, email text NOT NULL)")
        .unwrap();
    db.query("CREATE UNIQUE INDEX ON users(email)").unwrap();
    db.query("INSERT INTO users (email) VALUES ('a@example.test')")
        .unwrap();

    let ignored = db
        .query("INSERT IGNORE INTO users (email) VALUES ('a@example.test')")
        .unwrap();
    assert!(matches!(
        ignored,
        QueryOutput::InsertedIdentity { ref ids, .. } if ids.is_empty()
    ));

    let returned = db
        .query("INSERT INTO users (email) VALUES ('a@example.test'), ('b@example.test') ON CONFLICT DO NOTHING RETURNING user_id, email")
        .unwrap();
    assert!(matches!(
        returned,
        QueryOutput::Rows { ref rows, .. }
            if rows == &vec![vec![Value::Int64(4), Value::Text("b@example.test".into())]]
    ));
    assert_eq!(db.scan("users").unwrap().len(), 2);

    assert!(db
        .query("INSERT IGNORE INTO users (email) VALUES (123)")
        .is_err());
}

#[test]
fn insert_ignore_does_not_hide_foreign_key_or_not_null_errors() {
    let dir = tempfile::tempdir().unwrap();
    let db = Db::create(dir.path().join("ignore-errors.esql")).unwrap();
    db.query("CREATE TABLE parents (parent_id int AUTO_INCREMENT, label text NOT NULL)")
        .unwrap();
    db.query(
        "CREATE TABLE children (parent_id int REFERENCES parents(parent_id), label text NOT NULL)",
    )
    .unwrap();

    assert!(matches!(
        db.query("INSERT IGNORE INTO children (parent_id, label) VALUES (99, 'orphan')"),
        Err(Error::SchemaViolation(message)) if message.contains("foreign key violation")
    ));
    assert!(matches!(
        db.query("INSERT IGNORE INTO children (parent_id, label) VALUES (NULL, NULL)"),
        Err(Error::SchemaViolation(_))
    ));
}
