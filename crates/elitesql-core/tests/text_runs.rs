use std::collections::BTreeSet;
use std::fs;

use elitesql_core::{Column, ColumnType, Db, DbOptions, Record, TableSchema, Value};

fn record(id: &str, body: &str) -> Record {
    let mut record = Record::new();
    record.insert("id".into(), Value::Text(id.into()));
    record.insert("body".into(), Value::Text(body.into()));
    record
}

fn search_ids(db: &Db, term: &str) -> BTreeSet<String> {
    db.search_text("docs", "body", term, 10_000, None)
        .unwrap()
        .into_iter()
        .map(|hit| {
            assert!(
                matches!(hit.record.get("body"), Some(Value::Text(body)) if body.contains(term))
            );
            hit.id
        })
        .collect()
}

fn canonical_ids(db: &Db, term: &str) -> BTreeSet<String> {
    db.scan("docs")
        .unwrap()
        .into_iter()
        .filter_map(|(id, row)| match &row["body"] {
            Value::Text(body) if body.split_whitespace().any(|token| token == term) => Some(id),
            _ => None,
        })
        .collect()
}

#[test]
fn bm25_deltas_promote_without_resurrecting_postings() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("text-levels.esql");
    let options = DbOptions {
        memtable_max_bytes: u64::MAX,
        ..DbOptions::default()
    };
    let db = Db::create_with(&path, options.clone()).unwrap();
    db.create_table(TableSchema::new(
        "docs",
        vec![Column::new("body", ColumnType::Text)],
    ))
    .unwrap();
    db.create_text_index("docs", "body").unwrap();

    let mut checkpoint_bytes = Vec::new();
    let mut previous_bytes = 0;
    for batch in 0..24 {
        let mut txn = db.begin();
        for row in 0..20 {
            let id = format!("id-{batch:03}-{row:03}");
            let body = if row % 2 == 0 {
                "alpha common common"
            } else {
                "beta common"
            };
            txn.insert("docs", record(&id, body)).unwrap();
        }
        if batch > 0 {
            let moved = format!("id-{:03}-000", batch - 1);
            let mut patch = Record::new();
            patch.insert("body".into(), Value::Text("gamma common".into()));
            txn.update("docs", &moved, patch).unwrap();
            let deleted = format!("id-{:03}-001", batch - 1);
            txn.delete("docs", &deleted).unwrap();
        }
        txn.commit().unwrap();
        db.checkpoint().unwrap();
        let total = db.maintenance_stats().text_checkpoint_bytes_written;
        checkpoint_bytes.push(total - previous_bytes);
        previous_bytes = total;
    }
    db.wait_for_text_compaction();

    for term in ["alpha", "beta", "gamma", "common"] {
        assert_eq!(search_ids(&db, term), canonical_ids(&db, term), "{term}");
    }
    let smallest = *checkpoint_bytes
        .iter()
        .filter(|bytes| **bytes > 0)
        .min()
        .unwrap();
    let largest = *checkpoint_bytes.iter().max().unwrap();
    assert!(largest <= smallest * 3, "{checkpoint_bytes:?}");
    let stats = db.maintenance_stats();
    assert!(stats.text_run_compactions >= 2, "{stats:?}");
    assert!(stats.text_runs <= 12, "{stats:?}");
    assert!(stats.text_run_compaction_bytes_read > 0);
    assert!(stats.text_run_compaction_bytes_written > 0);
    let scores_before_reopen: Vec<_> = db
        .search_text("docs", "body", "alpha common", 100, None)
        .unwrap()
        .into_iter()
        .map(|hit| (hit.id, hit.score))
        .collect();
    drop(db);

    let reopened = Db::open_with(&path, options).unwrap();
    for term in ["alpha", "gamma", "common"] {
        assert_eq!(
            search_ids(&reopened, term),
            canonical_ids(&reopened, term),
            "{term} after reopen"
        );
    }
    let scores_after_reopen: Vec<_> = reopened
        .search_text("docs", "body", "alpha common", 100, None)
        .unwrap()
        .into_iter()
        .map(|hit| (hit.id, hit.score))
        .collect();
    assert_eq!(scores_before_reopen.len(), scores_after_reopen.len());
    for ((before_id, before_score), (after_id, after_score)) in
        scores_before_reopen.iter().zip(&scores_after_reopen)
    {
        assert_eq!(before_id, after_id);
        assert_eq!(before_score.to_bits(), after_score.to_bits());
    }
}

#[test]
fn missing_text_level_is_rebuilt_from_canonical_data() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("missing-text-run.esql");
    {
        let db = Db::create(&path).unwrap();
        db.create_table(TableSchema::new(
            "docs",
            vec![Column::new("body", ColumnType::Text)],
        ))
        .unwrap();
        db.create_text_index("docs", "body").unwrap();
        for batch in 0..9 {
            let mut txn = db.begin();
            for row in 0..10 {
                let id = format!("id-{batch:03}-{row:03}");
                txn.insert("docs", record(&id, "alpha common")).unwrap();
            }
            txn.commit().unwrap();
            db.checkpoint().unwrap();
        }
        db.wait_for_text_compaction();
    }

    let run = fs::read_dir(path.join("indexes"))
        .unwrap()
        .filter_map(|entry| entry.ok().map(|entry| entry.path()))
        .find(|path| {
            path.file_name()
                .and_then(|name| name.to_str())
                .is_some_and(|name| name.contains("-L") && name.ends_with(".tidx.run"))
        })
        .expect("text level run exists");
    fs::remove_file(run).unwrap();

    let reopened = Db::open(&path).unwrap();
    assert_eq!(search_ids(&reopened, "alpha").len(), 90);
    assert_eq!(
        search_ids(&reopened, "alpha"),
        canonical_ids(&reopened, "alpha")
    );
}
