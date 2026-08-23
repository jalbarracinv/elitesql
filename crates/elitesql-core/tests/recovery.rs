use std::fs::OpenOptions;
use std::io::Write;
use std::path::Path;

use elitesql_core::{
    check, Column, ColumnType, Db, DbOptions, Durability, Error, Record, TableSchema, Value,
};

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

fn wal_file(db_path: &Path) -> std::path::PathBuf {
    let wal_dir = db_path.join("wal");
    let mut files: Vec<_> = std::fs::read_dir(&wal_dir)
        .unwrap()
        .flatten()
        .map(|d| d.path())
        .filter(|p| p.extension().is_some_and(|e| e == "wal"))
        .collect();
    files.sort();
    files.pop().expect("wal file present")
}

fn first_wal_record_len(bytes: &[u8]) -> usize {
    let mut pos = 8;
    let count = u32::from_le_bytes(bytes[pos..pos + 4].try_into().unwrap()) as usize;
    pos += 4;
    for _ in 0..count {
        pos += 1;
        let table_len = u16::from_le_bytes(bytes[pos..pos + 2].try_into().unwrap()) as usize;
        pos += 2 + table_len;
        let id_len = u16::from_le_bytes(bytes[pos..pos + 2].try_into().unwrap()) as usize;
        pos += 2 + id_len;
        let payload_len = u32::from_le_bytes(bytes[pos..pos + 4].try_into().unwrap()) as usize;
        pos += 4 + payload_len;
    }
    pos + 4
}

#[test]
fn wal_replay_restores_unclean_close() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("test.esql");
    let mut ids = Vec::new();
    {
        let db = Db::create(&path).unwrap();
        db.create_table(schema()).unwrap();
        for i in 0..20 {
            ids.push(db.insert("docs", record(&format!("doc {i}"), i)).unwrap());
        }
        // No checkpoint: everything lives in the WAL. Drop without cleanup.
    }
    let db = Db::open(&path).unwrap();
    for (i, id) in ids.iter().enumerate() {
        let rec = db.get("docs", id).unwrap().unwrap();
        assert_eq!(rec["score"], Value::Int64(i as i64));
    }
}

#[test]
fn checkpoint_plus_wal_tail_restores_both() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("test.esql");
    let (a, b);
    {
        let db = Db::create(&path).unwrap();
        db.create_table(schema()).unwrap();
        a = db.insert("docs", record("in segment", 1)).unwrap();
        db.checkpoint().unwrap();
        b = db.insert("docs", record("in wal", 2)).unwrap();
    }
    let db = Db::open(&path).unwrap();
    assert!(db.get("docs", &a).unwrap().is_some(), "checkpointed data");
    assert!(db.get("docs", &b).unwrap().is_some(), "wal tail data");
    assert_eq!(db.scan("docs").unwrap().len(), 2);
}

#[test]
fn torn_wal_tail_drops_whole_commit_atomically() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("test.esql");
    let (a1, a2, b1, b2);
    {
        let db = Db::create(&path).unwrap();
        db.create_table(schema()).unwrap();
        // txn A: two records in one commit.
        let mut txn = db.begin();
        a1 = txn.insert("docs", record("a1", 1)).unwrap();
        a2 = txn.insert("docs", record("a2", 2)).unwrap();
        txn.commit().unwrap();
        // txn B: two records in one commit; will be torn.
        let mut txn = db.begin();
        b1 = txn.insert("docs", record("b1", 3)).unwrap();
        b2 = txn.insert("docs", record("b2", 4)).unwrap();
        txn.commit().unwrap();
    }
    // Tear the tail: chop bytes off the last WAL record.
    let wal = wal_file(&path);
    let len = std::fs::metadata(&wal).unwrap().len();
    let f = OpenOptions::new().write(true).open(&wal).unwrap();
    f.set_len(len - 5).unwrap();

    let db = Db::open(&path).unwrap();
    assert!(db.get("docs", &a1).unwrap().is_some(), "commit A intact");
    assert!(db.get("docs", &a2).unwrap().is_some(), "commit A intact");
    assert!(
        db.get("docs", &b1).unwrap().is_none(),
        "commit B fully dropped"
    );
    assert!(
        db.get("docs", &b2).unwrap().is_none(),
        "commit B fully dropped"
    );

    // The database keeps working after truncation.
    let c = db.insert("docs", record("after recovery", 5)).unwrap();
    drop(db);
    let db = Db::open(&path).unwrap();
    assert!(db.get("docs", &c).unwrap().is_some());
}

#[test]
fn garbage_wal_tail_is_ignored() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("test.esql");
    let id;
    {
        let db = Db::create(&path).unwrap();
        db.create_table(schema()).unwrap();
        id = db.insert("docs", record("solid", 1)).unwrap();
    }
    let wal = wal_file(&path);
    let mut f = OpenOptions::new().append(true).open(&wal).unwrap();
    f.write_all(&[0xDE, 0xAD, 0xBE, 0xEF, 0x00, 0x01, 0x02])
        .unwrap();

    let db = Db::open(&path).unwrap();
    assert!(db.get("docs", &id).unwrap().is_some());
    assert_eq!(db.scan("docs").unwrap().len(), 1);
}

