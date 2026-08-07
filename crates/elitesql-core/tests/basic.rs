use std::collections::BTreeMap;

use elitesql_core::{Column, ColumnType, Db, Error, Record, TableSchema, Value};
use tempfile::TempDir;

fn docs_schema() -> TableSchema {
    TableSchema::new(
        "docs",
        vec![
            Column::new("title", ColumnType::Text).not_null(),
            Column::new("score", ColumnType::Int64),
            Column::new("rating", ColumnType::Float64),
            Column::new("active", ColumnType::Bool),
            Column::new("payload", ColumnType::Blob),
            Column::new("created", ColumnType::Timestamp),
            Column::new("meta", ColumnType::Json),
        ],
    )
}

fn new_db() -> (TempDir, Db) {
    let dir = tempfile::tempdir().unwrap();
    let db = Db::create(dir.path().join("test.esql")).unwrap();
    db.create_table(docs_schema()).unwrap();
    (dir, db)
}

fn record(title: &str, score: i64) -> Record {
    let mut r = Record::new();
    r.insert("title".into(), Value::Text(title.into()));
    r.insert("score".into(), Value::Int64(score));
    r
}

#[test]
fn roundtrip_all_types() {
    let (_dir, db) = new_db();
    let mut r = Record::new();
    r.insert("title".into(), Value::Text("hello world".into()));
    r.insert("score".into(), Value::Int64(-7));
    r.insert("rating".into(), Value::Float64(4.5));
    r.insert("active".into(), Value::Bool(true));
    r.insert("payload".into(), Value::Blob(vec![0, 1, 2, 255]));
    r.insert("created".into(), Value::Timestamp(1_722_000_000_000_000));
    r.insert(
        "meta".into(),
        Value::Json(serde_json::json!({"tags": ["a", "b"], "n": 3})),
    );

    let id = db.insert("docs", r).unwrap();
    let read = db.get("docs", &id).unwrap().unwrap();

    assert_eq!(read["title"], Value::Text("hello world".into()));
    assert_eq!(read["score"], Value::Int64(-7));
    assert_eq!(read["rating"], Value::Float64(4.5));
    assert_eq!(read["active"], Value::Bool(true));
    assert_eq!(read["payload"], Value::Blob(vec![0, 1, 2, 255]));
    assert_eq!(read["created"], Value::Timestamp(1_722_000_000_000_000));
    assert_eq!(
        read["meta"],
        Value::Json(serde_json::json!({"tags": ["a", "b"], "n": 3}))
    );
    assert_eq!(read["id"], Value::Text(id));
}

#[test]
fn generated_ids_are_ulids_and_unique() {
    let (_dir, db) = new_db();
    let mut seen = std::collections::HashSet::new();
    for i in 0..100 {
        let id = db.insert("docs", record("t", i)).unwrap();
        assert_eq!(id.len(), 26, "ULID canonical form is 26 chars: {id}");
        assert!(seen.insert(id), "ids must be unique");
    }
}

#[test]
fn explicit_id_duplicate_and_reinsert_after_delete() {
    let (_dir, db) = new_db();
    let mut r = record("first", 1);
    r.insert("id".into(), Value::Text("doc-1".into()));
    assert_eq!(db.insert("docs", r).unwrap(), "doc-1");

    let mut dup = record("second", 2);
    dup.insert("id".into(), Value::Text("doc-1".into()));
    assert!(matches!(
        db.insert("docs", dup),
        Err(Error::DuplicateId { .. })
    ));

    assert!(db.delete("docs", "doc-1").unwrap());
    assert!(db.get("docs", "doc-1").unwrap().is_none());

    let mut again = record("third", 3);
    again.insert("id".into(), Value::Text("doc-1".into()));
    db.insert("docs", again).unwrap();
    let read = db.get("docs", "doc-1").unwrap().unwrap();
    assert_eq!(read["title"], Value::Text("third".into()));
}

#[test]
fn update_patches_and_snapshots_see_old_versions() {
    let (_dir, db) = new_db();
    let id = db.insert("docs", record("original", 10)).unwrap();

    let snap = db.snapshot();

    let mut patch = Record::new();
    patch.insert("score".into(), Value::Int64(99));
    db.update("docs", &id, patch).unwrap();

    let now = db.get("docs", &id).unwrap().unwrap();
    assert_eq!(now["score"], Value::Int64(99));
    assert_eq!(now["title"], Value::Text("original".into()), "patch keeps other fields");

    let then = db.get_at(&snap, "docs", &id).unwrap().unwrap();
    assert_eq!(then["score"], Value::Int64(10), "snapshot sees the old version");
}

