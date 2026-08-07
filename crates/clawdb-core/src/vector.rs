//! Native vector indexing: an engine-owned HNSW implementation.
//!
//! Spike outcome (Phase 3): the plan preferred an existing crate, but
//! `hnsw_rs` 0.3.4 failed the acceptance criterion — its upper-layer descent
//! performs a single greedy hop per layer instead of iterating to the local
//! minimum (see `search_filter` in its hnsw.rs), which measured as recall@10
//! of 0.47-0.77 at ef=128 on 100K clustered vectors, non-monotonic in ef.
//! `usearch` was rejected for its C++ build chain (hurts the future WASM
//! target). This module implements the standard HNSW algorithm (Malkov &
//! Yashunin 2016) with the neighbor-selection heuristic; recall is validated
//! against brute-force ground truth in tests and benchmarks.
//!
//! The graph is a derived structure: vectors live in canonical segments and
//! the index is rebuilt from them on open and compaction, so a lost graph
//! can never lose data. Deletes/updates are logical (HNSW does not remove
//! nodes): stale labels are tombstoned, filtered at search time, and dropped
//! for real when compaction rebuilds.

use std::cmp::Reverse;
use std::collections::{BinaryHeap, HashMap, HashSet};
use std::hash::{BuildHasherDefault, Hasher};

use serde::{Deserialize, Serialize};

/// Distance metric for a vector index. Scores returned by searches are
/// distances: lower is closer (`cosine` and `dot` return `1 - similarity`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum VectorMetric {
    Cosine,
    Dot,
    L2,
}

/// When a committed vector becomes searchable.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum IndexingMode {
    /// Searchable as soon as the commit returns.
    Sync,
    /// The commit returns fast; the vector enters the index in background.
    Async,
}

/// Persisted definition of a vector index (stored in the catalog).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct VectorIndexDef {
    pub column: String,
    pub metric: VectorMetric,
    #[serde(default = "default_m")]
    pub m: usize,
    #[serde(default = "default_ef_construction")]
    pub ef_construction: usize,
    #[serde(default = "default_mode")]
    pub mode: IndexingMode,
}

fn default_m() -> usize {
    16
}
fn default_ef_construction() -> usize {
    200
}
fn default_mode() -> IndexingMode {
    IndexingMode::Sync
}

/// Options for [`crate::Db::create_vector_index`].
#[derive(Debug, Clone)]
pub struct VectorIndexOptions {
    pub metric: VectorMetric,
    pub mode: IndexingMode,
    /// HNSW max connections per node (typical: 12-48).
    pub m: usize,
    /// HNSW construction beam width (typical: 100-400).
    pub ef_construction: usize,
}

impl Default for VectorIndexOptions {
    fn default() -> Self {
        VectorIndexOptions {
            metric: VectorMetric::Cosine,
            mode: IndexingMode::Sync,
            m: default_m(),
            ef_construction: default_ef_construction(),
        }
    }
}

/// Options for [`crate::Db::search_vector`].
#[derive(Debug, Clone, Default)]
pub struct VectorSearchOptions {
    /// Search beam width; higher = better recall, slower. Default max(64, 2*top_k).
    pub ef_search: Option<usize>,
    /// Equality filters on other columns of the record (metadata filter).
    pub filter: Option<crate::Record>,
}

/// One vector search hit: lower `distance` is closer.
#[derive(Debug, Clone)]
pub struct VectorHit {
    pub id: String,
    pub distance: f32,
    pub record: crate::Record,
}

// --- fast integer hashing for visited sets --------------------------------------

#[derive(Default)]
struct U32Hasher(u64);

impl Hasher for U32Hasher {
    fn finish(&self) -> u64 {
        self.0
    }
    fn write(&mut self, bytes: &[u8]) {
        for &b in bytes {
            self.0 = (self.0 ^ b as u64).wrapping_mul(0x9E37_79B9_7F4A_7C15);
        }
    }
    fn write_u32(&mut self, n: u32) {
        self.0 = (n as u64 ^ 0x5851_F42D_4C95_7F2D).wrapping_mul(0x9E37_79B9_7F4A_7C15);
    }
}

