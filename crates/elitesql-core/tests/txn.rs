use elitesql_core::{Column, ColumnType, Db, Error, Record, TableSchema, Value};
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
    assert!(db.get("docs", &a).unwrap().is_none(), "not visible before commit");
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
    assert_eq!(db.get("docs", &id).unwrap().unwrap()["score"], Value::Int64(2));
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
    assert!(matches!(t2.commit(), Err(Error::Conflict(_))), "CONFLICT_RETRY");

    // The retry pattern: begin again, reapply, commit.
    let mut t3 = db.begin();
    let mut p3 = Record::new();
    p3.insert("score".into(), Value::Int64(3));
    t3.update("docs", &id, p3).unwrap();
    t3.commit().unwrap();
    assert_eq!(db.get("docs", &id).unwrap().unwrap()["score"], Value::Int64(3));
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
    assert_eq!(db.get("docs", &a).unwrap().unwrap()["score"], Value::Int64(10));
    assert_eq!(db.get("docs", &b).unwrap().unwrap()["score"], Value::Int64(20));
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
        r.insert("email".into(), Value::Text(format!("u{}@example.com", i % 4)));
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
