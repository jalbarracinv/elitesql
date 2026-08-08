use std::sync::Arc;

use elitesql_core::{
    check, AutoCompactionOptions, Column, ColumnType, Db, DbOptions, Durability, Record,
    TableSchema, Value,
};

fn schema() -> TableSchema {
    TableSchema::new(
        "docs",
        vec![
            Column::new("body", ColumnType::Text).not_null(),
            Column::new("generation", ColumnType::Int64).not_null(),
        ],
    )
}

fn record(id: &str, generation: i64) -> Record {
    let mut record = Record::new();
    record.insert("id".into(), Value::Text(id.into()));
    record.insert("body".into(), Value::Text("x".repeat(512)));
    record.insert("generation".into(), Value::Int64(generation));
    record
}

fn update(generation: i64) -> Record {
    let mut patch = Record::new();
    patch.insert("generation".into(), Value::Int64(generation));
    patch
}

fn auto_options(min_operations: u64) -> AutoCompactionOptions {
    AutoCompactionOptions {
        enabled: true,
        min_obsolete_operations: min_operations,
        min_reclaim_ratio_percent: 1,
        force_reclaim_bytes: u64::MAX,
        max_segments: 1_000,
        min_interval_ms: 0,
    }
}

fn db_options(auto_compaction: AutoCompactionOptions) -> DbOptions {
    DbOptions {
        durability: Durability::Fast,
        memtable_max_bytes: u64::MAX,
        auto_compaction,
        ..DbOptions::default()
    }
}

#[test]
fn obsolete_operation_threshold_compacts_and_reports_reclamation() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("auto.esql");
    let db = Db::create_with(&path, db_options(auto_options(10))).unwrap();
    db.create_table(schema()).unwrap();

    for i in 0..20 {
        db.insert("docs", record(&format!("row-{i:02}"), 0))
            .unwrap();
    }
    db.checkpoint().unwrap();
    assert_eq!(db.maintenance_stats().automatic_compactions, 0);

    for i in 0..10 {
        db.update("docs", &format!("row-{i:02}"), update(1))
            .unwrap();
    }
    for i in 10..15 {
        assert!(db.delete("docs", &format!("row-{i:02}")).unwrap());
    }
    db.checkpoint().unwrap();
    db.wait_for_automatic_compaction();

    let stats = db.maintenance_stats();
    assert_eq!(stats.automatic_compactions, 1, "{stats:?}");
    assert_eq!(stats.automatic_compaction_failures, 0);
    assert!(stats.automatic_compaction_bytes_reclaimed > 0, "{stats:?}");
    assert_eq!(stats.compaction_debt_operations, 0, "{stats:?}");
    assert_eq!(stats.estimated_reclaimable_bytes, 0, "{stats:?}");
    assert_eq!(stats.segments, 1);
    assert_eq!(db.scan("docs").unwrap().len(), 15);
    drop(db);

    let db = Db::open(&path).unwrap();
    assert_eq!(db.scan("docs").unwrap().len(), 15);
    drop(db);
    let report = check(&path).unwrap();
    assert!(report.is_ok(), "{:?}", report.errors);
}

#[test]
fn insert_only_workload_waits_for_the_segment_limit() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("segments.esql");
    let options = AutoCompactionOptions {
        max_segments: 4,
        ..auto_options(1)
    };
    let db = Db::create_with(&path, db_options(options)).unwrap();
    db.create_table(schema()).unwrap();

    for i in 0..3 {
        db.insert("docs", record(&format!("row-{i}"), 0)).unwrap();
        db.checkpoint().unwrap();
    }
    db.wait_for_automatic_compaction();
    assert_eq!(db.maintenance_stats().automatic_compactions, 0);
    assert_eq!(db.maintenance_stats().segments, 3);

    db.insert("docs", record("row-3", 0)).unwrap();
    db.checkpoint().unwrap();
    db.wait_for_automatic_compaction();
    let stats = db.maintenance_stats();
    assert_eq!(stats.automatic_compactions, 1, "{stats:?}");
    assert_eq!(stats.segments, 1);
    assert_eq!(db.scan("docs").unwrap().len(), 4);
}