type VisitedSet = HashSet<u32, BuildHasherDefault<U32Hasher>>;

// --- HNSW ------------------------------------------------------------------------

/// Candidate ordered by distance; total order via `total_cmp`.
#[derive(PartialEq)]
struct Cand(f32, u32);

impl Eq for Cand {}
impl PartialOrd for Cand {
    fn partial_cmp(&self, other: &Self) -> Option<std::cmp::Ordering> {
        Some(self.cmp(other))
    }
}
impl Ord for Cand {
    fn cmp(&self, other: &Self) -> std::cmp::Ordering {
        self.0.total_cmp(&other.0).then(self.1.cmp(&other.1))
    }
}

struct HnswIndex {
    metric: VectorMetric,
    m: usize,
    m0: usize,
    ef_construction: usize,
    /// Level multiplier 1/ln(m).
    ml: f64,
    vectors: Vec<Vec<f32>>,
    norms: Vec<f32>,
    /// node -> level -> neighbor labels.
    links: Vec<Vec<Vec<u32>>>,
    entry: Option<u32>,
    top_level: u8,
    rng: u64,
}

impl HnswIndex {
    fn new(metric: VectorMetric, m: usize, ef_construction: usize) -> HnswIndex {
        let m = m.clamp(2, 256);
        HnswIndex {
            metric,
            m,
            m0: m * 2,
            ef_construction: ef_construction.max(m),
            ml: 1.0 / (m as f64).ln(),
            vectors: Vec::new(),
            norms: Vec::new(),
            links: Vec::new(),
            entry: None,
            top_level: 0,
            rng: 0x9E37_79B9_7F4A_7C15,
        }
    }

    fn random_level(&mut self) -> u8 {
        self.rng ^= self.rng << 13;
        self.rng ^= self.rng >> 7;
        self.rng ^= self.rng << 17;
        let unit = ((self.rng >> 11) as f64 / (1u64 << 53) as f64).max(1e-12);
        ((-unit.ln() * self.ml) as usize).min(31) as u8
    }

    fn dist_between(&self, a: u32, b: u32) -> f32 {
        self.dist_raw(&self.vectors[a as usize], self.norms[a as usize], b)
    }

    fn dist_raw(&self, v: &[f32], vnorm: f32, label: u32) -> f32 {
        let w = &self.vectors[label as usize];
        match self.metric {
            VectorMetric::Cosine => {
                let dot: f32 = v.iter().zip(w).map(|(x, y)| x * y).sum();
                1.0 - dot / (vnorm * self.norms[label as usize]).max(f32::EPSILON)
            }
            VectorMetric::Dot => {
                let dot: f32 = v.iter().zip(w).map(|(x, y)| x * y).sum();
                1.0 - dot
            }
            VectorMetric::L2 => v
                .iter()
                .zip(w)
                .map(|(x, y)| (x - y) * (x - y))
                .sum::<f32>()
                .sqrt(),
        }
    }

    /// Greedy within-layer walk to the local minimum (the step hnsw_rs got
    /// wrong: it must loop until no neighbor improves, not scan once).
    fn greedy_descend(&self, v: &[f32], vnorm: f32, mut ep: u32, mut ep_dist: f32, layer: usize) -> (u32, f32) {
        loop {
            let mut improved = false;
            let neighbours = &self.links[ep as usize][layer];
            for &n in neighbours {
                let d = self.dist_raw(v, vnorm, n);
                if d < ep_dist {
                    ep = n;
                    ep_dist = d;
                    improved = true;
                }
            }
            if !improved {
                return (ep, ep_dist);
            }
        }
    }

