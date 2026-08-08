use std::fs;

use elitesql_core::{Column, ColumnType, Db, DbOptions, Record, TableSchema, Value};

fn insert_batch(db: &Db, batch: usize, rows: usize) {
    let mut txn = db.begin();
    for row in 0..rows {
        let id = format!("id-{batch:03}-{row:04}");
        let mut record = Record::new();
        record.insert("id".into(), Value::Text(id));
        record.insert("value".into(), Value::Int64((batch * rows + row) as i64));
        txn.insert("items", record).unwrap();
    }
    txn.commit().unwrap();
}

#[test]
fn checkpoints_publish_constant_size_deltas_and_levels_bound_run_count() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("leveled-primary.esql");
    let options = DbOptions {
        memtable_max_bytes: u64::MAX,
        ..DbOptions::default()
    };
    let db = Db::create_with(&path, options.clone()).unwrap();
    db.create_table(TableSchema::new(
        "items",
        vec![Column::new("value", ColumnType::Int64)],
    ))
    .unwrap();

    let mut checkpoint_run_bytes = Vec::new();
    let mut previous = 0;
    for batch in 0..40 {
        insert_batch(&db, batch, 100);
        db.checkpoint().unwrap();
        let total = db.maintenance_stats().primary_checkpoint_bytes_written;
        checkpoint_run_bytes.push(total - previous);
        previous = total;
    }
    db.wait_for_primary_compaction();

    let smallest = *checkpoint_run_bytes.iter().min().unwrap();
    let largest = *checkpoint_run_bytes.iter().max().unwrap();
    assert!(smallest > 0);
    assert!(
        largest <= smallest * 2,
        "equal-sized batches must not rewrite a growing base: {checkpoint_run_bytes:?}"
    );
    let stats = db.maintenance_stats();
    assert!(stats.primary_run_compactions >= 2, "{stats:?}");
    assert!(stats.primary_runs <= 12, "{stats:?}");
    assert!(stats.primary_run_compaction_bytes_read > 0);
    assert!(stats.primary_run_compaction_bytes_written > 0);
    drop(db);

    let reopened = Db::open_with(&path, options).unwrap();
    assert_eq!(reopened.scan("items").unwrap().len(), 4_000);
    assert_eq!(
        reopened.get("items", "id-039-0099").unwrap().unwrap()["value"],
        Value::Int64(3_999)
    );
}

#[test]
fn missing_run_is_discarded_and_rebuilt_from_canonical_segments() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("missing-run.esql");
    {
        let db = Db::create_with(
            &path,
            DbOptions {
                memtable_max_bytes: u64::MAX,
                ..DbOptions::default()
            },
        )
        .unwrap();
        db.create_table(TableSchema::new(
            "items",
            vec![Column::new("value", ColumnType::Int64)],
        ))
        .unwrap();
        for batch in 0..6 {
            insert_batch(&db, batch, 20);
            db.checkpoint().unwrap();
        }
        db.wait_for_primary_compaction();
    }

    let run = fs::read_dir(path.join("indexes"))
        .unwrap()
        .filter_map(|entry| entry.ok().map(|entry| entry.path()))
        .find(|path| {
            path.file_name()
                .and_then(|name| name.to_str())
                .is_some_and(|name| name.starts_with("primary-L") && name.ends_with(".pidx.run"))
        })
        .expect("leveled run exists");
    fs::remove_file(run).unwrap();

    let reopened = Db::open(&path).unwrap();
    assert_eq!(reopened.scan("items").unwrap().len(), 120);
    assert_eq!(
        reopened.get("items", "id-005-0019").unwrap().unwrap()["value"],
        Value::Int64(119)
    );
}

#[test]
fn corrupt_run_manifest_is_rebuilt_from_canonical_segments() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("corrupt-run-manifest.esql");
    {
        let db = Db::create_with(
            &path,
            DbOptions {
                memtable_max_bytes: u64::MAX,
                ..DbOptions::default()
            },
        )
        .unwrap();
        db.create_table(TableSchema::new(
            "items",
            vec![Column::new("value", ColumnType::Int64)],
        ))
        .unwrap();
        for batch in 0..3 {
            insert_batch(&db, batch, 40);
            db.checkpoint().unwrap();
        }
    }

    fs::write(path.join("indexes/primary.runs"), b"damaged").unwrap();

    let reopened = Db::open(&path).unwrap();
    assert_eq!(reopened.scan("items").unwrap().len(), 120);
    assert_eq!(
        reopened.get("items", "id-002-0039").unwrap().unwrap()["value"],
        Value::Int64(119)
    );
    drop(reopened);

    let reopened_again = Db::open(&path).unwrap();
    assert_eq!(reopened_again.scan("items").unwrap().len(), 120);
}