#[test]
fn disabled_policy_accumulates_debt_until_manual_compaction() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("disabled.esql");
    let db = Db::create_with(
        &path,
        db_options(AutoCompactionOptions {
            enabled: false,
            ..auto_options(1)
        }),
    )
    .unwrap();
    db.create_table(schema()).unwrap();
    db.insert("docs", record("row", 0)).unwrap();
    db.checkpoint().unwrap();
    db.update("docs", "row", update(1)).unwrap();
    db.checkpoint().unwrap();
    db.wait_for_automatic_compaction();

    let before = db.maintenance_stats();
    assert_eq!(before.automatic_compactions, 0);
    assert!(before.compaction_debt_operations > 0, "{before:?}");
    assert!(before.estimated_reclaimable_bytes > 0, "{before:?}");
    db.compact().unwrap();
    let after = db.maintenance_stats();
    assert_eq!(after.compaction_debt_operations, 0, "{after:?}");
    assert_eq!(after.estimated_reclaimable_bytes, 0, "{after:?}");
}

#[test]
fn live_snapshot_versions_survive_automatic_compaction() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("snapshot.esql");
    let db = Db::create_with(&path, db_options(auto_options(1))).unwrap();
    db.create_table(schema()).unwrap();
    db.insert("docs", record("row", 1)).unwrap();
    db.checkpoint().unwrap();
    db.update("docs", "row", update(2)).unwrap();
    let snapshot = db.snapshot();
    db.update("docs", "row", update(3)).unwrap();
    db.checkpoint().unwrap();
    db.wait_for_automatic_compaction();

    assert_eq!(db.maintenance_stats().automatic_compactions, 1);
    assert_eq!(
        db.get_at(&snapshot, "docs", "row").unwrap().unwrap()["generation"],
        Value::Int64(2)
    );
    assert_eq!(
        db.get("docs", "row").unwrap().unwrap()["generation"],
        Value::Int64(3)
    );

    drop(snapshot);
    db.checkpoint().unwrap();
    db.wait_for_automatic_compaction();
    assert_eq!(db.maintenance_stats().automatic_compactions, 2);
    assert_eq!(
        db.get("docs", "row").unwrap().unwrap()["generation"],
        Value::Int64(3)
    );
}

#[test]
fn debt_is_reconstructed_on_open() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("reopen.esql");
    let disabled = AutoCompactionOptions {
        enabled: false,
        ..auto_options(1)
    };
    let db = Db::create_with(&path, db_options(disabled)).unwrap();
    db.create_table(schema()).unwrap();
    db.insert("docs", record("row", 0)).unwrap();
    db.checkpoint().unwrap();
    db.update("docs", "row", update(1)).unwrap();
    db.checkpoint().unwrap();
    drop(db);

    let db = Db::open_with(&path, db_options(auto_options(1))).unwrap();
    db.wait_for_automatic_compaction();
    let stats = db.maintenance_stats();
    assert_eq!(stats.automatic_compactions, 1, "{stats:?}");
    assert_eq!(stats.compaction_debt_operations, 0, "{stats:?}");
    assert_eq!(
        db.get("docs", "row").unwrap().unwrap()["generation"],
        Value::Int64(1)
    );
}

#[test]
fn concurrent_writers_remain_consistent_while_auto_compacting() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("concurrent.esql");
    let db = Db::create_with(
        &path,
        DbOptions {
            durability: Durability::Fast,
            memtable_max_bytes: 1,
            auto_compaction: AutoCompactionOptions {
                min_obsolete_operations: 8,
                min_reclaim_ratio_percent: 1,
                force_reclaim_bytes: u64::MAX,
                max_segments: 32,
                min_interval_ms: 0,
                enabled: true,
            },
            ..DbOptions::default()
        },
    )
    .unwrap();
    db.create_table(schema()).unwrap();
    for worker in 0..4 {
        db.insert("docs", record(&format!("worker-{worker}"), 0))
            .unwrap();
    }

    let db = Arc::new(db);
    let mut handles = Vec::new();
    for worker in 0..4 {
        let db = Arc::clone(&db);
        handles.push(std::thread::spawn(move || {
            let id = format!("worker-{worker}");
            for generation in 1..=40 {
                db.update("docs", &id, update(generation)).unwrap();
            }
        }));
    }
    for handle in handles {
        handle.join().unwrap();
    }
    db.wait_for_automatic_compaction();
    assert!(db.maintenance_stats().automatic_compactions > 0);
    for worker in 0..4 {
        assert_eq!(
            db.get("docs", &format!("worker-{worker}"))
                .unwrap()
                .unwrap()["generation"],
            Value::Int64(40)
        );
    }
    drop(db);
    let report = check(&path).unwrap();
    assert!(report.is_ok(), "{:?}", report.errors);
}