#[test]
fn corrupt_manifest_falls_back_to_prev() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("test.esql");
    {
        let db = Db::create(&path).unwrap();
        db.create_table(schema()).unwrap();
        for i in 0..5 {
            let mut r = record(&format!("set A {i}"), i);
            r.insert("id".into(), Value::Text(format!("a-{i}")));
            db.insert("docs", r).unwrap();
        }
        db.checkpoint().unwrap(); // manifest M1: set A in segments
        for i in 0..5 {
            let mut r = record(&format!("set B {i}"), i);
            r.insert("id".into(), Value::Text(format!("b-{i}")));
            db.insert("docs", r).unwrap();
        }
        db.checkpoint().unwrap(); // manifest M2: A+B; fallback is refreshed to M2
    }
    // Corrupt the primary manifest body.
    let manifest_path = path.join("manifest");
    let mut bytes = std::fs::read(&manifest_path).unwrap();
    let mid = bytes.len() / 2;
    bytes[mid] ^= 0xFF;
    std::fs::write(&manifest_path, &bytes).unwrap();

    // Opens via a redundant manifest.prev of M2. A successfully acknowledged
    // checkpoint must never disappear merely because the primary copy breaks.
    let db = Db::open(&path).unwrap();
    for i in 0..5 {
        assert!(
            db.get("docs", &format!("a-{i}")).unwrap().is_some(),
            "set A survives"
        );
    }
    for i in 0..5 {
        assert!(
            db.get("docs", &format!("b-{i}")).unwrap().is_some(),
            "set B survives"
        );
    }
    assert_eq!(db.scan("docs").unwrap().len(), 10);

    // The healed manifest must be a normal, working database.
    let c = db.insert("docs", record("post heal", 9)).unwrap();
    drop(db);
    let db = Db::open(&path).unwrap();
    assert!(db.get("docs", &c).unwrap().is_some());
    assert_eq!(db.scan("docs").unwrap().len(), 11);
}

#[test]
fn manifest_fallback_keeps_acknowledged_catalog_generation() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("catalog-fallback.esql");
    {
        let db = Db::create(&path).unwrap();
        db.create_table(schema()).unwrap();
        let mut first = record("one", 1);
        first.insert("id".into(), Value::Text("one".into()));
        db.insert("docs", first).unwrap();
        db.add_column("docs", Column::new("tag", ColumnType::Text))
            .unwrap();
        db.create_index("docs", "title", true).unwrap();
    }

    let manifest_path = path.join("manifest");
    let mut manifest = std::fs::read(&manifest_path).unwrap();
    let mid = manifest.len() / 2;
    manifest[mid] ^= 0xFF;
    std::fs::write(&manifest_path, manifest).unwrap();
    // The standalone catalog is only a compatibility mirror now.
    std::fs::write(path.join("catalog.json"), b"not json").unwrap();

    let db = Db::open(&path).unwrap();
    assert!(db
        .table_schema("docs")
        .unwrap()
        .columns
        .iter()
        .any(|column| column.name == "tag"));
    let mut duplicate = record("one", 2);
    duplicate.insert("id".into(), Value::Text("two".into()));
    assert!(matches!(
        db.insert("docs", duplicate),
        Err(Error::UniqueViolation { .. })
    ));
}

#[test]
fn manifest_fallback_after_compaction_keeps_external_blobs() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("blob-fallback.esql");
    {
        let db = Db::create_with(
            &path,
            DbOptions {
                external_blob_threshold: 8,
                ..DbOptions::default()
            },
        )
        .unwrap();
        db.create_table(TableSchema::new(
            "files",
            vec![Column::new("data", ColumnType::Blob)],
        ))
        .unwrap();
        let mut original = Record::new();
        original.insert("id".into(), Value::Text("file".into()));
        original.insert("data".into(), Value::Blob(vec![1; 4096]));
        db.insert("files", original).unwrap();
        db.checkpoint().unwrap();
        let mut patch = Record::new();
        patch.insert("data".into(), Value::Blob(vec![2; 8192]));
        db.update("files", "file", patch).unwrap();
        db.compact().unwrap();
    }

    let manifest_path = path.join("manifest");
    let mut manifest = std::fs::read(&manifest_path).unwrap();
    let mid = manifest.len() / 2;
    manifest[mid] ^= 0xFF;
    std::fs::write(manifest_path, manifest).unwrap();

    let db = Db::open(&path).unwrap();
    assert_eq!(
        db.get("files", "file").unwrap().unwrap()["data"],
        Value::Blob(vec![2; 8192])
    );
}

