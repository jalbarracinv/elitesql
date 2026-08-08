//! Phase 5 features + Phase 4 closure: full-text BM25, hybrid RRF search,
//! quantized vectors, out-of-line blob chunking and read-only mode.

use elitesql_core::{
    check, Column, ColumnType, Db, DbOptions, Error, HybridQuery, Record, TableSchema, Value,
    VectorIndexOptions, VectorSearchOptions,
};
use tempfile::TempDir;

fn doc(body: &str, ws: &str) -> Record {
    let mut r = Record::new();
    r.insert("body".into(), Value::Text(body.into()));
    r.insert("ws".into(), Value::Text(ws.into()));
    r
}

fn text_db() -> (TempDir, Db) {
    let dir = tempfile::tempdir().unwrap();
    let db = Db::create(dir.path().join("t.esql")).unwrap();
    db.create_table(TableSchema::new(
        "docs",
        vec![
            Column::new("body", ColumnType::Text).not_null(),
            Column::new("ws", ColumnType::Text),
            Column::vector("emb", 4),
        ],
    ))
    .unwrap();
    db.create_text_index("docs", "body").unwrap();
    (dir, db)
}

// --- full-text ------------------------------------------------------------------

#[test]
fn bm25_ranks_relevance_sensibly() {
    let (_d, db) = text_db();
    let heavy = db
        .insert("docs", doc("rust rust rust database engine", "a"))
        .unwrap();
    let light = db
        .insert(
            "docs",
            doc("a rust mention among many other words here", "a"),
        )
        .unwrap();
    db.insert("docs", doc("python snake tutorial", "a"))
        .unwrap();
    for i in 0..20 {
        db.insert("docs", doc(&format!("filler document number {i}"), "a"))
            .unwrap();
    }

    let hits = db.search_text("docs", "body", "rust", 10, None).unwrap();
    assert_eq!(hits.len(), 2, "only docs containing the term match");
    assert_eq!(hits[0].id, heavy, "higher term frequency ranks first");
    assert_eq!(hits[1].id, light);
    assert!(hits[0].score > hits[1].score);

    // Multi-term: rarer terms weigh more (idf).
    let rare = db.insert("docs", doc("zanahoria database", "a")).unwrap();
    let hits = db
        .search_text("docs", "body", "zanahoria database", 5, None)
        .unwrap();
    assert_eq!(hits[0].id, rare, "doc matching the rare term wins");

    // Case-insensitive, punctuation-tolerant.
    let hits = db.search_text("docs", "body", "RUST!!!", 5, None).unwrap();
    assert_eq!(hits.len(), 2);

    // No matches / empty query.
    assert!(db
        .search_text("docs", "body", "inexistente", 5, None)
        .unwrap()
        .is_empty());
    assert!(db
        .search_text("docs", "body", "", 5, None)
        .unwrap()
        .is_empty());
}

#[test]
fn text_index_tracks_updates_deletes_reopen_compaction() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("t.esql");
    let (a, b);
    {
        let db = Db::create(&path).unwrap();
        db.create_table(TableSchema::new(
            "docs",
            vec![
                Column::new("body", ColumnType::Text).not_null(),
                Column::new("ws", ColumnType::Text),
            ],
        ))
        .unwrap();
        db.create_text_index("docs", "body").unwrap();
        a = db.insert("docs", doc("gato negro", "a")).unwrap();
        b = db.insert("docs", doc("perro blanco", "a")).unwrap();

        // Update: old terms leave the index, new ones enter.
        let mut patch = Record::new();
        patch.insert("body".into(), Value::Text("loro verde".into()));
        db.update("docs", &a, patch).unwrap();
        assert!(db
            .search_text("docs", "body", "gato", 5, None)
            .unwrap()
            .is_empty());
        assert_eq!(
            db.search_text("docs", "body", "loro", 5, None).unwrap()[0].id,
            a
        );

        // Delete: gone from results.
        db.delete("docs", &b).unwrap();
        assert!(db
            .search_text("docs", "body", "perro", 5, None)
            .unwrap()
            .is_empty());
        db.compact().unwrap();
        assert_eq!(
            db.search_text("docs", "body", "loro", 5, None)
                .unwrap()
                .len(),
            1
        );
    }
    // Reopen rebuilds the inverted index from canonical data.
    let db = Db::open(&path).unwrap();
    assert_eq!(
        db.search_text("docs", "body", "loro verde", 5, None)
            .unwrap()[0]
            .id,
        a
    );
    assert!(db
        .search_text("docs", "body", "gato", 5, None)
        .unwrap()
        .is_empty());

    // The reopened index is an mmap-backed base. Updates must hide its old
    // postings and publish the new terms through the mutable delta.
    let mut patch = Record::new();
    patch.insert("body".into(), Value::Text("buho nocturno".into()));
    db.update("docs", &a, patch).unwrap();
    assert!(db
        .search_text("docs", "body", "loro", 5, None)
        .unwrap()
        .is_empty());
    assert_eq!(
        db.search_text("docs", "body", "buho", 5, None).unwrap()[0].id,
        a
    );
}