    /// Algorithm 2: beam search within one layer. `eps` seeds the beam;
    /// returns up to `ef` closest, ascending.
    fn search_layer(&self, v: &[f32], vnorm: f32, eps: &[(f32, u32)], ef: usize, layer: usize) -> Vec<(f32, u32)> {
        let mut visited: VisitedSet = HashSet::default();
        let mut candidates: BinaryHeap<Reverse<Cand>> = BinaryHeap::new(); // min-heap
        let mut results: BinaryHeap<Cand> = BinaryHeap::new(); // max-heap of best ef
        for &(d, l) in eps {
            if visited.insert(l) {
                candidates.push(Reverse(Cand(d, l)));
                results.push(Cand(d, l));
            }
        }
        while results.len() > ef {
            results.pop();
        }
        while let Some(Reverse(Cand(cd, cl))) = candidates.pop() {
            let worst = results.peek().map(|c| c.0).unwrap_or(f32::INFINITY);
            if cd > worst && results.len() >= ef {
                break;
            }
            for &n in &self.links[cl as usize][layer] {
                if !visited.insert(n) {
                    continue;
                }
                let d = self.dist_raw(v, vnorm, n);
                let worst = results.peek().map(|c| c.0).unwrap_or(f32::INFINITY);
                if results.len() < ef || d < worst {
                    candidates.push(Reverse(Cand(d, n)));
                    results.push(Cand(d, n));
                    if results.len() > ef {
                        results.pop();
                    }
                }
            }
        }
        let mut out: Vec<(f32, u32)> = results.into_iter().map(|Cand(d, l)| (d, l)).collect();
        out.sort_by(|a, b| a.0.total_cmp(&b.0));
        out
    }

    /// Algorithm 4: diversity-preserving neighbor selection, with pruned
    /// candidates kept as fill so nodes never end up under-connected.
    fn select_neighbors(&self, candidates: &[(f32, u32)], m: usize) -> Vec<u32> {
        let mut selected: Vec<(f32, u32)> = Vec::with_capacity(m);
        let mut skipped: Vec<(f32, u32)> = Vec::new();
        for &(d, c) in candidates {
            if selected.len() >= m {
                break;
            }
            let diverse = selected
                .iter()
                .all(|&(_, s)| self.dist_between(c, s) >= d);
            if diverse {
                selected.push((d, c));
            } else {
                skipped.push((d, c));
            }
        }
        for &(d, c) in &skipped {
            if selected.len() >= m {
                break;
            }
            selected.push((d, c));
        }
        selected.into_iter().map(|(_, c)| c).collect()
    }

    fn insert(&mut self, v: &[f32]) -> u32 {
        let label = self.vectors.len() as u32;
        let norm = v.iter().map(|x| x * x).sum::<f32>().sqrt();
        let level = self.random_level();
        self.vectors.push(v.to_vec());
        self.norms.push(norm);
        self.links.push((0..=level as usize).map(|_| Vec::new()).collect());

        let Some(entry) = self.entry else {
            self.entry = Some(label);
            self.top_level = level;
            return label;
        };

        let mut ep = entry;
        let mut ep_dist = self.dist_raw(v, norm, ep);
        // Descend through layers above the new node's level.
        if self.top_level > level {
            for l in ((level as usize + 1)..=self.top_level as usize).rev() {
                (ep, ep_dist) = self.greedy_descend(v, norm, ep, ep_dist, l);
            }
        }
        // Connect on each layer from min(level, top) down to 0.
        let mut eps = vec![(ep_dist, ep)];
        for l in (0..=level.min(self.top_level) as usize).rev() {
            let candidates = self.search_layer(v, norm, &eps, self.ef_construction, l);
            let mmax = if l == 0 { self.m0 } else { self.m };
            let neighbors = self.select_neighbors(&candidates, self.m);
            for &n in &neighbors {
                self.links[n as usize][l].push(label);
                if self.links[n as usize][l].len() > mmax {
                    self.prune(n, l, mmax);
                }
            }
            self.links[label as usize][l] = neighbors;
            eps = candidates;
        }
        if level > self.top_level {
            self.top_level = level;
            self.entry = Some(label);
        }
        label
    }

