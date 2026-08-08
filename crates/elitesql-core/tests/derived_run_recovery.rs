use std::fs;

use elitesql_core::{Column, ColumnType, Db, Record, TableSchema, Value};

#[test]
fn corrupt_secondary_and_text_manifests_rebuild_from_canonical_data() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("corrupt-derived-manifests.esql");
    {
        let db = Db::create(&path).unwrap();
        db.create_table(TableSchema::new(
            "docs",
            vec![
                Column::new("group", ColumnType::Text),
                Column::new("body", ColumnType::Text),
            ],
        ))
        .unwrap();
        db.create_index("docs", "group", false).unwrap();
        db.create_text_index("docs", "body").unwrap();
        let mut txn = db.begin();
        for row in 0..100 {
            let mut record = Record::new();
            record.insert("id".into(), Value::Text(format!("id-{row:03}")));
            record.insert("group".into(), Value::Text("hot".into()));
            record.insert("body".into(), Value::Text("alpha common".into()));
            txn.insert("docs", record).unwrap();
        }
        txn.commit().unwrap();
        db.checkpoint().unwrap();
    }

    let manifests: Vec<_> = fs::read_dir(path.join("indexes"))
        .unwrap()
        .filter_map(|entry| entry.ok().map(|entry| entry.path()))
        .filter(|path| {
            path.file_name()
                .and_then(|name| name.to_str())
                .is_some_and(|name| name.ends_with(".sidx.runs") || name.ends_with(".tidx.runs"))
        })
        .collect();
    assert_eq!(manifests.len(), 2);
    for manifest in manifests {
        fs::write(manifest, b"damaged").unwrap();
    }

    let reopened = Db::open(&path).unwrap();
    assert_eq!(
        reopened
            .find_eq("docs", "group", &Value::Text("hot".into()))
            .unwrap()
            .len(),
        100
    );
    assert_eq!(
        reopened
            .search_text("docs", "body", "alpha", 200, None)
            .unwrap()
            .len(),
        100
    );
    drop(reopened);

    let reopened_again = Db::open(&path).unwrap();
    assert_eq!(
        reopened_again
            .find_eq("docs", "group", &Value::Text("hot".into()))
            .unwrap()
            .len(),
        100
    );
    assert_eq!(
        reopened_again
            .search_text("docs", "body", "alpha", 200, None)
            .unwrap()
            .len(),
        100
    );
}