#[test]
fn text_filter_and_errors() {
    let (_d, db) = text_db();
    db.insert("docs", doc("informe mensual", "alpha")).unwrap();
    db.insert("docs", doc("informe anual", "beta")).unwrap();

    let mut filter = Record::new();
    filter.insert("ws".into(), Value::Text("beta".into()));
    let hits = db
        .search_text("docs", "body", "informe", 10, Some(&filter))
        .unwrap();
    assert_eq!(hits.len(), 1);
    assert_eq!(hits[0].record["ws"], Value::Text("beta".into()));

    let err = db.search_text("docs", "ws", "x", 5, None).unwrap_err();
    assert!(err.to_string().contains("create_text_index"), "{err}");
    assert!(matches!(
        db.create_text_index("docs", "emb"),
        Err(Error::SchemaViolation(_))
    ));
    assert!(matches!(
        db.create_text_index("docs", "body"),
        Err(Error::InvalidArgument(_))
    ));
}

// --- hybrid ---------------------------------------------------------------------

#[test]
fn hybrid_rrf_fuses_both_modalities() {
    let (_d, db) = text_db();
    db.create_vector_index("docs", "emb", VectorIndexOptions::default())
        .unwrap();

    let mk = |body: &str, emb: [f32; 4]| {
        let mut r = doc(body, "a");
        r.insert("emb".into(), Value::Vector(emb.to_vec()));
        r
    };
    // both: matches text AND is near the query vector.
    let both = db
        .insert("docs", mk("motor de base vectorial", [1.0, 0.0, 0.0, 0.0]))
        .unwrap();
    // text_only: strong text match, far vector.
    let text_only = db
        .insert(
            "docs",
            mk("base vectorial de conocimiento", [0.0, 0.0, 0.0, 1.0]),
        )
        .unwrap();
    // vec_only: near vector, unrelated text.
    let vec_only = db
        .insert("docs", mk("receta de cocina", [0.9, 0.1, 0.0, 0.0]))
        .unwrap();
    for i in 0..10 {
        db.insert("docs", mk(&format!("relleno {i}"), [0.0, 1.0, 0.0, 0.0]))
            .unwrap();
    }

    let q = HybridQuery {
        text: Some(("body", "base vectorial")),
        vector: Some(("emb", &[1.0, 0.0, 0.0, 0.0])),
        top_k: 3,
        ..Default::default()
    };
    let hits = db.search_hybrid("docs", &q).unwrap();
    assert_eq!(hits[0].id, both, "present in both rankings wins RRF");
    let ids: Vec<&str> = hits.iter().map(|h| h.id.as_str()).collect();
    assert!(ids.contains(&text_only.as_str()));
    assert!(ids.contains(&vec_only.as_str()));

    // Degenerate cases: single modality works; none is an error.
    let only_text = HybridQuery {
        text: Some(("body", "cocina")),
        top_k: 2,
        ..Default::default()
    };
    assert_eq!(
        db.search_hybrid("docs", &only_text).unwrap()[0].id,
        vec_only
    );
    let none = HybridQuery {
        top_k: 3,
        ..Default::default()
    };
    assert!(matches!(
        db.search_hybrid("docs", &none),
        Err(Error::InvalidArgument(_))
    ));
}

// --- quantized vectors ------------------------------------------------------------

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

