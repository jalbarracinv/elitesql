use elitesql_core::{
    Column, ColumnType, Db, Error, IndexingMode, Record, TableSchema, Value, VectorIndexOptions,
    VectorMetric, VectorSearchOptions,
};
use tempfile::TempDir;

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
    fn unit_f32(&mut self) -> f32 {
        (self.next() % 10_000) as f32 / 10_000.0 - 0.5
    }
    fn vec(&mut self, dim: usize) -> Vec<f32> {
        (0..dim).map(|_| self.unit_f32()).collect()
    }
}

fn cosine_distance(a: &[f32], b: &[f32]) -> f32 {
    let dot: f32 = a.iter().zip(b).map(|(x, y)| x * y).sum();
    let na: f32 = a.iter().map(|x| x * x).sum::<f32>().sqrt();
    let nb: f32 = b.iter().map(|x| x * x).sum::<f32>().sqrt();
    1.0 - dot / (na * nb).max(f32::EPSILON)
}

fn docs_schema(dim: usize) -> TableSchema {
    TableSchema::new(
        "docs",
        vec![
            Column::new("title", ColumnType::Text).not_null(),
            Column::new("workspace", ColumnType::Text),
            Column::vector("embedding", dim),
        ],
    )
}

fn new_db(dim: usize) -> (TempDir, Db) {
    let dir = tempfile::tempdir().unwrap();
    let db = Db::create(dir.path().join("vec.esql")).unwrap();
    db.create_table(docs_schema(dim)).unwrap();
    (dir, db)
}

fn doc(title: &str, workspace: &str, embedding: Vec<f32>) -> Record {
    let mut r = Record::new();
    r.insert("title".into(), Value::Text(title.into()));
    r.insert("workspace".into(), Value::Text(workspace.into()));
    r.insert("embedding".into(), Value::Vector(embedding));
    r
}

#[test]
fn vector_roundtrip_and_dimension_validation() {
    let (_d, db) = new_db(4);
    let id = db.insert("docs", doc("a", "w1", vec![0.1, 0.2, 0.3, 0.4])).unwrap();
    let rec = db.get("docs", &id).unwrap().unwrap();
    assert_eq!(rec["embedding"], Value::Vector(vec![0.1, 0.2, 0.3, 0.4]));

    // Wrong dimension is rejected at the schema layer.
    let err = db.insert("docs", doc("bad", "w1", vec![0.1, 0.2])).unwrap_err();
    assert!(matches!(err, Error::SchemaViolation(_)), "{err}");

    // Vector columns reject non-vector index creation.
    assert!(matches!(
        db.create_index("docs", "embedding", false),
        Err(Error::SchemaViolation(_))
    ));
}

#[test]
fn knn_recall_vs_brute_force() {
    let dim = 32;
    let n = 2000;
    let (_d, db) = new_db(dim);
    db.create_vector_index("docs", "embedding", VectorIndexOptions::default()).unwrap();

    let mut rng = XorShift(42);
    let mut vectors = Vec::with_capacity(n);
    let mut txn = db.begin();
    for i in 0..n {
        let v = rng.vec(dim);
        let mut r = doc(&format!("doc {i}"), "w", v.clone());
        r.insert("id".into(), Value::Text(format!("d-{i:05}")));
        txn.insert("docs", r).unwrap();
        vectors.push((format!("d-{i:05}"), v));
    }
    txn.commit().unwrap();

    let k = 10;
    let queries = 50;
    let mut hits_total = 0usize;
    for _ in 0..queries {
        let q = rng.vec(dim);
        // Brute-force ground truth.
        let mut truth: Vec<(String, f32)> = vectors
            .iter()
            .map(|(id, v)| (id.clone(), cosine_distance(&q, v)))
            .collect();
        truth.sort_by(|a, b| a.1.total_cmp(&b.1));
        let truth_ids: std::collections::HashSet<&str> =
            truth[..k].iter().map(|(id, _)| id.as_str()).collect();

        let opts = VectorSearchOptions { ef_search: Some(128), ..Default::default() };
        let found = db.search_vector("docs", "embedding", &q, k, &opts).unwrap();
        assert_eq!(found.len(), k);
        hits_total += found.iter().filter(|h| truth_ids.contains(h.id.as_str())).count();
        // Results come back closest-first.
        for w in found.windows(2) {
            assert!(w[0].distance <= w[1].distance + 1e-6);
        }
    }
    let recall = hits_total as f64 / (queries * k) as f64;
    assert!(recall >= 0.9, "recall@{k} too low: {recall:.3}");
}

