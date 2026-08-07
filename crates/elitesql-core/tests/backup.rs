//! Backup and restore: snapshot-consistent logical backup (including under
//! concurrent writers), verified restore, and refusal paths.

use std::collections::BTreeMap;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

use elitesql_core::{
    check, restore, Column, ColumnType, Db, DbOptions, Record, TableSchema, Value,
    VectorIndexOptions, VectorSearchOptions,
};
use tempfile::TempDir;

fn note(body: &str, n: i64, emb: [f32; 4]) -> Record {
    let mut r = Record::new();
    r.insert("body".into(), Value::Text(body.into()));
    r.insert("n".into(), Value::Int64(n));
    r.insert("emb".into(), Value::Vector(emb.to_vec()));
    r
}

/// A database exercising every index kind plus out-of-line blobs.
fn populated_db() -> (TempDir, Db) {
    let dir = tempfile::tempdir().unwrap();
    let db = Db::create_with(
        dir.path().join("src.esql"),
        DbOptions {
            external_blob_threshold: 64,
            ..DbOptions::default()
        },
    )
    .unwrap();
    db.create_table(TableSchema::new(
        "notes",
        vec![
            Column::new("body", ColumnType::Text).not_null(),
            Column::new("n", ColumnType::Int64),
            Column::vector("emb", 4),
        ],
    ))
    .unwrap();
    db.create_index("notes", "n", false).unwrap();
    db.create_text_index("notes", "body").unwrap();
    db.create_vector_index("notes", "emb", VectorIndexOptions::default())
        .unwrap();
    db.create_table(TableSchema::new(
        "files",
        vec![Column::new("data", ColumnType::Blob)],
    ))
    .unwrap();

    db.insert("notes", note("rust database engine", 1, [1.0, 0.0, 0.0, 0.0]))
        .unwrap();
    db.insert("notes", note("python tutorial", 2, [0.0, 1.0, 0.0, 0.0]))
        .unwrap();
    db.insert("notes", note("rust vectors and search", 1, [0.9, 0.1, 0.0, 0.0]))
        .unwrap();
    let mut rec = Record::new();
    rec.insert("data".into(), Value::Blob(vec![7u8; 4096])); // out-of-line
    db.insert("files", rec).unwrap();
    let mut rec = Record::new();
    rec.insert("data".into(), Value::Blob(vec![9u8; 16])); // inline
    db.insert("files", rec).unwrap();
    (dir, db)
}

fn full_scan(db: &Db) -> BTreeMap<(String, String), Record> {
    let mut out = BTreeMap::new();
    for table in db.tables() {
        for (id, rec) in db.scan(&table).unwrap() {
            out.insert((table.clone(), id), rec);
        }
    }
    out
}

#[test]
fn backup_roundtrip_preserves_data_and_indexes() {
    let (dir, db) = populated_db();
    let dst = dir.path().join("backup.esql");

    let report = db.backup(&dst).unwrap();
    assert_eq!(report.tables, 2);
    assert_eq!(report.records, 5);
    assert!(check(&dst).unwrap().is_ok(), "backup validates offline");
    assert!(!dir.path().join("backup.esql.partial").exists());

    let copy = Db::open(&dst).unwrap();
    assert_eq!(full_scan(&db), full_scan(&copy), "same ids, same records");

    // Every index kind survived: secondary, text (BM25) and vector (ANN).
    assert_eq!(
        copy.find_eq("notes", "n", &Value::Int64(1)).unwrap().len(),
        2
    );
    let hits = copy.search_text("notes", "body", "rust", 10, None).unwrap();
    assert_eq!(hits.len(), 2);
    let hits = copy
        .search_vector(
            "notes",
            "emb",
            &[1.0, 0.0, 0.0, 0.0],
            1,
            &VectorSearchOptions::default(),
        )
        .unwrap();
    assert_eq!(hits.len(), 1);
}

