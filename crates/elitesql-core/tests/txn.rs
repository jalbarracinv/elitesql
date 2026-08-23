use elitesql_core::{
    Column, ColumnType, Db, DbOptions, Error, MemoryOptions, Record, TableSchema, Value,
};
use tempfile::TempDir;

fn new_db() -> (TempDir, Db) {
    let dir = tempfile::tempdir().unwrap();
    let db = Db::create(dir.path().join("test.esql")).unwrap();
    db.create_table(TableSchema::new(
        "docs",
        vec![
            Column::new("title", ColumnType::Text).not_null(),
            Column::new("score", ColumnType::Int64),
            Column::new("email", ColumnType::Text),
        ],
    ))
    .unwrap();
    (dir, db)
}

fn record(title: &str, score: i64) -> Record {
    let mut r = Record::new();
    r.insert("title".into(), Value::Text(title.into()));
    r.insert("score".into(), Value::Int64(score));
    r
}

#[test]
fn txn_commit_is_atomic_and_isolated() {
    let (_dir, db) = new_db();
    let mut txn = db.begin();
    let a = txn.insert("docs", record("a", 1)).unwrap();
    let b = txn.insert("docs", record("b", 2)).unwrap();

    // Staged writes are visible inside the transaction only.
    assert!(txn.get("docs", &a).unwrap().is_some());
    assert!(
        db.get("docs", &a).unwrap().is_none(),
        "not visible before commit"
    );
    assert!(db.get("docs", &b).unwrap().is_none());

    txn.commit().unwrap();
    assert!(db.get("docs", &a).unwrap().is_some());
    assert!(db.get("docs", &b).unwrap().is_some());
}

#[test]
fn txn_rollback_discards_everything() {
    let (_dir, db) = new_db();
    let mut txn = db.begin();
    let id = txn.insert("docs", record("ghost", 1)).unwrap();
    txn.rollback();
    assert!(db.get("docs", &id).unwrap().is_none());
    assert!(db.scan("docs").unwrap().is_empty());
}

#[test]
fn txn_preserves_out_of_order_and_replaced_staged_operations() {
    let (_dir, db) = new_db();
    let mut txn = db.begin();
    for (id, score) in [("z", 3), ("a", 1), ("m", 2)] {
        let mut row = record(id, score);
        row.insert("id".into(), Value::Text(id.into()));
        txn.insert("docs", row).unwrap();
    }

    let mut patch = Record::new();
    patch.insert("score".into(), Value::Int64(20));
    txn.update("docs", "m", patch).unwrap();
    assert!(txn.delete("docs", "z").unwrap());
    txn.commit().unwrap();

    assert_eq!(
        db.get("docs", "a").unwrap().unwrap()["score"],
        Value::Int64(1)
    );
    assert_eq!(
        db.get("docs", "m").unwrap().unwrap()["score"],
        Value::Int64(20)
    );
    assert!(db.get("docs", "z").unwrap().is_none());

    db.checkpoint().unwrap();
    assert_eq!(db.scan("docs").unwrap().len(), 2);
}

#[test]
fn txn_interns_multiple_tables_without_crossing_staged_rows() {
    let dir = tempfile::tempdir().unwrap();
    let db = Db::create(dir.path().join("test.esql")).unwrap();
    for table in ["alpha", "omega"] {
        db.create_table(TableSchema::new(
            table,
            vec![Column::new("title", ColumnType::Text).not_null()],
        ))
        .unwrap();
    }

    let mut txn = db.begin();
    for (table, id) in [
        ("omega", "b"),
        ("alpha", "z"),
        ("omega", "a"),
        ("alpha", "a"),
    ] {
        let mut row = Record::new();
        row.insert("id".into(), Value::Text(id.into()));
        row.insert("title".into(), Value::Text(format!("{table}-{id}")));
        txn.insert(table, row).unwrap();
    }
    txn.commit().unwrap();

    for (table, id) in [
        ("omega", "b"),
        ("alpha", "z"),
        ("omega", "a"),
        ("alpha", "a"),
    ] {
        assert_eq!(
            db.get(table, id).unwrap().unwrap()["title"],
            Value::Text(format!("{table}-{id}"))
        );
    }
}

#[test]
fn txn_reads_from_stable_snapshot() {
    let (_dir, db) = new_db();
    let id = db.insert("docs", record("v1", 1)).unwrap();

    let txn = db.begin();
    // A commit from "another writer" lands after the txn began.
    let mut patch = Record::new();
    patch.insert("score".into(), Value::Int64(2));
    db.update("docs", &id, patch).unwrap();

    // The txn still sees the snapshot version.
    let seen = txn.get("docs", &id).unwrap().unwrap();
    assert_eq!(seen["score"], Value::Int64(1));
    // Outside the txn, the new version is visible.
    assert_eq!(
        db.get("docs", &id).unwrap().unwrap()["score"],
        Value::Int64(2)
    );
}

