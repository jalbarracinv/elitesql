use elitesql_core::{Db, Error, Record, Value};

fn text_id(record: &Record) -> String {
    match &record["id"] {
        Value::Text(id) => id.clone(),
        value => panic!("expected text id, got {value:?}"),
    }
}

fn insert_parent(db: &Db, table: &str, label: &str) -> (String, i64) {
    let mut row = Record::new();
    row.insert("label".into(), Value::Text(label.into()));
    let id = db.insert(table, row).unwrap();
    let record = db.get(table, &id).unwrap().unwrap();
    let Value::Int64(public_id) = record["public_id"] else {
        panic!("expected identity")
    };
    (id, public_id)
}

#[test]
fn inline_foreign_key_rejects_orphans_and_allows_same_transaction_parent() {
    let dir = tempfile::tempdir().unwrap();
    let db = Db::create(dir.path().join("fk.esql")).unwrap();
    db.query("CREATE TABLE accounts (public_id int AUTO_INCREMENT, label text NOT NULL)")
        .unwrap();
    db.query(
        "CREATE TABLE docs (account_id int NOT NULL REFERENCES accounts(public_id), title text NOT NULL)",
    )
    .unwrap();

    assert!(matches!(
        db.query("INSERT INTO docs (account_id, title) VALUES (999, 'orphan')"),
        Err(Error::SchemaViolation(message)) if message.contains("foreign key violation")
    ));

    let mut tx = db.begin();
    let mut account = Record::new();
    account.insert("label".into(), Value::Text("same transaction".into()));
    let account_id = tx.insert("accounts", account).unwrap();
    let account = tx.get("accounts", &account_id).unwrap().unwrap();
    let public_id = account["public_id"].clone();
    let mut doc = Record::new();
    doc.insert("account_id".into(), public_id);
    doc.insert("title".into(), Value::Text("valid".into()));
    tx.insert("docs", doc).unwrap();
    tx.commit().unwrap();
    assert_eq!(db.scan("docs").unwrap().len(), 1);

    // The automatically provisioned child index is part of the FK and cannot
    // accidentally be removed.
    assert!(matches!(
        db.query("DROP INDEX ON docs(account_id)"),
        Err(Error::SchemaViolation(message)) if message.contains("required by a foreign key")
    ));
}

#[test]
fn restrict_and_cascade_are_atomic_and_persist_across_reopen() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("cascade.esql");
    {
        let db = Db::create(&path).unwrap();
        db.query("CREATE TABLE parents (public_id int AUTO_INCREMENT, label text NOT NULL)")
            .unwrap();
        db.query(
            "CREATE TABLE restricted (parent_id int REFERENCES parents(public_id) ON DELETE RESTRICT, label text)",
        )
        .unwrap();
        db.query("CREATE TABLE children (public_id int AUTO_INCREMENT, parent_id int REFERENCES parents(public_id) ON DELETE CASCADE, label text)")
            .unwrap();
        db.query("CREATE TABLE grandchildren (child_id int REFERENCES children(public_id) ON DELETE CASCADE, label text)")
            .unwrap();

        let (restricted_parent_id, restricted_public_id) =
            insert_parent(&db, "parents", "restricted");
        db.query_params(
            "INSERT INTO restricted (parent_id, label) VALUES (%s, 'keep')",
            &[Value::Int64(restricted_public_id)],
        )
        .unwrap();
        assert!(matches!(
            db.delete("parents", &restricted_parent_id),
            Err(Error::SchemaViolation(message)) if message.contains("referenced by restricted.parent_id")
        ));
        assert!(db.get("parents", &restricted_parent_id).unwrap().is_some());

        let (cascade_parent_id, cascade_public_id) = insert_parent(&db, "parents", "cascade");
        db.query_params(
            "INSERT INTO children (parent_id, label) VALUES (%s, 'child')",
            &[Value::Int64(cascade_public_id)],
        )
        .unwrap();
        let child = db.scan("children").unwrap().pop().unwrap().1;
        let Value::Int64(child_public_id) = child["public_id"] else {
            panic!("expected child identity")
        };
        db.query_params(
            "INSERT INTO grandchildren (child_id, label) VALUES (%s, 'grandchild')",
            &[Value::Int64(child_public_id)],
        )
        .unwrap();

        assert!(db.delete("parents", &cascade_parent_id).unwrap());
        assert!(db.scan("children").unwrap().is_empty());
        assert!(db.scan("grandchildren").unwrap().is_empty());
        db.checkpoint().unwrap();
    }

    let db = Db::open(&path).unwrap();
    assert!(db.scan("children").unwrap().is_empty());
    assert!(db.scan("grandchildren").unwrap().is_empty());
    assert_eq!(db.scan("parents").unwrap().len(), 1);
    assert_eq!(db.scan("restricted").unwrap().len(), 1);
}

