//! Persistence of the ANN graph in vectors/: fast load on open, incremental
//! catch-up when the dump is older than the committed state, and fallback to
//! a full rebuild when the dump is corrupt or its definition changed.

use std::path::{Path, PathBuf};

use clawdb_core::{
    Column, ColumnType, Db, Record, TableSchema, Value, VectorIndexOptions, VectorSearchOptions,
};

const DIM: usize = 16;

struct XorShift(u64);

impl XorShift {
    fn next(&mut self) -> u64 {
        let mut x = self.0;
        x ^= x << 13;
        x ^= x >> 7;
        x ^= x << 17;
        self.0 = x;
        x
    }
    fn vec(&mut self, dim: usize) -> Vec<f32> {
        (0..dim)
            .map(|_| (self.next() % 10_000) as f32 / 10_000.0 - 0.5)
            .collect()
    }
}

fn schema() -> TableSchema {
    TableSchema::new(
        "docs",
        vec![
            Column::new("title", ColumnType::Text).not_null(),
            Column::vector("embedding", DIM),
        ],
    )
}

fn doc(title: &str, embedding: Vec<f32>) -> Record {
    let mut r = Record::new();
    r.insert("title".into(), Value::Text(title.into()));
    r.insert("embedding".into(), Value::Vector(embedding));
    r
}

fn try_vidx_file(db_path: &Path) -> Option<PathBuf> {
    std::fs::read_dir(db_path.join("vectors"))
        .ok()?
        .flatten()
        .map(|e| e.path())
        .find(|p| p.extension().is_some_and(|e| e == "vidx"))
}

fn vidx_file(db_path: &Path) -> PathBuf {
    try_vidx_file(db_path).expect("vidx dump present")
}

fn search_ids(db: &Db, q: &[f32], k: usize) -> Vec<String> {
    let opts = VectorSearchOptions {
        ef_search: Some(200),
        ..Default::default()
    };
    db.search_vector("docs", "embedding", q, k, &opts)
        .unwrap()
        .into_iter()
        .map(|h| h.id)
        .collect()
}

#[test]
fn clean_close_dumps_and_open_loads_identically() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("p.clawdb");
    let mut rng = XorShift(11);
    let q = rng.vec(DIM);
    let expected: Vec<String>;
    {
        let db = Db::create(&path).unwrap();
        db.create_table(schema()).unwrap();
        db.create_vector_index("docs", "embedding", VectorIndexOptions::default()).unwrap();
        for i in 0..400 {
            db.insert("docs", doc(&format!("d{i}"), rng.vec(DIM))).unwrap();
        }
        expected = search_ids(&db, &q, 10);
    } // drop dumps the graph

    let dump = vidx_file(&path);
    assert!(std::fs::metadata(&dump).unwrap().len() > 0, "dump written on close");

    let db = Db::open(&path).unwrap();
    assert_eq!(search_ids(&db, &q, 10), expected, "loaded graph answers identically");
}

#[test]
fn stale_dump_catches_up_with_newer_commits() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("p.clawdb");
    let mut rng = XorShift(22);
    let near = vec![1.0; DIM];

    // Session 1: baseline docs, clean close -> dump at version V1.
    let mut ids = Vec::new();
    {
        let db = Db::create(&path).unwrap();
        db.create_table(schema()).unwrap();
        db.create_vector_index("docs", "embedding", VectorIndexOptions::default()).unwrap();
        for i in 0..100 {
            ids.push(db.insert("docs", doc(&format!("old {i}"), rng.vec(DIM))).unwrap());
        }
    }
    // Save the V1 dump aside.
    let dump_path = vidx_file(&path);
    let stale_dump = std::fs::read(&dump_path).unwrap();

    // Session 2: mutate heavily, clean close -> fresh dump at V2.
    let winner;
    {
        let db = Db::open(&path).unwrap();
        winner = db.insert("docs", doc("winner", near.clone())).unwrap();
        for id in ids.iter().take(30) {
            db.delete("docs", id).unwrap();
        }
        let mut patch = Record::new();
        patch.insert("embedding".into(), Value::Vector(near.iter().map(|x| x * 0.9).collect()));
        db.update("docs", &ids[40], patch).unwrap();
    }
    // Restore the STALE dump: simulates a crash after those commits (WAL
    // and segments are newer than the persisted graph).
    std::fs::write(&dump_path, &stale_dump).unwrap();

    let db = Db::open(&path).unwrap();
    let top = search_ids(&db, &near, 2);
    assert_eq!(top[0], winner, "record committed after the dump is searchable");
    assert_eq!(top[1], ids[40], "update committed after the dump is reflected");
    // Deletions after the dump never resurface.
    let all = search_ids(&db, &rng.vec(DIM), 71);
    let deleted: std::collections::HashSet<&String> = ids.iter().take(30).collect();
    assert!(all.iter().all(|id| !deleted.contains(id)), "deleted doc resurfaced from stale dump");
    // 100 - 30 deleted + 1 winner = 71 live docs.
    assert_eq!(db.scan("docs").unwrap().len(), 71);
}

#[test]
fn corrupt_dump_falls_back_to_rebuild() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("p.clawdb");
    let mut rng = XorShift(33);
    let q = rng.vec(DIM);
    let expected: Vec<String>;
    {
        let db = Db::create(&path).unwrap();
        db.create_table(schema()).unwrap();
        db.create_vector_index("docs", "embedding", VectorIndexOptions::default()).unwrap();
        for i in 0..200 {
            db.insert("docs", doc(&format!("d{i}"), rng.vec(DIM))).unwrap();
        }
        expected = search_ids(&db, &q, 10);
    }
    // Corrupt the dump body.
    let dump_path = vidx_file(&path);
    let mut bytes = std::fs::read(&dump_path).unwrap();
    let mid = bytes.len() / 2;
    bytes[mid] ^= 0xFF;
    std::fs::write(&dump_path, &bytes).unwrap();

    let db = Db::open(&path).unwrap();
    assert_eq!(search_ids(&db, &q, 10), expected, "rebuild produces correct results");
    // And truncated garbage doesn't panic either.
    drop(db);
    std::fs::write(&dump_path, b"CLAWVIDXgarbage").unwrap();
    let db = Db::open(&path).unwrap();
    assert_eq!(search_ids(&db, &q, 10), expected);
}

#[test]
fn compaction_refreshes_the_dump() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("p.clawdb");
    let mut rng = XorShift(44);
    {
        let db = Db::create(&path).unwrap();
        db.create_table(schema()).unwrap();
        db.create_vector_index("docs", "embedding", VectorIndexOptions::default()).unwrap();
        let mut ids = Vec::new();
        for i in 0..100 {
            ids.push(db.insert("docs", doc(&format!("d{i}"), rng.vec(DIM))).unwrap());
        }
        for id in ids.iter().take(50) {
            db.delete("docs", id).unwrap();
        }
        let before = try_vidx_file(&path)
            .and_then(|p| std::fs::metadata(p).ok())
            .map(|m| m.len())
            .unwrap_or(0);
        db.compact().unwrap(); // dumps the compacted (tombstone-free) graph
        let after = std::fs::metadata(vidx_file(&path)).unwrap().len();
        assert!(after > 0);
        if before > 0 {
            assert!(after < before, "compacted dump should shrink: {before} -> {after}");
        }
    }
    let db = Db::open(&path).unwrap();
    assert_eq!(db.scan("docs").unwrap().len(), 50);
    assert_eq!(search_ids(&db, &rng.vec(DIM), 50).len(), 50);
}