#[test]
fn transactional_sql_sees_staged_rows_and_commits_business_unit_atomically() {
    let dir = tempfile::tempdir().unwrap();
    let db = Db::create(dir.path().join("sql-txn.esql")).unwrap();
    db.query("CREATE TABLE users (user_id int AUTO_INCREMENT, credits int NOT NULL)")
        .unwrap();
    db.query("CREATE TABLE docs (owner_id int REFERENCES users(user_id), title text NOT NULL)")
        .unwrap();
    db.query("INSERT INTO users (credits) VALUES (10)").unwrap();

    let mut tx = db.begin();
    assert_eq!(
        tx.query_params(
            "UPDATE users SET credits = credits - %s WHERE user_id = %s AND credits >= %s",
            &[Value::Int64(3), Value::Int64(1), Value::Int64(3)],
        )
        .unwrap(),
        elitesql_core::QueryOutput::Affected(1)
    );
    let inserted = tx
        .query_params(
            "INSERT INTO docs (owner_id, title) VALUES (%s, %s) RETURNING owner_id, title",
            &[Value::Int64(1), Value::Text("contract".into())],
        )
        .unwrap();
    assert!(matches!(
        inserted,
        elitesql_core::QueryOutput::Rows { ref rows, .. }
            if rows == &vec![vec![Value::Int64(1), Value::Text("contract".into())]]
    ));
    let selected = tx
        .query("SELECT title FROM docs WHERE owner_id = 1")
        .unwrap();
    assert!(matches!(
        selected,
        elitesql_core::QueryOutput::Rows { ref rows, .. } if rows.len() == 1
    ));
    assert!(db.scan("docs").unwrap().is_empty());
    tx.commit().unwrap();

    assert_eq!(db.scan("docs").unwrap().len(), 1);
    assert_eq!(db.scan("users").unwrap()[0].1["credits"], Value::Int64(7));
}

#[test]
fn transactional_sql_rollback_discards_every_statement() {
    let (_dir, db) = new_db();
    let mut tx = db.begin();
    tx.query("INSERT INTO docs (title, score, email) VALUES ('temporary', 1, NULL)")
        .unwrap();
    tx.query("UPDATE docs SET score = score + 1 WHERE title = 'temporary'")
        .unwrap();
    tx.rollback();
    assert!(db.scan("docs").unwrap().is_empty());
}

#[test]
fn concurrent_update_same_record_conflicts() {
    let (_dir, db) = new_db();
    let id = db.insert("docs", record("contended", 0)).unwrap();

    let mut t1 = db.begin();
    let mut t2 = db.begin();

    let mut p1 = Record::new();
    p1.insert("score".into(), Value::Int64(1));
    t1.update("docs", &id, p1).unwrap();

    let mut p2 = Record::new();
    p2.insert("score".into(), Value::Int64(2));
    t2.update("docs", &id, p2).unwrap();

    t1.commit().unwrap();
    assert!(
        matches!(t2.commit(), Err(Error::Conflict(_))),
        "CONFLICT_RETRY"
    );

    // The retry pattern: begin again, reapply, commit.
    let mut t3 = db.begin();
    let mut p3 = Record::new();
    p3.insert("score".into(), Value::Int64(3));
    t3.update("docs", &id, p3).unwrap();
    t3.commit().unwrap();
    assert_eq!(
        db.get("docs", &id).unwrap().unwrap()["score"],
        Value::Int64(3)
    );
}

#[test]
fn concurrent_insert_same_id_conflicts() {
    let (_dir, db) = new_db();
    let mut t1 = db.begin();
    let mut t2 = db.begin();

    let mut r1 = record("one", 1);
    r1.insert("id".into(), Value::Text("same".into()));
    t1.insert("docs", r1).unwrap();

    let mut r2 = record("two", 2);
    r2.insert("id".into(), Value::Text("same".into()));
    t2.insert("docs", r2).unwrap();

    t1.commit().unwrap();
    assert!(matches!(t2.commit(), Err(Error::Conflict(_))));
    assert_eq!(
        db.get("docs", "same").unwrap().unwrap()["title"],
        Value::Text("one".into())
    );
}

