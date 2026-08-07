//! Phase 3 acceptance benchmark: ANN search latency and recall@10 over a
//! 100K-vector dataset with brute-force ground truth computed in-process
//! (no external dataset download required).
//!
//! The dataset is clustered (centers + noise) with noise comparable to the
//! center scale, modelling real embedding distributions where neighbors are
//! distinguishable. Two degenerate regimes are deliberately avoided because
//! exact recall@k is meaningless there for ANY ANN structure: pure uniform
//! noise (distances concentrate) and ultra-tight clusters (the true top-k
//! among ~100 near-ties is decided by float dust).
//!
//! Run with: cargo bench -p clawdb-core --bench vector

use std::hint::black_box;

use clawdb_core::{
    Column, ColumnType, Db, DbOptions, Durability, Record, TableSchema, Value,
    VectorIndexOptions, VectorSearchOptions,
};
use criterion::{criterion_group, criterion_main, Criterion};
use tempfile::TempDir;

const N: usize = 100_000;
const DIM: usize = 64;
const K: usize = 10;
const CLUSTERS: usize = 1024;
const NOISE: f32 = 0.6;

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

/// Clustered generator: pick a center, add noise. Queries use the same
/// distribution, like real embedding lookups.
struct Clustered {
    centers: Vec<Vec<f32>>,
    rng: XorShift,
}

impl Clustered {
    fn new(seed: u64) -> Clustered {
        let mut rng = XorShift(seed);
        let centers = (0..CLUSTERS).map(|_| rng.vec(DIM)).collect();
        Clustered { centers, rng }
    }
    fn vec(&mut self) -> Vec<f32> {
        let c = &self.centers[(self.rng.next() % CLUSTERS as u64) as usize];
        c.iter().map(|x| x + self.rng.unit_f32() * NOISE).collect()
    }
}

fn build() -> (TempDir, Db, Vec<Vec<f32>>) {
    let dir = tempfile::tempdir().unwrap();
    let opts = DbOptions {
        durability: Durability::Fast,
        ..DbOptions::default()
    };
    let db = Db::create_with(dir.path().join("vec.clawdb"), opts).unwrap();
    db.create_table(TableSchema::new(
        "docs",
        vec![
            Column::new("n", ColumnType::Int64),
            Column::vector("embedding", DIM),
        ],
    ))
    .unwrap();
    db.create_vector_index("docs", "embedding", VectorIndexOptions::default()).unwrap();

    let mut gen = Clustered::new(0xABCDEF);
    let mut vectors = Vec::with_capacity(N);
    let mut txn = db.begin();
    for i in 0..N {
        let v = gen.vec();
        let mut r = Record::new();
        r.insert("id".into(), Value::Text(format!("d-{i:06}")));
        r.insert("n".into(), Value::Int64(i as i64));
        r.insert("embedding".into(), Value::Vector(v.clone()));
        txn.insert("docs", r).unwrap();
        vectors.push(v);
        if i % 10_000 == 9_999 {
            txn.commit().unwrap();
            txn = db.begin();
        }
    }
    txn.commit().unwrap();
    (dir, db, vectors)
}

fn measure_recall(db: &Db, vectors: &[Vec<f32>], ef_search: usize, queries: usize) -> f64 {
    let mut gen = Clustered::new(0x5EED_0000_0001);
    let mut hit = 0usize;
    for _ in 0..queries {
        let q = gen.vec();
        let mut truth: Vec<(usize, f32)> = vectors
            .iter()
            .enumerate()
            .map(|(i, v)| (i, cosine_distance(&q, v)))
            .collect();
        truth.select_nth_unstable_by(K, |a, b| a.1.total_cmp(&b.1));
        let truth_ids: std::collections::HashSet<String> = truth[..K]
            .iter()
            .map(|(i, _)| format!("d-{i:06}"))
            .collect();
        let opts = VectorSearchOptions {
            ef_search: Some(ef_search),
            ..Default::default()
        };
        let found = db.search_vector("docs", "embedding", &q, K, &opts).unwrap();
        hit += found.iter().filter(|h| truth_ids.contains(&h.id)).count();
    }
    hit as f64 / (queries * K) as f64
}

fn bench_vector(c: &mut Criterion) {
    let (_dir, db, vectors) = build();

    // Acceptance metric: recall@10 vs brute-force ground truth.
    for ef in [64, 128, 256] {
        let recall = measure_recall(&db, &vectors, ef, 50);
        println!("recall@{K} (N={N}, dim={DIM}, ef_search={ef}): {recall:.4}");
        if ef == 128 {
            assert!(recall >= 0.85, "recall@10 with ef=128 must be >= 0.85, got {recall}");
        }
    }

    let mut g = c.benchmark_group("vector_100k");
    g.sample_size(30);
    let mut gen = Clustered::new(0x1111);
    for ef in [64_usize, 128] {
        g.bench_function(format!("search_top10_ef{ef}"), |b| {
            let opts = VectorSearchOptions {
                ef_search: Some(ef),
                ..Default::default()
            };
            b.iter(|| {
                let q = gen.vec();
                let hits = db.search_vector("docs", "embedding", &q, K, &opts).unwrap();
                black_box(hits);
            })
        });
    }
    g.finish();
}

criterion_group!(benches, bench_vector);
criterion_main!(benches);
