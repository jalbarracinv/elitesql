use std::collections::BTreeSet;
use std::fs;

use elitesql_core::{Column, ColumnType, Db, DbOptions, Record, TableSchema, Value};

fn record(id: &str, group: &str, value: i64) -> Record {
    let mut record = Record::new();
    record.insert("id".into(), Value::Text(id.into()));
    record.insert("group".into(), Value::Text(group.into()));
    record.insert("value".into(), Value::Int64(value));
    record
}

fn indexed_ids(db: &Db, group: &str) -> BTreeSet<String> {
    db.find_eq("items", "group", &Value::Text(group.into()))
        .unwrap()
        .into_iter()
        .map(|(id, row)| {
            assert_eq!(row["group"], Value::Text(group.into()));
            id
        })
        .collect()
}

fn canonical_ids(db: &Db, group: &str) -> BTreeSet<String> {
    db.scan("items")
        .unwrap()
        .into_iter()
        .filter_map(|(id, row)| (row["group"] == Value::Text(group.into())).then_some(id))
        .collect()
}

#[test]
fn equality_deltas_promote_without_resurrecting_old_pairs() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("secondary-levels.esql");
    let options = DbOptions {
        memtable_max_bytes: u64::MAX,
        ..DbOptions::default()
    };
    let db = Db::create_with(&path, options.clone()).unwrap();
    db.create_table(TableSchema::new(
        "items",
        vec![
            Column::new("group", ColumnType::Text),
            Column::new("value", ColumnType::Int64),
        ],
    ))
    .unwrap();
    db.create_index("items", "group", false).unwrap();

    let mut checkpoint_bytes = Vec::new();
    let mut previous_bytes = 0;
    for batch in 0..24 {
        let mut txn = db.begin();
        for row in 0..40 {
            let id = format!("id-{batch:03}-{row:03}");
            let group = if row % 2 == 0 { "hot" } else { "cold" };
            txn.insert("items", record(&id, group, (batch * 40 + row) as i64))
                .unwrap();
        }
        if batch > 0 {
            let moved = format!("id-{:03}-000", batch - 1);
            let mut patch = Record::new();
            patch.insert("group".into(), Value::Text("moved".into()));
            txn.update("items", &moved, patch).unwrap();
            let deleted = format!("id-{:03}-001", batch - 1);
            txn.delete("items", &deleted).unwrap();
        }
        txn.commit().unwrap();
        db.checkpoint().unwrap();
        let total = db.maintenance_stats().secondary_checkpoint_bytes_written;
        checkpoint_bytes.push(total - previous_bytes);
        previous_bytes = total;
    }
    db.wait_for_secondary_compaction().unwrap();

    assert_eq!(indexed_ids(&db, "hot"), canonical_ids(&db, "hot"));
    assert_eq!(indexed_ids(&db, "cold"), canonical_ids(&db, "cold"));
    assert_eq!(indexed_ids(&db, "moved"), canonical_ids(&db, "moved"));
    let smallest = *checkpoint_bytes
        .iter()
        .filter(|bytes| **bytes > 0)
        .min()
        .unwrap();
    let largest = *checkpoint_bytes.iter().max().unwrap();
    assert!(largest <= smallest * 3, "{checkpoint_bytes:?}");
    let stats = db.maintenance_stats();
    assert!(stats.secondary_run_compactions >= 2, "{stats:?}");
    assert!(stats.secondary_runs <= 12, "{stats:?}");
    assert!(stats.secondary_run_compaction_bytes_read > 0);
    assert!(stats.secondary_run_compaction_bytes_written > 0);
    drop(db);

    let reopened = Db::open_with(&path, options).unwrap();
    assert_eq!(
        indexed_ids(&reopened, "hot"),
        canonical_ids(&reopened, "hot")
    );
    assert_eq!(
        indexed_ids(&reopened, "moved"),
        canonical_ids(&reopened, "moved")
    );
}

#[test]
fn missing_secondary_level_is_rebuilt_from_canonical_data() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("missing-secondary-run.esql");
    {
        let db = Db::create(&path).unwrap();
        db.create_table(TableSchema::new(
            "items",
            vec![
                Column::new("group", ColumnType::Text),
                Column::new("value", ColumnType::Int64),
            ],
        ))
        .unwrap();
        db.create_index("items", "group", false).unwrap();
        for batch in 0..9 {
            let mut txn = db.begin();
            for row in 0..10 {
                let id = format!("id-{batch:03}-{row:03}");
                txn.insert("items", record(&id, "hot", (batch * 10 + row) as i64))
                    .unwrap();
            }
            txn.commit().unwrap();
            db.checkpoint().unwrap();
        }
        db.wait_for_secondary_compaction().unwrap();
    }

    let run = fs::read_dir(path.join("indexes"))
        .unwrap()
        .filter_map(|entry| entry.ok().map(|entry| entry.path()))
        .find(|path| {
            path.file_name()
                .and_then(|name| name.to_str())
                .is_some_and(|name| name.contains("-L") && name.ends_with(".sidx.run"))
        })
        .expect("secondary level run exists");
    fs::remove_file(run).unwrap();

    let reopened = Db::open(&path).unwrap();
    assert_eq!(indexed_ids(&reopened, "hot").len(), 90);
    assert_eq!(
        indexed_ids(&reopened, "hot"),
        canonical_ids(&reopened, "hot")
    );
}