#[test]
fn concurrent_delete_vs_update_conflicts() {
    let (_dir, db) = new_db();
    let id = db.insert("docs", record("target", 1)).unwrap();

    let mut t1 = db.begin();
    let mut t2 = db.begin();
    assert!(t1.delete("docs", &id).unwrap());
    let mut p = Record::new();
    p.insert("score".into(), Value::Int64(9));
    t2.update("docs", &id, p).unwrap();

    t1.commit().unwrap();
    assert!(matches!(t2.commit(), Err(Error::Conflict(_))));
    assert!(db.get("docs", &id).unwrap().is_none());
}

#[test]
fn non_conflicting_txns_both_commit() {
    let (_dir, db) = new_db();
    let a = db.insert("docs", record("a", 1)).unwrap();
    let b = db.insert("docs", record("b", 2)).unwrap();

    let mut t1 = db.begin();
    let mut t2 = db.begin();
    let mut pa = Record::new();
    pa.insert("score".into(), Value::Int64(10));
    t1.update("docs", &a, pa).unwrap();
    let mut pb = Record::new();
    pb.insert("score".into(), Value::Int64(20));
    t2.update("docs", &b, pb).unwrap();

    let v1 = t1.commit().unwrap();
    let v2 = t2.commit().unwrap();
    assert!(v2 > v1, "commit versions are monotonic");
    assert_eq!(
        db.get("docs", &a).unwrap().unwrap()["score"],
        Value::Int64(10)
    );
    assert_eq!(
        db.get("docs", &b).unwrap().unwrap()["score"],
        Value::Int64(20)
    );
}

#[test]
fn parallel_writers_on_disjoint_records() {
    let (_dir, db) = new_db();
    let db = std::sync::Arc::new(db);
    let mut handles = Vec::new();
    for t in 0..8 {
        let db = db.clone();
        handles.push(std::thread::spawn(move || {
            for i in 0..25 {
                let mut txn = db.begin();
                txn.insert("docs", record(&format!("w{t}-{i}"), i)).unwrap();
                txn.commit().unwrap();
            }
        }));
    }
    for h in handles {
        h.join().unwrap();
    }
    assert_eq!(db.scan("docs").unwrap().len(), 200);
}

#[test]
fn unique_index_rejects_duplicates_at_commit() {
    let (_dir, db) = new_db();
    db.create_index("docs", "email", true).unwrap();

    let mut r1 = record("ana", 1);
    r1.insert("email".into(), Value::Text("ana@example.com".into()));
    db.insert("docs", r1).unwrap();

    let mut r2 = record("impostor", 2);
    r2.insert("email".into(), Value::Text("ana@example.com".into()));
    assert!(matches!(
        db.insert("docs", r2),
        Err(Error::UniqueViolation { .. })
    ));

    // Nulls are not constrained by unique indexes.
    db.insert("docs", record("no email 1", 3)).unwrap();
    db.insert("docs", record("no email 2", 4)).unwrap();
}

#[test]
fn unique_index_concurrent_txns() {
    let (_dir, db) = new_db();
    db.create_index("docs", "email", true).unwrap();

    let mut t1 = db.begin();
    let mut t2 = db.begin();
    let mut r1 = record("first", 1);
    r1.insert("email".into(), Value::Text("x@example.com".into()));
    t1.insert("docs", r1).unwrap();
    let mut r2 = record("second", 2);
    r2.insert("email".into(), Value::Text("x@example.com".into()));
    t2.insert("docs", r2).unwrap();

    t1.commit().unwrap();
    assert!(matches!(t2.commit(), Err(Error::UniqueViolation { .. })));
}

#[test]
fn unique_value_can_move_between_records_in_one_txn() {
    let (_dir, db) = new_db();
    db.create_index("docs", "email", true).unwrap();

    let mut r1 = record("holder", 1);
    r1.insert("email".into(), Value::Text("shared@example.com".into()));
    let holder = db.insert("docs", r1).unwrap();
    let other = db.insert("docs", record("other", 2)).unwrap();

    // One txn frees the value and assigns it to another record.
    let mut txn = db.begin();
    let mut free = Record::new();
    free.insert("email".into(), Value::Null);
    txn.update("docs", &holder, free).unwrap();
    let mut take = Record::new();
    take.insert("email".into(), Value::Text("shared@example.com".into()));
    txn.update("docs", &other, take).unwrap();
    txn.commit().unwrap();

    let found = db
        .find_eq("docs", "email", &Value::Text("shared@example.com".into()))
        .unwrap();
    assert_eq!(found.len(), 1);
    assert_eq!(found[0].0, other);
}

