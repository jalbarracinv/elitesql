use clawdb_core::{check, Column, ColumnType, Db, Record, TableSchema, Value};

fn schema() -> TableSchema {
    TableSchema::new(
        "docs",
        vec![
            Column::new("title", ColumnType::Text).not_null(),
            Column::new("score", ColumnType::Int64),
        ],
    )
}

fn record(title: &str, score: i64) -> Record {
    let mut r = Record::new();
    r.insert("title".into(), Value::Text(title.into()));
    r.insert("score".into(), Value::Int64(score));
    r
}

fn dir_size(path: &std::path::Path) -> u64 {
    let mut total = 0;
    for entry in walk(path) {
        total += std::fs::metadata(&entry).map(|m| m.len()).unwrap_or(0);
    }
    total
}

fn walk(path: &std::path::Path) -> Vec<std::path::PathBuf> {
    let mut out = Vec::new();
    if let Ok(entries) = std::fs::read_dir(path) {
        for e in entries.flatten() {
            let p = e.path();
            if p.is_dir() {
                out.extend(walk(&p));
            } else {
                out.push(p);
            }
        }
    }
    out
}

#[test]
fn compaction_preserves_live_snapshot_versions() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("test.clawdb");
    let db = Db::create(&path).unwrap();
    db.create_table(schema()).unwrap();

    let mut ids = Vec::new();
    for i in 0..50 {
        ids.push(db.insert("docs", record(&format!("v1-{i}"), 1)).unwrap());
    }
    for id in &ids {
        let mut p = Record::new();
        p.insert("score".into(), Value::Int64(2));
        db.update("docs", id, p).unwrap();
    }
    let snap_v2 = db.snapshot();
    for id in &ids {
        let mut p = Record::new();
        p.insert("score".into(), Value::Int64(3));
        db.update("docs", id, p).unwrap();
    }

    // Compact with the snapshot alive: version-2 states must survive,
    // version-1 states may be pruned.
    db.compact().unwrap();

    for id in &ids {
        assert_eq!(
            db.get_at(&snap_v2, "docs", id).unwrap().unwrap()["score"],
            Value::Int64(2),
            "live snapshot still reads its version after compaction"
        );
        assert_eq!(
            db.get("docs", id).unwrap().unwrap()["score"],
            Value::Int64(3),
            "latest state intact"
        );
    }

    // Drop the snapshot and compact again: only the latest versions remain.
    drop(snap_v2);
    db.compact().unwrap();
    for id in &ids {
        assert_eq!(db.get("docs", id).unwrap().unwrap()["score"], Value::Int64(3));
    }
    drop(db);
    // Everything still consistent after reopen.
    let db = Db::open(&path).unwrap();
    assert_eq!(db.scan("docs").unwrap().len(), 50);
    let report = check(&path).unwrap();
    assert!(report.is_ok(), "{:?}", report.errors);
}

#[test]
fn compaction_purges_deleted_records_and_shrinks_disk() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("test.clawdb");
    let db = Db::create(&path).unwrap();
    db.create_table(schema()).unwrap();

    let filler = "x".repeat(500);
    let mut ids = Vec::new();
    for i in 0..200 {
        ids.push(db.insert("docs", record(&filler, i)).unwrap());
    }
    // Update everything a few times to accumulate garbage versions.
    for round in 0..3 {
        for id in &ids {
            let mut p = Record::new();
            p.insert("score".into(), Value::Int64(round));
            db.update("docs", id, p).unwrap();
        }
    }
    // Delete half.
    for id in ids.iter().take(100) {
        db.delete("docs", id).unwrap();
    }
    db.checkpoint().unwrap();
    let before = dir_size(&path);

    db.compact().unwrap();
    let after = dir_size(&path);
    assert!(
        after < before / 2,
        "compaction should reclaim most garbage: {before} -> {after}"
    );

    assert_eq!(db.scan("docs").unwrap().len(), 100);
    for id in ids.iter().take(100) {
        assert!(db.get("docs", id).unwrap().is_none(), "deleted stays deleted");
    }
    drop(db);
    let db = Db::open(&path).unwrap();
    assert_eq!(db.scan("docs").unwrap().len(), 100);
}

#[test]
fn compaction_of_empty_and_tombstone_only_tables() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("test.clawdb");
    let db = Db::create(&path).unwrap();
    db.create_table(schema()).unwrap();

    // Insert then delete everything: compaction should drop it all.
    let id = db.insert("docs", record("gone", 1)).unwrap();
    db.delete("docs", &id).unwrap();
    db.compact().unwrap();
    assert!(db.scan("docs").unwrap().is_empty());

    // Compacting an effectively empty db is a no-op that must not break state.
    db.compact().unwrap();
    let id2 = db.insert("docs", record("fresh", 2)).unwrap();
    assert!(db.get("docs", &id2).unwrap().is_some());
    drop(db);
    let db = Db::open(&path).unwrap();
    assert_eq!(db.scan("docs").unwrap().len(), 1);
}

#[test]
fn secondary_index_correct_after_compaction() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("test.clawdb");
    let db = Db::create(&path).unwrap();
    db.create_table(schema()).unwrap();
    db.create_index("docs", "score", false).unwrap();

    for i in 0..30 {
        db.insert("docs", record(&format!("d{i}"), i % 3)).unwrap();
    }
    db.compact().unwrap();
    assert_eq!(db.find_eq("docs", "score", &Value::Int64(1)).unwrap().len(), 10);
    drop(db);
    let db = Db::open(&path).unwrap();
    assert_eq!(db.find_eq("docs", "score", &Value::Int64(1)).unwrap().len(), 10);
}