fn cosine_distance(a: &[f32], b: &[f32]) -> f32 {
    let dot: f32 = a.iter().zip(b).map(|(x, y)| x * y).sum();
    let na: f32 = a.iter().map(|x| x * x).sum::<f32>().sqrt();
    let nb: f32 = b.iter().map(|x| x * x).sum::<f32>().sqrt();
    1.0 - dot / (na * nb).max(f32::EPSILON)
}

#[test]
fn quantized_index_keeps_recall_and_shrinks_dump() {
    let dim = 32;
    let n = 1500;
    let build = |path: &std::path::Path, quantized: bool| -> (Db, Vec<(String, Vec<f32>)>) {
        let db = Db::create(path).unwrap();
        db.create_table(TableSchema::new("v", vec![Column::vector("emb", dim)]))
            .unwrap();
        db.create_vector_index(
            "v",
            "emb",
            VectorIndexOptions {
                quantized,
                ..Default::default()
            },
        )
        .unwrap();
        let mut rng = XorShift(99);
        let mut data = Vec::new();
        let mut txn = db.begin();
        for i in 0..n {
            let v = rng.vec(dim);
            let mut r = Record::new();
            r.insert("id".into(), Value::Text(format!("q-{i:05}")));
            r.insert("emb".into(), Value::Vector(v.clone()));
            txn.insert("v", r).unwrap();
            data.push((format!("q-{i:05}"), v));
        }
        txn.commit().unwrap();
        (db, data)
    };

    let dir = tempfile::tempdir().unwrap();
    let (dbq, data) = build(&dir.path().join("quant.esql"), true);

    // Recall vs brute force stays high after int8 quantization.
    let mut rng = XorShift(1234);
    let k = 10;
    let mut hit = 0usize;
    for _ in 0..30 {
        let q = rng.vec(dim);
        let mut truth: Vec<(&str, f32)> = data
            .iter()
            .map(|(id, v)| (id.as_str(), cosine_distance(&q, v)))
            .collect();
        truth.sort_by(|a, b| a.1.total_cmp(&b.1));
        let truth_ids: std::collections::HashSet<&str> =
            truth[..k].iter().map(|(id, _)| *id).collect();
        let opts = VectorSearchOptions {
            ef_search: Some(128),
            ..Default::default()
        };
        let found = dbq.search_vector("v", "emb", &q, k, &opts).unwrap();
        hit += found
            .iter()
            .filter(|h| truth_ids.contains(h.id.as_str()))
            .count();
    }
    let recall = hit as f64 / (30 * k) as f64;
    assert!(recall >= 0.8, "quantized recall too low: {recall:.3}");

    // The persisted graph shrinks roughly 4x on the vector payload.
    drop(dbq);
    let (dbf, _) = build(&dir.path().join("full.esql"), false);
    drop(dbf);
    let size_of = |p: &std::path::Path| -> u64 {
        std::fs::read_dir(p.join("vectors"))
            .unwrap()
            .flatten()
            .map(|e| e.metadata().map(|m| m.len()).unwrap_or(0))
            .sum()
    };
    let quant = size_of(&dir.path().join("quant.esql"));
    let full = size_of(&dir.path().join("full.esql"));
    // The vector payload shrinks 4x (f32 -> i8 + scale); graph links are the
    // same in both dumps, so assert on the absolute savings: at least
    // ~2 bytes per component of the theoretical 3.
    let min_savings = (n * dim * 2) as u64;
    assert!(
        quant + min_savings < full,
        "quantized dump should save >= {min_savings} bytes: {quant} vs {full}"
    );

    // Reload from the quantized dump answers consistently.
    let db = Db::open(dir.path().join("quant.esql")).unwrap();
    let hits = db
        .search_vector("v", "emb", &data[0].1, 3, &VectorSearchOptions::default())
        .unwrap();
    assert_eq!(
        hits[0].id, data[0].0,
        "self-query finds itself after reload"
    );
}

// --- blob chunking ------------------------------------------------------------------