#[test]
fn create_unique_index_fails_on_existing_duplicates() {
    let (_dir, db) = new_db();
    let mut r1 = record("a", 1);
    r1.insert("email".into(), Value::Text("dup@example.com".into()));
    db.insert("docs", r1).unwrap();
    let mut r2 = record("b", 2);
    r2.insert("email".into(), Value::Text("dup@example.com".into()));
    db.insert("docs", r2).unwrap();

    assert!(matches!(
        db.create_index("docs", "email", true),
        Err(Error::UniqueViolation { .. })
    ));
    // Non-unique index over the same data is fine.
    db.create_index("docs", "email", false).unwrap();
    let found = db
        .find_eq("docs", "email", &Value::Text("dup@example.com".into()))
        .unwrap();
    assert_eq!(found.len(), 2);
}

#[test]
fn find_eq_with_and_without_index() {
    let (_dir, db) = new_db();
    for i in 0..20 {
        let mut r = record(&format!("t{i}"), i % 4);
        r.insert(
            "email".into(),
            Value::Text(format!("u{}@example.com", i % 4)),
        );
        db.insert("docs", r).unwrap();
    }
    // Without an index: full scan path.
    let hits = db.find_eq("docs", "score", &Value::Int64(2)).unwrap();
    assert_eq!(hits.len(), 5);
    // With an index: indexed path, same semantics.
    db.create_index("docs", "email", false).unwrap();
    let hits = db
        .find_eq("docs", "email", &Value::Text("u2@example.com".into()))
        .unwrap();
    assert_eq!(hits.len(), 5);
    // Index stays correct across updates and deletes.
    let (moved_id, _) = hits[0].clone();
    let mut patch = Record::new();
    patch.insert("email".into(), Value::Text("moved@example.com".into()));
    db.update("docs", &moved_id, patch).unwrap();
    let hits = db
        .find_eq("docs", "email", &Value::Text("u2@example.com".into()))
        .unwrap();
    assert_eq!(hits.len(), 4);
    db.delete("docs", &hits[0].0).unwrap();
    let hits = db
        .find_eq("docs", "email", &Value::Text("u2@example.com".into()))
        .unwrap();
    assert_eq!(hits.len(), 3);
    let hits = db
        .find_eq("docs", "email", &Value::Text("moved@example.com".into()))
        .unwrap();
    assert_eq!(hits.len(), 1);
}

#[test]
fn unindexed_find_eq_streams_latest_versions_across_storage_layers() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("streaming-find.esql");
    let db = Db::create(&path).unwrap();
    db.create_table(TableSchema::new(
        "docs",
        vec![
            Column::new("title", ColumnType::Text).not_null(),
            Column::new("score", ColumnType::Int64),
            Column::new("email", ColumnType::Text),
        ],
    ))
    .unwrap();

    for (id, title, score) in [
        ("a", "updated away", 7),
        ("b", "updated into match", 2),
        ("c", "deleted", 7),
        ("d", "stable segment match", 7),
    ] {
        let mut rec = record(title, score);
        rec.insert("id".into(), Value::Text(id.into()));
        db.insert("docs", rec).unwrap();
    }
    assert_eq!(db.maintenance_stats().checkpoints, 0);
    db.checkpoint().unwrap();
    assert_eq!(db.maintenance_stats().checkpoints, 1);

    let mut patch = Record::new();
    patch.insert("score".into(), Value::Int64(0));
    db.update("docs", "a", patch).unwrap();
    let mut patch = Record::new();
    patch.insert("score".into(), Value::Int64(7));
    db.update("docs", "b", patch).unwrap();
    db.delete("docs", "c").unwrap();
    let mut rec = record("new memtable match", 7);
    rec.insert("id".into(), Value::Text("e".into()));
    db.insert("docs", rec).unwrap();

    let assert_hits = |db: &Db| {
        let hits = db.find_eq("docs", "score", &Value::Int64(7)).unwrap();
        let ids: Vec<&str> = hits.iter().map(|(id, _)| id.as_str()).collect();
        assert_eq!(ids, ["b", "d", "e"]);
        assert_eq!(hits[0].1["title"], Value::Text("updated into match".into()));
        assert_eq!(
            hits[1].1["title"],
            Value::Text("stable segment match".into())
        );
        assert_eq!(hits[2].1["title"], Value::Text("new memtable match".into()));
    };

    // Old versions are in a segment while the replacements and tombstone
    // are still in the memtable.
    assert_hits(&db);
    db.checkpoint().unwrap();
    assert_eq!(db.maintenance_stats().checkpoints, 2);
    assert_hits(&db);
    drop(db);

    let db = Db::open(&path).unwrap();
    assert_eq!(db.maintenance_stats().checkpoints, 0);
    assert_hits(&db);
    db.compact().unwrap();
    assert_hits(&db);
}