#[test]
fn metadata_filter_restricts_results() {
    let dim = 8;
    let (_d, db) = new_db(dim);
    db.create_vector_index("docs", "embedding", VectorIndexOptions::default()).unwrap();

    let mut rng = XorShift(7);
    for i in 0..200 {
        let ws = if i % 2 == 0 { "alpha" } else { "beta" };
        db.insert("docs", doc(&format!("d{i}"), ws, rng.vec(dim))).unwrap();
    }
    let q = rng.vec(dim);
    let mut filter = Record::new();
    filter.insert("workspace".into(), Value::Text("alpha".into()));
    let opts = VectorSearchOptions { filter: Some(filter), ..Default::default() };
    let hits = db.search_vector("docs", "embedding", &q, 20, &opts).unwrap();
    assert_eq!(hits.len(), 20);
    for h in &hits {
        assert_eq!(h.record["workspace"], Value::Text("alpha".into()));
    }
}

#[test]
fn updates_and_deletes_are_reflected() {
    let dim = 4;
    let (_d, db) = new_db(dim);
    db.create_vector_index("docs", "embedding", VectorIndexOptions::default()).unwrap();

    // Two docs: one right on the query, one far away.
    let near = db.insert("docs", doc("near", "w", vec![1.0, 0.0, 0.0, 0.0])).unwrap();
    let far = db.insert("docs", doc("far", "w", vec![-1.0, 0.0, 0.0, 0.0])).unwrap();

    let q = [1.0, 0.0, 0.0, 0.0];
    let opts = VectorSearchOptions::default();
    let hits = db.search_vector("docs", "embedding", &q, 1, &opts).unwrap();
    assert_eq!(hits[0].id, near);

    // Update: move "far" onto the query; it should win (or tie) now.
    let mut patch = Record::new();
    patch.insert("embedding".into(), Value::Vector(vec![1.0, 0.001, 0.0, 0.0]));
    db.update("docs", &far, patch).unwrap();
    let hits = db.search_vector("docs", "embedding", &q, 2, &opts).unwrap();
    assert_eq!(hits.len(), 2, "both docs are close now");

    // Delete "near": it must never appear again.
    db.delete("docs", &near).unwrap();
    let hits = db.search_vector("docs", "embedding", &q, 5, &opts).unwrap();
    assert!(hits.iter().all(|h| h.id != near), "deleted doc leaked into results");
    assert_eq!(hits.len(), 1);

    // Setting the vector to NULL removes it from the index.
    let mut clear = Record::new();
    clear.insert("embedding".into(), Value::Null);
    db.update("docs", &far, clear).unwrap();
    let hits = db.search_vector("docs", "embedding", &q, 5, &opts).unwrap();
    assert!(hits.is_empty());
}

#[test]
fn reopen_rebuilds_index_from_canonical_data() {
    let dim = 16;
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("vec.esql");
    let mut rng = XorShift(99);
    let q = rng.vec(dim);
    let expected: Vec<String>;
    {
        let db = Db::create(&path).unwrap();
        db.create_table(docs_schema(dim)).unwrap();
        db.create_vector_index("docs", "embedding", VectorIndexOptions::default()).unwrap();
        for i in 0..300 {
            db.insert("docs", doc(&format!("d{i}"), "w", rng.vec(dim))).unwrap();
        }
        let opts = VectorSearchOptions { ef_search: Some(200), ..Default::default() };
        expected = db
            .search_vector("docs", "embedding", &q, 5, &opts)
            .unwrap()
            .into_iter()
            .map(|h| h.id)
            .collect();
    }
    let db = Db::open(&path).unwrap();
    let opts = VectorSearchOptions { ef_search: Some(200), ..Default::default() };
    let got: Vec<String> = db
        .search_vector("docs", "embedding", &q, 5, &opts)
        .unwrap()
        .into_iter()
        .map(|h| h.id)
        .collect();
    assert_eq!(expected, got, "rebuilt index returns the same neighbours");
}

#[test]
fn compaction_rebuilds_dropping_tombstones() {
    let dim = 8;
    let (_d, db) = new_db(dim);
    db.create_vector_index("docs", "embedding", VectorIndexOptions::default()).unwrap();

    let mut rng = XorShift(1234);
    let mut ids = Vec::new();
    for i in 0..100 {
        ids.push(db.insert("docs", doc(&format!("d{i}"), "w", rng.vec(dim))).unwrap());
    }
    // Churn: update half, delete a quarter.
    for id in ids.iter().take(50) {
        let mut p = Record::new();
        p.insert("embedding".into(), Value::Vector(rng.vec(dim)));
        db.update("docs", id, p).unwrap();
    }
    for id in ids.iter().skip(50).take(25) {
        db.delete("docs", id).unwrap();
    }
    db.compact().unwrap();

    // ANN is approximate: asking for every point is not guaranteed to return
    // all of them. What compaction must guarantee: deleted docs are gone,
    // and live docs remain searchable with high recall.
    let q = rng.vec(dim);
    let opts = VectorSearchOptions { ef_search: Some(400), ..Default::default() };
    let hits = db.search_vector("docs", "embedding", &q, 75, &opts).unwrap();
    let deleted: std::collections::HashSet<&String> = ids.iter().skip(50).take(25).collect();
    assert!(
        hits.iter().all(|h| !deleted.contains(&h.id)),
        "a deleted doc survived compaction in the ANN index"
    );
    assert!(hits.len() >= 70, "recall too low after compaction: {}/75", hits.len());

    // Spot check: live docs find themselves by their own vector.
    for id in ids.iter().take(5) {
        let rec = db.get("docs", id).unwrap().unwrap();
        let Value::Vector(v) = &rec["embedding"] else { panic!("vector expected") };
        let selfhits = db.search_vector("docs", "embedding", v, 3, &opts).unwrap();
        assert!(selfhits.iter().any(|h| &h.id == id), "doc {id} lost after compaction");
    }
}