#[test]
fn backup_is_snapshot_consistent_under_concurrent_writers() {
    let dir = tempfile::tempdir().unwrap();
    let db = Arc::new(Db::create(dir.path().join("src.esql")).unwrap());
    db.create_table(TableSchema::new(
        "events",
        vec![Column::new("seq", ColumnType::Int64)],
    ))
    .unwrap();
    let mut initial = Vec::new();
    for i in 0..200 {
        let mut r = Record::new();
        r.insert("seq".into(), Value::Int64(i));
        initial.push(db.insert("events", r).unwrap());
    }

    let stop = Arc::new(AtomicBool::new(false));
    let writers: Vec<_> = (0..4)
        .map(|w| {
            let db = Arc::clone(&db);
            let stop = Arc::clone(&stop);
            std::thread::spawn(move || {
                let mut i = 0i64;
                while !stop.load(Ordering::Relaxed) {
                    let mut r = Record::new();
                    r.insert("seq".into(), Value::Int64(1_000 * (w + 1) + i));
                    db.insert("events", r).unwrap();
                    i += 1;
                }
            })
        })
        .collect();

    let dst = dir.path().join("hot.esql");
    let report = db.backup(&dst).unwrap();
    stop.store(true, Ordering::Relaxed);
    for w in writers {
        w.join().unwrap();
    }

    assert!(report.records >= 200);
    assert!(check(&dst).unwrap().is_ok());
    let copy = Db::open(&dst).unwrap();
    let copied = copy.scan("events").unwrap();
    assert!(copied.len() as u64 >= 200);
    let copied_ids: std::collections::HashSet<_> =
        copied.iter().map(|(id, _)| id.clone()).collect();
    for id in &initial {
        assert!(copied_ids.contains(id), "pre-backup commit missing: {id}");
    }
    // Everything the backup holds was committed in the source.
    let src_ids: std::collections::HashSet<_> = db
        .scan("events")
        .unwrap()
        .into_iter()
        .map(|(id, _)| id)
        .collect();
    assert!(copied_ids.is_subset(&src_ids));
}

#[test]
fn backup_refuses_existing_destination() {
    let (dir, db) = populated_db();
    let dst = dir.path().join("backup.esql");
    db.backup(&dst).unwrap();
    let err = db.backup(&dst).unwrap_err();
    assert!(err.to_string().contains("already exists"), "{err}");
}

#[test]
fn restore_roundtrip() {
    let (dir, db) = populated_db();
    let backup = dir.path().join("backup.esql");
    db.backup(&backup).unwrap();

    let restored_path = dir.path().join("restored.esql");
    let report = restore(&backup, &restored_path).unwrap();
    assert_eq!(report.tables, 2);
    assert_eq!(report.records, 5);

    let restored = Db::open(&restored_path).unwrap();
    assert_eq!(full_scan(&db), full_scan(&restored));
    let hits = restored
        .search_text("notes", "body", "rust", 10, None)
        .unwrap();
    assert_eq!(hits.len(), 2, "derived indexes rebuilt on restore");
}

#[test]
fn restore_refuses_corrupt_backup_and_existing_destination() {
    let (dir, db) = populated_db();
    let backup = dir.path().join("backup.esql");
    db.backup(&backup).unwrap();

    let occupied = dir.path().join("src.esql");
    let err = restore(&backup, &occupied).unwrap_err();
    assert!(err.to_string().contains("already exists"), "{err}");

    // Flip one byte inside a segment: check must fail and restore refuse.
    let seg_dir = backup.join("segments");
    let seg = std::fs::read_dir(&seg_dir)
        .unwrap()
        .map(|e| e.unwrap().path())
        .find(|p| p.extension().is_some_and(|e| e == "seg"))
        .expect("backup has a segment");
    let mut bytes = std::fs::read(&seg).unwrap();
    let mid = bytes.len() / 2;
    bytes[mid] ^= 0xFF;
    std::fs::write(&seg, bytes).unwrap();

    let dst = dir.path().join("from-corrupt.esql");
    let err = restore(&backup, &dst).unwrap_err();
    assert!(err.to_string().contains("failed check"), "{err}");
    assert!(!dst.exists(), "nothing materialized from a bad backup");
}