#[test]
fn unindexed_find_eq_ignores_older_versions_in_the_same_segment() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("streaming-same-segment.esql");
    let db = Db::create(&path).unwrap();
    db.create_table(TableSchema::new(
        "docs",
        vec![Column::new("score", ColumnType::Int64)],
    ))
    .unwrap();

    let mut record = Record::new();
    record.insert("id".into(), Value::Text("changing".into()));
    record.insert("score".into(), Value::Int64(1));
    db.insert("docs", record).unwrap();
    for score in [2, 3, 4] {
        let mut patch = Record::new();
        patch.insert("score".into(), Value::Int64(score));
        db.update("docs", "changing", patch).unwrap();
    }
    db.checkpoint().unwrap();

    for old in [1, 2, 3] {
        assert!(db
            .find_eq("docs", "score", &Value::Int64(old))
            .unwrap()
            .is_empty());
    }
    let current = db.find_eq("docs", "score", &Value::Int64(4)).unwrap();
    assert_eq!(current.len(), 1);
    assert_eq!(current[0].0, "changing");
}

#[test]
fn secondary_index_survives_reopen() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("test.esql");
    {
        let db = Db::create(&path).unwrap();
        db.create_table(TableSchema::new(
            "docs",
            vec![
                Column::new("title", ColumnType::Text).not_null(),
                Column::new("email", ColumnType::Text),
            ],
        ))
        .unwrap();
        db.create_index("docs", "email", true).unwrap();
        let mut r = Record::new();
        r.insert("title".into(), Value::Text("ana".into()));
        r.insert("email".into(), Value::Text("ana@example.com".into()));
        db.insert("docs", r).unwrap();
    }
    let db = Db::open(&path).unwrap();
    // Uniqueness still enforced after reopen (index rebuilt from data).
    let mut dup = Record::new();
    dup.insert("title".into(), Value::Text("clone".into()));
    dup.insert("email".into(), Value::Text("ana@example.com".into()));
    assert!(matches!(
        db.insert("docs", dup),
        Err(Error::UniqueViolation { .. })
    ));
    let found = db
        .find_eq("docs", "email", &Value::Text("ana@example.com".into()))
        .unwrap();
    assert_eq!(found.len(), 1);
}

#[test]
fn paged_secondary_index_merges_updates_and_pages_hot_keys() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("paged-secondary.esql");
    let mut original_ids = Vec::new();
    {
        let db = Db::create_with(
            &path,
            DbOptions {
                memory: MemoryOptions {
                    query_working_bytes: 256,
                    scan_batch_rows: 4,
                    spill_directory: Some(dir.path().join("spill")),
                    ..MemoryOptions::default()
                },
                ..DbOptions::default()
            },
        )
        .unwrap();
        db.create_table(TableSchema::new(
            "docs",
            vec![Column::new("tag", ColumnType::Text)],
        ))
        .unwrap();
        db.create_index("docs", "tag", false).unwrap();
        for _ in 0..200 {
            let mut record = Record::new();
            record.insert("tag".into(), Value::Text("hot".into()));
            original_ids.push(db.insert("docs", record).unwrap());
        }
    }
    assert!(path.join("indexes").read_dir().unwrap().any(|entry| {
        entry
            .ok()
            .is_some_and(|entry| entry.path().extension().is_some_and(|ext| ext == "sidx"))
    }));

    let db = Db::open(&path).unwrap();
    let mut patch = Record::new();
    patch.insert("tag".into(), Value::Text("cold".into()));
    db.update("docs", &original_ids[25], patch).unwrap();
    db.delete("docs", &original_ids[50]).unwrap();
    let mut added = Record::new();
    added.insert("tag".into(), Value::Text("hot".into()));
    let added_id = db.insert("docs", added).unwrap();

    let mut seen = Vec::new();
    let mut after: Option<String> = None;
    loop {
        let batch = db
            .find_eq_batch(
                "docs",
                "tag",
                &Value::Text("hot".into()),
                after.as_deref(),
                7,
            )
            .unwrap();
        if batch.is_empty() {
            break;
        }
        after = batch.last().map(|(id, _)| id.clone());
        seen.extend(batch.into_iter().map(|(id, _)| id));
    }
    assert_eq!(seen.len(), 199);
    assert!(seen.windows(2).all(|pair| pair[0] < pair[1]));
    assert!(!seen.contains(&original_ids[25]));
    assert!(!seen.contains(&original_ids[50]));
    assert!(seen.contains(&added_id));
}