#[test]
fn delete_respects_snapshots() {
    let (_dir, db) = new_db();
    let id = db.insert("docs", record("ephemeral", 1)).unwrap();
    let before = db.snapshot();

    assert!(db.delete("docs", &id).unwrap());
    assert!(!db.delete("docs", &id).unwrap(), "second delete is a no-op");
    assert!(db.get("docs", &id).unwrap().is_none());
    assert!(db.scan("docs").unwrap().is_empty());

    let old = db.get_at(&before, "docs", &id).unwrap().unwrap();
    assert_eq!(old["title"], Value::Text("ephemeral".into()));
    assert_eq!(db.scan_at(&before, "docs").unwrap().len(), 1);
}

#[test]
fn reopen_rebuilds_state() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("test.esql");
    let mut ids = Vec::new();
    {
        let db = Db::create(&path).unwrap();
        db.create_table(docs_schema()).unwrap();
        for i in 0..50 {
            ids.push(db.insert("docs", record(&format!("doc {i}"), i)).unwrap());
        }
        db.delete("docs", &ids[0]).unwrap();
        let mut patch = Record::new();
        patch.insert("score".into(), Value::Int64(1000));
        db.update("docs", &ids[1], patch).unwrap();
    }

    let db = Db::open(&path).unwrap();
    assert_eq!(db.tables(), vec!["docs".to_string()]);
    assert!(db.get("docs", &ids[0]).unwrap().is_none(), "delete survives reopen");
    let updated = db.get("docs", &ids[1]).unwrap().unwrap();
    assert_eq!(updated["score"], Value::Int64(1000), "update survives reopen");
    assert_eq!(db.scan("docs").unwrap().len(), 49);

    // Version counter continues: snapshots taken after reopen see new writes.
    let before = db.snapshot();
    let new_id = db.insert("docs", record("after reopen", 1)).unwrap();
    assert!(db.get_at(&before, "docs", &new_id).unwrap().is_none());
    assert!(db.get("docs", &new_id).unwrap().is_some());
}

#[test]
fn schema_validation_errors() {
    let (_dir, db) = new_db();

    let mut unknown = record("x", 1);
    unknown.insert("nope".into(), Value::Bool(true));
    assert!(matches!(
        db.insert("docs", unknown),
        Err(Error::SchemaViolation(_))
    ));

    let mut wrong_type = Record::new();
    wrong_type.insert("title".into(), Value::Int64(5));
    assert!(matches!(
        db.insert("docs", wrong_type),
        Err(Error::SchemaViolation(_))
    ));

    let missing_not_null = Record::new();
    assert!(matches!(
        db.insert("docs", missing_not_null),
        Err(Error::SchemaViolation(_))
    ));

    assert!(matches!(
        db.insert("missing_table", record("x", 1)),
        Err(Error::TableNotFound(_))
    ));

    let reserved = TableSchema::new("bad", vec![Column::new("id", ColumnType::Text)]);
    assert!(matches!(
        db.create_table(reserved),
        Err(Error::InvalidArgument(_))
    ));

    assert!(matches!(
        db.create_table(docs_schema()),
        Err(Error::TableExists(_))
    ));
}

#[test]
fn concurrent_readers_and_writer() {
    let (_dir, db) = new_db();
    let db = std::sync::Arc::new(db);
    let mut ids = Vec::new();
    for i in 0..100 {
        ids.push(db.insert("docs", record(&format!("doc {i}"), i)).unwrap());
    }

    let mut handles = Vec::new();
    for t in 0..4 {
        let db = db.clone();
        let ids = ids.clone();
        handles.push(std::thread::spawn(move || {
            for round in 0..50 {
                let id = &ids[(t * 7 + round * 13) % ids.len()];
                let rec = db.get("docs", id).unwrap().unwrap();
                assert!(matches!(rec["title"], Value::Text(_)));
            }
        }));
    }
    let writer_db = db.clone();
    handles.push(std::thread::spawn(move || {
        for i in 0..50 {
            writer_db
                .insert("docs", record(&format!("new {i}"), i))
                .unwrap();
        }
    }));
    for h in handles {
        h.join().unwrap();
    }
    assert_eq!(db.scan("docs").unwrap().len(), 150);
}

#[test]
fn second_process_is_locked_out() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("test.esql");
    let db = Db::create(&path).unwrap();
    assert!(matches!(Db::open(&path), Err(Error::DatabaseLocked(_))));
    drop(db);
    Db::open(&path).unwrap();
}

#[test]
fn record_map_with_btreemap_alias() {
    let r: Record = BTreeMap::new();
    assert!(r.is_empty());
}