#[test]
fn missing_active_wal_is_corruption_not_an_empty_replacement() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("missing-wal.esql");
    {
        let db = Db::create(&path).unwrap();
        db.create_table(schema()).unwrap();
        db.insert("docs", record("only in wal", 1)).unwrap();
    }
    std::fs::remove_file(wal_file(&path)).unwrap();

    assert!(matches!(Db::open(&path), Err(Error::Corrupt(_))));
    let report = check(&path).unwrap();
    assert!(!report.is_ok());
    assert!(report.errors.iter().any(|error| error.contains("wal")));
}

#[test]
fn a_wal_version_gap_is_rejected_instead_of_losing_the_missing_commit() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("wal-gap.esql");
    {
        let db = Db::create(&path).unwrap();
        db.create_table(schema()).unwrap();
        db.insert("docs", record("first", 1)).unwrap();
        db.insert("docs", record("second", 2)).unwrap();
    }
    let wal = wal_file(&path);
    let bytes = std::fs::read(&wal).unwrap();
    let first_len = first_wal_record_len(&bytes);
    std::fs::write(&wal, &bytes[first_len..]).unwrap();

    assert!(matches!(Db::open(&path), Err(Error::Corrupt(_))));
    let report = check(&path).unwrap();
    assert!(!report.is_ok());
    assert!(report.errors.iter().any(|error| error.contains("gap")));
}

#[test]
fn all_durability_modes_roundtrip() {
    for durability in [Durability::Safe, Durability::Balanced, Durability::Fast] {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("test.esql");
        let opts = DbOptions {
            durability,
            ..DbOptions::default()
        };
        let ids: Vec<String>;
        {
            let db = Db::create_with(&path, opts.clone()).unwrap();
            db.create_table(schema()).unwrap();
            ids = (0..10)
                .map(|i| db.insert("docs", record(&format!("d{i}"), i)).unwrap())
                .collect();
        }
        let db = Db::open_with(&path, opts).unwrap();
        for id in &ids {
            assert!(db.get("docs", id).unwrap().is_some(), "{durability:?}");
        }
    }
}

#[test]
fn check_reports_clean_and_corrupt_states() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("test.esql");
    {
        let db = Db::create(&path).unwrap();
        db.create_table(schema()).unwrap();
        for i in 0..10 {
            db.insert("docs", record(&format!("d{i}"), i)).unwrap();
        }
        db.checkpoint().unwrap();
        db.insert("docs", record("tail", 99)).unwrap();
    }
    let report = check(&path).unwrap();
    assert!(report.is_ok(), "clean db: {:?}", report.errors);

    // Corrupt a segment byte: check must flag it.
    let seg_dir = path.join("segments");
    let seg = std::fs::read_dir(&seg_dir)
        .unwrap()
        .flatten()
        .map(|d| d.path())
        .find(|p| p.extension().is_some_and(|e| e == "seg"))
        .unwrap();
    let mut bytes = std::fs::read(&seg).unwrap();
    let mid = bytes.len() / 2;
    bytes[mid] ^= 0xFF;
    std::fs::write(&seg, &bytes).unwrap();

    let report = check(&path).unwrap();
    assert!(!report.is_ok(), "corrupt segment must be reported");
}

#[test]
fn version_watermark_continues_across_checkpoints() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("test.esql");
    {
        let db = Db::create(&path).unwrap();
        db.create_table(schema()).unwrap();
        db.insert("docs", record("one", 1)).unwrap();
        db.checkpoint().unwrap();
        db.insert("docs", record("two", 2)).unwrap();
    }
    {
        let db = Db::open(&path).unwrap();
        let snap_before = db.snapshot();
        db.insert("docs", record("three", 3)).unwrap();
        assert_eq!(db.scan("docs").unwrap().len(), 3);
        assert_eq!(
            db.scan_at(&snap_before, "docs").unwrap().len(),
            2,
            "snapshot from before the insert sees exactly the reopened state"
        );
    }
}

#[test]
fn incomplete_creation_never_removes_unknown_files() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("partial.esql");
    std::fs::create_dir(&path).unwrap();
    std::fs::write(
        path.join(".ELITESQL.creating"),
        b"elitesql creation journal v1\n",
    )
    .unwrap();
    std::fs::write(path.join("keep-me.txt"), b"important").unwrap();

    assert!(matches!(Db::create(&path), Err(Error::InvalidArgument(_))));
    assert_eq!(
        std::fs::read(path.join("keep-me.txt")).unwrap(),
        b"important"
    );
}

#[test]
fn incomplete_creation_can_clean_only_known_artifacts() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("partial.esql");
    std::fs::create_dir(&path).unwrap();
    std::fs::write(
        path.join(".ELITESQL.creating"),
        b"elitesql creation journal v1\n",
    )
    .unwrap();
    std::fs::create_dir(path.join("segments")).unwrap();
    std::fs::write(path.join("segments").join("partial.seg"), b"partial").unwrap();
    std::fs::write(path.join("catalog.json.tmp"), b"partial").unwrap();

    let db = Db::create(&path).unwrap();
    assert!(db.scan("missing").is_err());
    assert!(!path.join("segments").join("partial.seg").exists());
    assert!(path.join("ELITESQL").exists());
}