#[test]
fn async_mode_indexes_in_background() {
    let dim = 8;
    let (_d, db) = new_db(dim);
    db.create_vector_index(
        "docs",
        "embedding",
        VectorIndexOptions {
            mode: IndexingMode::Async,
            ..Default::default()
        },
    )
    .unwrap();

    let mut rng = XorShift(5);
    for i in 0..100 {
        db.insert("docs", doc(&format!("d{i}"), "w", rng.vec(dim))).unwrap();
    }
    // Commits return before indexing; wait for the background thread.
    db.wait_vector_indexing();
    assert_eq!(db.vector_indexing_backlog(), 0);
    let hits = db
        .search_vector("docs", "embedding", &rng.vec(dim), 10, &VectorSearchOptions::default())
        .unwrap();
    assert_eq!(hits.len(), 10);
}

#[test]
fn dot_and_l2_metrics_work() {
    for metric in [VectorMetric::Dot, VectorMetric::L2] {
        let dim = 8;
        let (_d, db) = new_db(dim);
        db.create_vector_index(
            "docs",
            "embedding",
            VectorIndexOptions { metric, ..Default::default() },
        )
        .unwrap();
        let mut rng = XorShift(77);
        for i in 0..100 {
            db.insert("docs", doc(&format!("d{i}"), "w", rng.vec(dim))).unwrap();
        }
        let hits = db
            .search_vector("docs", "embedding", &rng.vec(dim), 5, &VectorSearchOptions::default())
            .unwrap();
        assert_eq!(hits.len(), 5, "{metric:?}");
        for w in hits.windows(2) {
            assert!(w[0].distance <= w[1].distance + 1e-6, "{metric:?} ordering");
        }
    }
}

#[test]
fn search_errors_are_clear() {
    let (_d, db) = new_db(4);
    let err = db
        .search_vector("docs", "embedding", &[0.0; 4], 5, &VectorSearchOptions::default())
        .unwrap_err();
    assert!(err.to_string().contains("create_vector_index"), "{err}");

    db.create_vector_index("docs", "embedding", VectorIndexOptions::default()).unwrap();
    let err = db
        .search_vector("docs", "embedding", &[0.0; 3], 5, &VectorSearchOptions::default())
        .unwrap_err();
    assert!(matches!(err, Error::InvalidArgument(_)), "dimension mismatch: {err}");

    let err = db
        .search_vector("docs", "title", &[0.0; 4], 5, &VectorSearchOptions::default())
        .unwrap_err();
    assert!(matches!(err, Error::SchemaViolation(_)), "non-vector column: {err}");

    assert!(matches!(
        db.create_vector_index("docs", "embedding", VectorIndexOptions::default()),
        Err(Error::InvalidArgument(_))
    ));
}

#[test]
fn sql_creates_and_fills_vector_columns() {
    let (_d, db) = new_db(4);
    db.query("CREATE TABLE items (label text, emb vector(3))").unwrap();
    db.query("INSERT INTO items (label, emb) VALUES ('a', '[1.0, 0.0, 0.0]')").unwrap();
    db.create_vector_index("items", "emb", VectorIndexOptions::default()).unwrap();
    let hits = db
        .search_vector("items", "emb", &[1.0, 0.0, 0.0], 1, &VectorSearchOptions::default())
        .unwrap();
    assert_eq!(hits.len(), 1);
    assert_eq!(hits[0].record["label"], Value::Text("a".into()));

    // Dimension mismatch through SQL is also rejected.
    let err = db
        .query("INSERT INTO items (emb) VALUES ('[1.0, 2.0]')")
        .unwrap_err();
    assert!(matches!(err, Error::SchemaViolation(_)), "{err}");
    // vector requires a dimension.
    let err = db.query("CREATE TABLE bad (v vector)").unwrap_err();
    assert!(err.to_string().contains("vector(N)"), "{err}");
}