fn blob_db(threshold: usize) -> (TempDir, Db) {
    let dir = tempfile::tempdir().unwrap();
    let opts = DbOptions {
        external_blob_threshold: threshold,
        ..DbOptions::default()
    };
    let db = Db::create_with(dir.path().join("b.esql"), opts).unwrap();
    db.create_table(TableSchema::new(
        "files",
        vec![
            Column::new("name", ColumnType::Text).not_null(),
            Column::new("data", ColumnType::Blob),
        ],
    ))
    .unwrap();
    (dir, db)
}

fn blob_files(dir: &TempDir) -> usize {
    std::fs::read_dir(dir.path().join("b.esql/blobs"))
        .map(|d| d.flatten().count())
        .unwrap_or(0)
}

#[test]
fn big_blobs_go_out_of_line_and_roundtrip() {
    let (dir, db) = blob_db(64);
    let big: Vec<u8> = (0..10_000u32).map(|i| (i % 251) as u8).collect();
    let small = vec![7u8; 10];

    let mut r = Record::new();
    r.insert("name".into(), Value::Text("big".into()));
    r.insert("data".into(), Value::Blob(big.clone()));
    let big_id = db.insert("files", r).unwrap();

    let mut r = Record::new();
    r.insert("name".into(), Value::Text("small".into()));
    r.insert("data".into(), Value::Blob(small.clone()));
    let small_id = db.insert("files", r).unwrap();

    assert_eq!(blob_files(&dir), 1, "only the big blob is externalized");
    assert_eq!(
        db.get("files", &big_id).unwrap().unwrap()["data"],
        Value::Blob(big.clone()),
        "byte-exact roundtrip through the chunk file"
    );
    assert_eq!(
        db.get("files", &small_id).unwrap().unwrap()["data"],
        Value::Blob(small)
    );

    // Survives checkpoint + reopen (reference travels through segments).
    db.checkpoint().unwrap();
    drop(db);
    let db = Db::open_with(
        dir.path().join("b.esql"),
        DbOptions {
            external_blob_threshold: 64,
            ..DbOptions::default()
        },
    )
    .unwrap();
    assert_eq!(
        db.get("files", &big_id).unwrap().unwrap()["data"],
        Value::Blob(big)
    );
    let report = check(dir.path().join("b.esql")).unwrap();
    assert!(report.is_ok(), "{:?}", report.errors);
}

#[test]
fn blob_gc_on_compaction_and_corruption_detection() {
    let (dir, db) = blob_db(64);
    let payload =
        |seed: u8| -> Vec<u8> { (0..5000).map(|i| (i as u8).wrapping_mul(seed)).collect() };

    let mut ids = Vec::new();
    for i in 0..4u8 {
        let mut r = Record::new();
        r.insert("name".into(), Value::Text(format!("f{i}")));
        r.insert("data".into(), Value::Blob(payload(i + 1)));
        ids.push(db.insert("files", r).unwrap());
    }
    assert_eq!(blob_files(&dir), 4);

    // Update one (new chunk written) and delete another.
    let mut patch = Record::new();
    patch.insert("data".into(), Value::Blob(payload(99)));
    db.update("files", &ids[0], patch).unwrap();
    db.delete("files", &ids[1]).unwrap();
    assert_eq!(blob_files(&dir), 5, "old chunks linger until compaction");

    db.compact().unwrap();
    assert_eq!(blob_files(&dir), 3, "GC kept exactly the live chunks");
    assert_eq!(
        db.get("files", &ids[0]).unwrap().unwrap()["data"],
        Value::Blob(payload(99))
    );

    // Corrupt a chunk: reads fail loudly, check() flags it, nothing panics.
    let chunk = std::fs::read_dir(dir.path().join("b.esql/blobs"))
        .unwrap()
        .flatten()
        .map(|e| e.path())
        .next()
        .unwrap();
    let mut bytes = std::fs::read(&chunk).unwrap();
    let last = bytes.len() - 1;
    bytes[last] ^= 0xFF;
    std::fs::write(&chunk, &bytes).unwrap();

    let mut corrupt_seen = false;
    for id in &ids {
        match db.get("files", id) {
            Ok(_) => {}
            Err(Error::Corrupt(msg)) => {
                corrupt_seen = true;
                assert!(msg.contains("blob chunk"), "{msg}");
            }
            Err(other) => panic!("unexpected error {other}"),
        }
    }
    assert!(corrupt_seen, "the damaged chunk must be reported on read");
    drop(db);
    let report = check(dir.path().join("b.esql")).unwrap();
    assert!(!report.is_ok(), "check must flag the corrupt chunk");
}