#[test]
fn concurrent_child_insert_cannot_race_past_parent_delete() {
    let dir = tempfile::tempdir().unwrap();
    let db = Db::create(dir.path().join("race.esql")).unwrap();
    db.query("CREATE TABLE parents (public_id int AUTO_INCREMENT, label text NOT NULL)")
        .unwrap();
    db.query("CREATE TABLE children (parent_id int REFERENCES parents(public_id) ON DELETE CASCADE, label text)")
        .unwrap();
    let (parent_id, public_id) = insert_parent(&db, "parents", "race");

    let mut deleting = db.begin();
    assert!(deleting.delete("parents", &parent_id).unwrap());
    db.query_params(
        "INSERT INTO children (parent_id, label) VALUES (%s, 'late')",
        &[Value::Int64(public_id)],
    )
    .unwrap();

    assert!(matches!(deleting.commit(), Err(Error::Conflict(_))));
    assert!(db.delete("parents", &parent_id).unwrap());
    assert!(db.scan("children").unwrap().is_empty());
    assert!(db.get("parents", &parent_id).unwrap().is_none());
}

#[test]
fn foreign_key_ddl_requires_matching_unique_target_and_protects_dependencies() {
    let dir = tempfile::tempdir().unwrap();
    let db = Db::create(dir.path().join("ddl.esql")).unwrap();
    db.query("CREATE TABLE parents (code int, label text)")
        .unwrap();
    assert!(matches!(
        db.query("CREATE TABLE bad (parent_code int REFERENCES parents(code))"),
        Err(Error::SchemaViolation(message)) if message.contains("must have a unique index")
    ));
    db.query("CREATE UNIQUE INDEX ON parents(code)").unwrap();
    db.query("CREATE TABLE children (parent_code int REFERENCES parents(code), label text)")
        .unwrap();
    assert!(matches!(
        db.query("CREATE TABLE wrong_type (parent_code text REFERENCES parents(code))"),
        Err(Error::SchemaViolation(message)) if message.contains("does not match")
    ));
    assert!(matches!(
        db.drop_table("parents"),
        Err(Error::SchemaViolation(message)) if message.contains("referenced by children.parent_code")
    ));
    assert!(db.drop_column("parents", "code").is_err());
    assert!(db.rename_column("parents", "code", "new_code").is_err());
    assert!(db.rename_table("parents", "renamed").is_err());
}

#[test]
fn nullable_foreign_key_accepts_null() {
    let dir = tempfile::tempdir().unwrap();
    let db = Db::create(dir.path().join("nullable.esql")).unwrap();
    db.query("CREATE TABLE parents (public_id int AUTO_INCREMENT, label text)")
        .unwrap();
    db.query("CREATE TABLE children (parent_id int REFERENCES parents(public_id), label text)")
        .unwrap();
    db.query("INSERT INTO children (parent_id, label) VALUES (NULL, 'unassigned')")
        .unwrap();
    let record = &db.scan("children").unwrap()[0].1;
    assert_eq!(record["parent_id"], Value::Null);
    assert!(!text_id(record).is_empty());
}

#[test]
fn cascade_cycles_terminate_and_backup_preserves_relations() {
    let dir = tempfile::tempdir().unwrap();
    let source = dir.path().join("cycles.esql");
    let backup = dir.path().join("cycles-backup.esql");
    let db = Db::create(&source).unwrap();
    db.query("CREATE TABLE nodes (node_id int AUTO_INCREMENT, parent_id int REFERENCES nodes(node_id) ON DELETE CASCADE, label text)")
        .unwrap();
    let mut tx = db.begin();
    tx.query("INSERT INTO nodes (node_id, parent_id, label) VALUES (1, 2, 'one')")
        .unwrap();
    tx.query("INSERT INTO nodes (node_id, parent_id, label) VALUES (2, 1, 'two')")
        .unwrap();
    tx.commit().unwrap();
    assert_eq!(db.scan("nodes").unwrap().len(), 2);

    db.backup(&backup).unwrap();
    let copied = Db::open(&backup).unwrap();
    assert_eq!(copied.scan("nodes").unwrap().len(), 2);
    let first_id = copied
        .scan("nodes")
        .unwrap()
        .into_iter()
        .find(|(_, record)| record["node_id"] == Value::Int64(1))
        .map(|(id, _)| id)
        .unwrap();
    assert!(copied.delete("nodes", &first_id).unwrap());
    assert!(copied.scan("nodes").unwrap().is_empty());
}