    fn prune(&mut self, node: u32, layer: usize, mmax: usize) {
        let current = self.links[node as usize][layer].clone();
        let mut with_dist: Vec<(f32, u32)> = current
            .into_iter()
            .map(|n| (self.dist_between(node, n), n))
            .collect();
        with_dist.sort_by(|a, b| a.0.total_cmp(&b.0));
        self.links[node as usize][layer] = self.select_neighbors(&with_dist, mmax);
    }

    fn search(&self, v: &[f32], k: usize, ef: usize) -> Vec<(u32, f32)> {
        let Some(entry) = self.entry else {
            return Vec::new();
        };
        let norm = v.iter().map(|x| x * x).sum::<f32>().sqrt();
        let mut ep = entry;
        let mut ep_dist = self.dist_raw(v, norm, ep);
        for l in (1..=self.top_level as usize).rev() {
            (ep, ep_dist) = self.greedy_descend(v, norm, ep, ep_dist, l);
        }
        let results = self.search_layer(v, norm, &[(ep_dist, ep)], ef.max(k), 0);
        results
            .into_iter()
            .take(k)
            .map(|(d, l)| (l, d))
            .collect()
    }
}

/// One in-memory vector index over (table, column). Rebuilt from canonical
/// data on open and on compaction; never a source of truth.
pub(crate) struct VecIdx {
    backend: HnswIndex,
    /// label -> record id.
    labels: Vec<String>,
    /// record id -> current (latest) label.
    id_to_label: HashMap<String, usize>,
    /// labels superseded by updates or deletes; filtered at search time.
    deleted: HashSet<usize>,
}

impl VecIdx {
    pub fn new(def: VectorIndexDef) -> VecIdx {
        VecIdx {
            backend: HnswIndex::new(def.metric, def.m, def.ef_construction),
            labels: Vec::new(),
            id_to_label: HashMap::new(),
            deleted: HashSet::new(),
        }
    }

    /// Index (or re-index) the vector for a record. A previous vector for
    /// the same id is tombstoned.
    pub fn insert(&mut self, id: &str, v: &[f32]) {
        if let Some(old) = self.id_to_label.get(id) {
            self.deleted.insert(*old);
        }
        let label = self.backend.insert(v) as usize;
        debug_assert_eq!(label, self.labels.len());
        self.labels.push(id.to_owned());
        self.id_to_label.insert(id.to_owned(), label);
    }

    /// Tombstone a record's vector (delete, or update that removed it).
    pub fn remove(&mut self, id: &str) {
        if let Some(label) = self.id_to_label.remove(id) {
            self.deleted.insert(label);
        }
    }

    pub fn live_len(&self) -> usize {
        self.id_to_label.len()
    }

    /// Total labels in the backend, including tombstoned ones. Over-fetch
    /// escalation must cap here, not at `live_len`, or a search could give
    /// up while only tombstones have been pulled.
    pub fn total_len(&self) -> usize {
        self.labels.len()
    }

    /// Raw ANN candidates with tombstones and stale labels filtered out,
    /// sorted by ascending distance. May return fewer than `fetch_k`.
    pub fn search_raw(&self, q: &[f32], fetch_k: usize, ef: usize) -> Vec<(String, f32)> {
        if self.labels.is_empty() {
            return Vec::new();
        }
        let k = fetch_k.min(self.labels.len());
        let raw = self.backend.search(q, k, ef.max(k));
        let mut out = Vec::with_capacity(raw.len());
        for (label, distance) in raw {
            let label = label as usize;
            if self.deleted.contains(&label) {
                continue;
            }
            let Some(id) = self.labels.get(label) else { continue };
            // Guard against stale labels not yet tombstoned.
            if self.id_to_label.get(id) != Some(&label) {
                continue;
            }
            out.push((id.clone(), distance));
        }
        out
    }
}