// --- read-only mode -----------------------------------------------------------------

#[test]
fn read_only_reads_and_rejects_writes() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("ro.esql");
    {
        let db = Db::create(&path).unwrap();
        db.query("CREATE TABLE t (n int64 NOT NULL)").unwrap();
        db.query("INSERT INTO t (id, n) VALUES ('a', 1), ('b', 2)")
            .unwrap();
    }
    // Snapshot the on-disk bytes: read-only must not touch anything.
    let before = dir_bytes(&path);

    let db = Db::open_read_only(&path).unwrap();
    assert_eq!(db.scan("t").unwrap().len(), 2);
    let rows = db.query("SELECT n FROM t ORDER BY n").unwrap();
    assert!(matches!(rows, elitesql_core::QueryOutput::Rows { .. }));

    // Every write path is rejected with ReadOnly (code 13).
    let mut rec = Record::new();
    rec.insert("n".into(), Value::Int64(3));
    for err in [
        db.insert("t", rec).unwrap_err(),
        db.query("INSERT INTO t (n) VALUES (3)").unwrap_err(),
        db.query("CREATE TABLE nope (x int64)").unwrap_err(),
        db.checkpoint().unwrap_err(),
        db.compact().unwrap_err(),
        db.create_index("t", "n", false).unwrap_err(),
    ] {
        assert!(matches!(err, Error::ReadOnly), "{err}");
        assert_eq!(err.code(), 13);
    }

    // Two read-only handles coexist; a writer is locked out meanwhile.
    let db2 = Db::open_read_only(&path).unwrap();
    assert_eq!(db2.scan("t").unwrap().len(), 2);
    assert!(matches!(Db::open(&path), Err(Error::DatabaseLocked(_))));
    drop(db2);
    drop(db);

    assert_eq!(
        before,
        dir_bytes(&path),
        "read-only left every byte untouched"
    );
    // And a writer works again after the readers are gone.
    let db = Db::open(&path).unwrap();
    db.query("INSERT INTO t (n) VALUES (3)").unwrap();
}

#[test]
fn read_only_opens_a_corrupt_database_best_effort() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("ro.esql");
    {
        let db = Db::create(&path).unwrap();
        db.query("CREATE TABLE t (n int64 NOT NULL)").unwrap();
        for i in 0..30 {
            db.query(&format!("INSERT INTO t (id, n) VALUES ('r-{i:03}', {i})"))
                .unwrap();
        }
        db.checkpoint().unwrap();
    }
    // Corrupt the middle of the segment.
    let seg = std::fs::read_dir(path.join("segments"))
        .unwrap()
        .flatten()
        .map(|e| e.path())
        .find(|p| p.extension().is_some_and(|e| e == "seg"))
        .unwrap();
    let mut bytes = std::fs::read(&seg).unwrap();
    let mid = bytes.len() / 2;
    bytes[mid] ^= 0xFF;
    std::fs::write(&seg, &bytes).unwrap();

    assert!(Db::open(&path).is_err(), "normal open refuses");
    let db = Db::open_read_only(&path).unwrap();
    let visible = db.scan("t").unwrap().len();
    assert!(
        visible > 5 && visible < 30,
        "valid prefix visible: {visible}"
    );
    assert!(matches!(db.checkpoint().unwrap_err(), Error::ReadOnly));
}

fn dir_bytes(path: &std::path::Path) -> Vec<(String, Vec<u8>)> {
    fn walk(p: &std::path::Path, out: &mut Vec<(String, Vec<u8>)>) {
        for e in std::fs::read_dir(p).unwrap().flatten() {
            let path = e.path();
            if path.is_dir() {
                walk(&path, out);
            } else if path.file_name().is_some_and(|n| n != "LOCK") {
                out.push((path.display().to_string(), std::fs::read(&path).unwrap()));
            }
        }
    }
    let mut out = Vec::new();
    walk(path, &mut out);
    out.sort();
    out
}
