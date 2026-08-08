use elitesql_core::{Column, ColumnType, Db, Error, Record, TableSchema, Value};

fn schema() -> TableSchema {
    TableSchema::new(
        "docs",
        vec![
            Column::new("title", ColumnType::Text).not_null(),
            Column::new("score", ColumnType::Int64),
        ],
    )
}

fn record(i: usize) -> Record {
    let mut record = Record::new();
    record.insert("id".into(), Value::Text(format!("row-{i:08}")));
    record.insert("title".into(), Value::Text(format!("document {i}")));
    record.insert("score".into(), Value::Int64(i as i64));
    record
}

#[test]
fn sorted_bulk_load_is_atomic_queryable_and_reopenable() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("bulk.esql");
    let db = Db::create(&path).unwrap();
    db.create_table(schema()).unwrap();
    let before = db.snapshot();

    assert_eq!(
        db.bulk_insert_sorted("docs", (0..10_000).map(record))
            .unwrap(),
        10_000
    );
    assert!(db
        .get_at(&before, "docs", "row-00009999")
        .unwrap()
        .is_none());
    assert_eq!(
        db.get("docs", "row-00009999").unwrap().unwrap()["score"],
        Value::Int64(9_999)
    );
    assert_eq!(
        db.find_eq("docs", "score", &Value::Int64(9_999))
            .unwrap()
            .len(),
        1
    );
    assert_eq!(
        db.bulk_insert_sorted("docs", (10_000..10_100).map(record))
            .unwrap(),
        100
    );
    drop(before);
    drop(db);

    let db = Db::open(&path).unwrap();
    assert_eq!(db.scan("docs").unwrap().len(), 10_100);
    assert_eq!(
        db.get("docs", "row-00010099").unwrap().unwrap()["title"],
        Value::Text("document 10099".into())
    );
}

#[test]
fn invalid_order_publishes_no_partial_rows() {
    let dir = tempfile::tempdir().unwrap();
    let db = Db::create(dir.path().join("invalid.esql")).unwrap();
    db.create_table(schema()).unwrap();

    let error = db
        .bulk_insert_sorted("docs", [record(1), record(3), record(2)])
        .unwrap_err();
    assert!(matches!(error, Error::InvalidArgument(_)), "{error:?}");
    assert!(db.scan("docs").unwrap().is_empty());

    db.bulk_insert_sorted("docs", [record(5)]).unwrap();
    let error = db.bulk_insert_sorted("docs", [record(4)]).unwrap_err();
    assert!(matches!(error, Error::InvalidArgument(_)), "{error:?}");
    assert_eq!(db.scan("docs").unwrap().len(), 1);
}

#[test]
fn derived_indexes_must_be_created_after_bulk_load() {
    let dir = tempfile::tempdir().unwrap();
    let db = Db::create(dir.path().join("indexed.esql")).unwrap();
    db.create_table(schema()).unwrap();
    db.create_index("docs", "score", false).unwrap();

    let error = db.bulk_insert_sorted("docs", [record(1)]).unwrap_err();
    assert!(matches!(error, Error::InvalidArgument(_)), "{error:?}");
    assert!(db.scan("docs").unwrap().is_empty());
}
