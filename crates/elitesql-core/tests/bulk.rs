use elitesql_core::{Column, ColumnType, Db, Error, Record, TableSchema, Value};
use std::sync::{mpsc, Arc};
use std::time::{Duration, Instant};

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

#[test]
fn a_large_scan_yields_the_state_lock_to_a_concurrent_writer() {
    let dir = tempfile::tempdir().unwrap();
    let db = Db::create(dir.path().join("scan-writer.esql")).unwrap();
    db.create_table(schema()).unwrap();
    let body = "x".repeat(768);
    const ROWS: usize = 50_000;
    db.bulk_insert_sorted(
        "docs",
        (0..ROWS).map(|i| {
            let mut row = record(i);
            row.insert("title".into(), Value::Text(body.clone()));
            row
        }),
    )
    .unwrap();

    let baseline_started = Instant::now();
    assert_eq!(db.scan("docs").unwrap().len(), ROWS);
    let baseline = baseline_started.elapsed();

    let db = Arc::new(db);
    let scanner = db.clone();
    let (started_tx, started_rx) = mpsc::channel();
    let (done_tx, done_rx) = mpsc::channel();
    let worker = std::thread::spawn(move || {
        started_tx.send(()).unwrap();
        let rows = scanner.scan("docs").unwrap();
        done_tx.send(rows.len()).unwrap();
    });
    started_rx.recv().unwrap();
    // Let the scan get under way, but never sleep long enough for it to
    // finish: mapped segment reads make even this fixture scan fast.
    std::thread::sleep((baseline / 5).min(Duration::from_millis(10)));

    let commit_started = Instant::now();
    db.insert("docs", record(ROWS + 1)).unwrap();
    let commit_elapsed = commit_started.elapsed();
    assert!(
        commit_elapsed < baseline / 2,
        "commit waited {commit_elapsed:?} behind a scan whose baseline is {baseline:?}"
    );
    assert!(
        done_rx.try_recv().is_err(),
        "the scan fixture finished before the concurrent commit could demonstrate progress"
    );
    assert_eq!(done_rx.recv().unwrap(), ROWS);
    worker.join().unwrap();
}
