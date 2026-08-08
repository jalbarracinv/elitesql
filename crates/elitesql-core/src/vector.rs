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
use std::fs::{File, OpenOptions};
use std::hash::{BuildHasherDefault, Hasher};
use std::io::{BufWriter, Seek, SeekFrom, Write};
use std::path::Path;
use std::sync::Arc;

use memmap2::{Advice, Mmap, UncheckedAdvice};
use serde::{Deserialize, Serialize};

use crate::error::{Error, Result};
use crate::value::{read_u16, read_u32, read_u64, read_u8};

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
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct VectorIndexDef {
    pub column: String,
    pub metric: VectorMetric,
    #[serde(default = "default_m")]
    pub m: usize,
    #[serde(default = "default_ef_construction")]
    pub ef_construction: usize,
    #[serde(default = "default_mode")]
    pub mode: IndexingMode,
    /// Store vectors as int8 with a per-vector scale: ~4x less memory and
    /// disk at a small recall cost. Canonical f32 data is untouched.
    #[serde(default)]
    pub quantized: bool,
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
    /// int8 scalar quantization of the in-index vectors.
    pub quantized: bool,
}

impl Default for VectorIndexOptions {
    fn default() -> Self {
        VectorIndexOptions {
            metric: VectorMetric::Cosine,
            mode: IndexingMode::Sync,
            m: default_m(),
            ef_construction: default_ef_construction(),
            quantized: false,
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

/// In-index vector storage: full f32, or int8 with a per-vector scale
/// (x ≈ q * scale), which cuts memory and dump size ~4x.
enum VecStore {
    F32 {
        dim: usize,
        values: Vec<f32>,
    },
    I8 {
        dim: usize,
        values: Vec<i8>,
        scales: Vec<f32>,
    },
}

impl VecStore {
    fn len(&self) -> usize {
        match self {
            VecStore::F32 { dim: 0, .. } | VecStore::I8 { dim: 0, .. } => 0,
            VecStore::F32 { dim, values } => values.len() / dim,
            VecStore::I8 { dim, scales, .. } => {
                debug_assert_eq!(scales.len(), self.raw_len() / dim);
                scales.len()
            }
        }
    }

    fn raw_len(&self) -> usize {
        match self {
            VecStore::F32 { values, .. } => values.len(),
            VecStore::I8 { values, .. } => values.len(),
        }
    }

    fn f32_at(&self, index: usize) -> &[f32] {
        let VecStore::F32 { dim, values } = self else {
            unreachable!("f32 accessor on quantized store")
        };
        &values[index * dim..(index + 1) * dim]
    }

    fn i8_at(&self, index: usize) -> (&[i8], f32) {
        let VecStore::I8 {
            dim,
            values,
            scales,
        } = self
        else {
            unreachable!("i8 accessor on f32 store")
        };
        (&values[index * dim..(index + 1) * dim], scales[index])
    }

    /// Append, returning the stored vector's norm (of what was stored,
    /// i.e. post-quantization, so cosine stays self-consistent).
    fn push(&mut self, v: &[f32]) -> f32 {
        match self {
            VecStore::F32 { dim, values } => {
                if *dim == 0 {
                    *dim = v.len();
                }
                debug_assert_eq!(*dim, v.len());
                values.extend_from_slice(v);
                v.iter().map(|x| x * x).sum::<f32>().sqrt()
            }
            VecStore::I8 {
                dim,
                values,
                scales,
            } => {
                if *dim == 0 {
                    *dim = v.len();
                }
                debug_assert_eq!(*dim, v.len());
                let max_abs = v.iter().fold(0.0f32, |a, x| a.max(x.abs()));
                let scale = if max_abs == 0.0 { 1.0 } else { max_abs / 127.0 };
                let start = values.len();
                values.extend(
                    v.iter()
                        .map(|x| (x / scale).round().clamp(-127.0, 127.0) as i8),
                );
                let sumsq: i64 = values[start..].iter().map(|&b| b as i64 * b as i64).sum();
                scales.push(scale);
                scale * (sumsq as f32).sqrt()
            }
        }
    }
}

struct HnswIndex {
    metric: VectorMetric,
    m: usize,
    m0: usize,
    ef_construction: usize,
    /// Level multiplier 1/ln(m).
    ml: f64,
    store: VecStore,
    norms: Vec<f32>,
    /// node -> level -> neighbor labels.
    links: Vec<Vec<Vec<u32>>>,
    entry: Option<u32>,
    top_level: u8,
    rng: u64,
}

impl HnswIndex {
    fn new(metric: VectorMetric, m: usize, ef_construction: usize, quantized: bool) -> HnswIndex {
        let m = m.clamp(2, 256);
        HnswIndex {
            metric,
            m,
            m0: m * 2,
            ef_construction: ef_construction.max(m),
            ml: 1.0 / (m as f64).ln(),
            store: if quantized {
                VecStore::I8 {
                    dim: 0,
                    values: Vec::new(),
                    scales: Vec::new(),
                }
            } else {
                VecStore::F32 {
                    dim: 0,
                    values: Vec::new(),
                }
            },
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
        match &self.store {
            VecStore::F32 { .. } => {
                self.dist_raw(self.store.f32_at(a as usize), self.norms[a as usize], b)
            }
            VecStore::I8 { .. } => {
                let (qa, sa) = self.store.i8_at(a as usize);
                let (qb, sb) = self.store.i8_at(b as usize);
                match self.metric {
                    VectorMetric::Cosine => {
                        let dot_i: i64 =
                            qa.iter().zip(qb).map(|(&x, &y)| x as i64 * y as i64).sum();
                        let dot = dot_i as f32 * sa * sb;
                        1.0 - dot
                            / (self.norms[a as usize] * self.norms[b as usize]).max(f32::EPSILON)
                    }
                    VectorMetric::Dot => {
                        let dot_i: i64 =
                            qa.iter().zip(qb).map(|(&x, &y)| x as i64 * y as i64).sum();
                        1.0 - dot_i as f32 * sa * sb
                    }
                    VectorMetric::L2 => qa
                        .iter()
                        .zip(qb)
                        .map(|(&x, &y)| {
                            let d = x as f32 * sa - y as f32 * sb;
                            d * d
                        })
                        .sum::<f32>()
                        .sqrt(),
                }
            }
        }
    }

    fn dist_raw(&self, v: &[f32], vnorm: f32, label: u32) -> f32 {
        match &self.store {
            VecStore::F32 { .. } => {
                let w = self.store.f32_at(label as usize);
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
            VecStore::I8 { .. } => {
                let (q, scale) = self.store.i8_at(label as usize);
                match self.metric {
                    VectorMetric::Cosine => {
                        let dot: f32 =
                            v.iter().zip(q).map(|(x, &y)| x * y as f32).sum::<f32>() * scale;
                        1.0 - dot / (vnorm * self.norms[label as usize]).max(f32::EPSILON)
                    }
                    VectorMetric::Dot => {
                        let dot: f32 =
                            v.iter().zip(q).map(|(x, &y)| x * y as f32).sum::<f32>() * scale;
                        1.0 - dot
                    }
                    VectorMetric::L2 => v
                        .iter()
                        .zip(q)
                        .map(|(x, &y)| {
                            let d = x - y as f32 * scale;
                            d * d
                        })
                        .sum::<f32>()
                        .sqrt(),
                }
            }
        }
    }

    /// Greedy within-layer walk to the local minimum (the step hnsw_rs got
    /// wrong: it must loop until no neighbor improves, not scan once).
    fn greedy_descend(
        &self,
        v: &[f32],
        vnorm: f32,
        mut ep: u32,
        mut ep_dist: f32,
        layer: usize,
    ) -> (u32, f32) {
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
    fn search_layer(
        &self,
        v: &[f32],
        vnorm: f32,
        eps: &[(f32, u32)],
        ef: usize,
        layer: usize,
    ) -> Vec<(f32, u32)> {
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
            let diverse = selected.iter().all(|&(_, s)| self.dist_between(c, s) >= d);
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
        let label = self.store.len() as u32;
        let level = self.random_level();
        let stored_norm = self.store.push(v);
        // Query-side norm stays the exact f32 norm; the stored norm reflects
        // what the index will compare against.
        let norm = v.iter().map(|x| x * x).sum::<f32>().sqrt();
        self.norms.push(stored_norm);
        self.links
            .push((0..=level as usize).map(|_| Vec::new()).collect());

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
        results.into_iter().take(k).map(|(d, l)| (l, d)).collect()
    }
}

/// One in-memory vector index over (table, column). Rebuilt from canonical
/// data on open and on compaction; never a source of truth.
pub(crate) struct VecIdx {
    /// Immutable on-disk graph. New indexes remain resident until first dump;
    /// reopened indexes query this mapping plus the mutable backend below.
    mapped: Vec<MappedHnsw>,
    backend: HnswIndex,
    /// label -> record id.
    labels: Vec<Arc<str>>,
    /// record id -> current (latest) label.
    id_to_label: HashMap<Arc<str>, usize>,
    /// Labels superseded by updates or deletes. `Vec<bool>` is a packed bitmap,
    /// avoiding one hash-table entry per tombstone.
    deleted: Vec<bool>,
}

impl VecIdx {
    pub fn new(def: VectorIndexDef) -> VecIdx {
        VecIdx {
            mapped: Vec::new(),
            backend: HnswIndex::new(def.metric, def.m, def.ef_construction, def.quantized),
            labels: Vec::new(),
            id_to_label: HashMap::new(),
            deleted: Vec::new(),
        }
    }

    /// Index (or re-index) the vector for a record. A previous vector for
    /// the same id is tombstoned.
    pub fn insert(&mut self, id: &str, v: &[f32]) {
        for mapped in &mut self.mapped {
            mapped.remove(id);
        }
        if let Some(old) = self.id_to_label.get(id) {
            self.deleted[*old] = true;
        }
        let label = self.backend.insert(v) as usize;
        debug_assert_eq!(label, self.labels.len());
        let id: Arc<str> = Arc::from(id);
        self.labels.push(id.clone());
        self.deleted.push(false);
        self.id_to_label.insert(id, label);
    }

    /// Tombstone a record's vector (delete, or update that removed it).
    pub fn remove(&mut self, id: &str) {
        for mapped in &mut self.mapped {
            mapped.remove(id);
        }
        if let Some(label) = self.id_to_label.remove(id) {
            self.deleted[label] = true;
        }
    }

    pub fn live_len(&self) -> usize {
        self.mapped.iter().map(MappedHnsw::live_len).sum::<usize>() + self.id_to_label.len()
    }

    /// Total labels in the backend, including tombstoned ones. Over-fetch
    /// escalation must cap here, not at `live_len`, or a search could give
    /// up while only tombstones have been pulled.
    pub fn total_len(&self) -> usize {
        self.mapped.iter().map(MappedHnsw::len).sum::<usize>() + self.labels.len()
    }

    /// Ids currently indexed (latest labels only).
    pub fn ids(&self) -> Vec<String> {
        let mut ids = Vec::new();
        for mapped in &self.mapped {
            ids.extend(mapped.ids());
        }
        ids.extend(self.id_to_label.keys().map(|id| id.to_string()));
        ids
    }

    /// Raw ANN candidates with tombstones and stale labels filtered out,
    /// sorted by ascending distance. May return fewer than `fetch_k`.
    pub fn search_raw(&self, q: &[f32], fetch_k: usize, ef: usize) -> Vec<(String, f32)> {
        if self.total_len() == 0 {
            return Vec::new();
        }
        let k = fetch_k.min(self.total_len());
        let mut out = Vec::new();
        for mapped in &self.mapped {
            out.extend(mapped.search(q, k, ef.max(k)));
        }
        let raw = self.backend.search(q, k, ef.max(k));
        for (label, distance) in raw {
            let label = label as usize;
            if self.deleted.get(label).copied().unwrap_or(true) {
                continue;
            }
            let Some(id) = self.labels.get(label) else {
                continue;
            };
            // Guard against stale labels not yet tombstoned.
            if self.id_to_label.get(id.as_ref()) != Some(&label) {
                continue;
            }
            out.push((id.to_string(), distance));
        }
        out.sort_by(|left, right| {
            left.1
                .total_cmp(&right.1)
                .then_with(|| left.0.cmp(&right.0))
        });
        out.truncate(k);
        out
    }

    pub(crate) fn has_mapped_base(&self) -> bool {
        !self.mapped.is_empty()
    }

    pub(crate) fn delta_memory_bytes(&self) -> usize {
        let vector_bytes = match &self.backend.store {
            VecStore::F32 { values, .. } => values.len() * std::mem::size_of::<f32>(),
            VecStore::I8 { values, scales, .. } => {
                values.len() + scales.len() * std::mem::size_of::<f32>()
            }
        };
        let link_bytes = self
            .backend
            .links
            .iter()
            .flat_map(|levels| levels.iter())
            .map(|links| links.len() * std::mem::size_of::<u32>() + 24)
            .sum::<usize>();
        let label_bytes = self.labels.iter().map(|id| id.len() + 40).sum::<usize>();
        let overlay_metadata = self
            .mapped
            .iter()
            .skip(1)
            .map(MappedHnsw::metadata_memory_bytes)
            .sum::<usize>();
        vector_bytes
            .saturating_add(link_bytes)
            .saturating_add(label_bytes)
            .saturating_add(self.backend.norms.len() * std::mem::size_of::<f32>())
            .saturating_add(self.deleted.len().div_ceil(8))
            .saturating_add(overlay_metadata)
    }

    /// Freeze only the mutable overlay as another mmap graph. The canonical
    /// base file remains the durable restart point; this run is intentionally
    /// unlinked after mapping because WAL/segments can reconstruct it after a
    /// crash. Multiple immutable runs are searched and merged exactly like a
    /// single base, keeping the write-time graph bounded without rebuilding
    /// every old vector.
    pub(crate) fn flush_delta_mmap(
        &mut self,
        path: &Path,
        table: &str,
        column: &str,
        def: &VectorIndexDef,
        dump_version: u64,
        remove_after_map: bool,
    ) -> Result<()> {
        if self.labels.is_empty() {
            return Ok(());
        }
        self.dump_file(path, table, column, def, dump_version)?;
        let file = File::open(path)?;
        let mmap = unsafe { memmap2::MmapOptions::new().map(&file) }?;
        let (mut loaded, version) = Self::load_mmap(mmap, table, column, def)?;
        if version != dump_version || loaded.mapped.len() != 1 {
            let _ = std::fs::remove_file(path);
            return Err(Error::Corrupt("invalid vector delta generation".into()));
        }
        self.mapped
            .push(loaded.mapped.pop().expect("one mapped run"));
        self.backend = HnswIndex::new(def.metric, def.m, def.ef_construction, def.quantized);
        self.labels.clear();
        self.id_to_label.clear();
        self.deleted.clear();
        // The mapping keeps the inode alive on supported Unix platforms; a
        // restart intentionally rebuilds this non-durable overlay from WAL.
        if remove_after_map {
            let _ = std::fs::remove_file(path);
        }
        Ok(())
    }
}

// --- mmap-native graph format (V4) -----------------------------------------

const MMAP_HEADER_FIXED: usize = 72;

struct MappedHnsw {
    mmap: Mmap,
    metric: VectorMetric,
    quantized: bool,
    node_count: usize,
    directory_offset: usize,
    levels: Vec<u8>,
    entry: Option<u32>,
    top_level: u8,
    id_to_label: HashMap<Arc<str>, usize>,
    deleted: Vec<bool>,
}

struct MappedNode<'a> {
    id: &'a str,
    deleted: bool,
    norm: f32,
    dim: usize,
    vector_pos: usize,
    scale: f32,
    levels_pos: usize,
    level_count: u8,
}

impl VecIdx {
    /// Stream a resident graph to a sectioned file. The node directory lives
    /// at the tail, so publication needs only O(nodes) offsets and never a
    /// second graph-sized byte buffer.
    pub(crate) fn dump_file(
        &self,
        path: &Path,
        table: &str,
        column: &str,
        def: &VectorIndexDef,
        dump_version: u64,
    ) -> Result<()> {
        let backend = &self.backend;
        let count = backend.store.len();
        let header_len = MMAP_HEADER_FIXED
            .checked_add(table.len())
            .and_then(|len| len.checked_add(column.len()))
            .ok_or_else(|| Error::InvalidArgument("vidx: header too large".into()))?;
        if table.len() > u16::MAX as usize || column.len() > u16::MAX as usize {
            return Err(Error::InvalidArgument("vidx: identity too long".into()));
        }
        let raw_file = OpenOptions::new()
            .create(true)
            .truncate(true)
            .read(true)
            .write(true)
            .open(path)?;
        let mut file = VectorDumpWriter::new(raw_file, header_len)?;
        let mut offsets = Vec::with_capacity(count.saturating_add(1));
        for node in 0..count {
            offsets.push(file.position());
            file.write(&(self.labels[node].len() as u16).to_le_bytes())?;
            file.write(self.labels[node].as_bytes())?;
            file.write(&[self.deleted[node] as u8])?;
            file.write(&backend.norms[node].to_le_bytes())?;
            match &backend.store {
                VecStore::F32 { .. } => {
                    let vector = backend.store.f32_at(node);
                    file.write(&(vector.len() as u32).to_le_bytes())?;
                    write_f32_slice(&mut file, vector)?;
                }
                VecStore::I8 { .. } => {
                    let (vector, scale) = backend.store.i8_at(node);
                    file.write(&(vector.len() as u32).to_le_bytes())?;
                    file.write(&scale.to_le_bytes())?;
                    // i8 and u8 have identical byte representation.
                    let bytes = unsafe {
                        std::slice::from_raw_parts(vector.as_ptr().cast::<u8>(), vector.len())
                    };
                    file.write(bytes)?;
                }
            }
            let levels = &backend.links[node];
            file.write(&[levels.len() as u8])?;
            for level in levels {
                file.write(&(level.len() as u32).to_le_bytes())?;
                write_u32_slice(&mut file, level)?;
            }
        }
        let directory_offset = file.position();
        offsets.push(directory_offset);
        for offset in &offsets {
            file.write(&offset.to_le_bytes())?;
        }
        let body_crc = file.body_crc();

        let mut header = Vec::with_capacity(header_len);
        header.extend_from_slice(VIDX_MAGIC);
        header.extend_from_slice(&VIDX_FORMAT_MMAP.to_le_bytes());
        header.extend_from_slice(&0u32.to_le_bytes());
        header.extend_from_slice(&body_crc.to_le_bytes());
        header.extend_from_slice(&(header_len as u32).to_le_bytes());
        header.extend_from_slice(&dump_version.to_le_bytes());
        header.extend_from_slice(&(count as u32).to_le_bytes());
        header.extend_from_slice(&directory_offset.to_le_bytes());
        header.push(backend.top_level);
        header.push(metric_code(def.metric));
        header.push(def.quantized as u8);
        header.push(0);
        header.extend_from_slice(&backend.entry.unwrap_or(u32::MAX).to_le_bytes());
        header.extend_from_slice(&(def.m as u32).to_le_bytes());
        header.extend_from_slice(&(def.ef_construction as u32).to_le_bytes());
        header.extend_from_slice(&backend.rng.to_le_bytes());
        header.extend_from_slice(&(table.len() as u16).to_le_bytes());
        header.extend_from_slice(&(column.len() as u16).to_le_bytes());
        header.extend_from_slice(table.as_bytes());
        header.extend_from_slice(column.as_bytes());
        debug_assert_eq!(header.len(), header_len);
        let header_crc = crc32fast::hash(&header[16..]);
        header[12..16].copy_from_slice(&header_crc.to_le_bytes());
        file.finish(&header)
    }

    pub(crate) fn load_mmap(
        mmap: Mmap,
        table: &str,
        column: &str,
        def: &VectorIndexDef,
    ) -> Result<(Self, u64)> {
        if mmap.len() < MMAP_HEADER_FIXED || &mmap[..8] != VIDX_MAGIC {
            return Err(Error::Corrupt("vidx: bad mmap header".into()));
        }
        let _ = mmap.advise(Advice::Sequential);
        if fixed_u32(&mmap, 8)? != VIDX_FORMAT_MMAP {
            return Err(Error::Corrupt("vidx: unsupported mmap format".into()));
        }
        let header_len = fixed_u32(&mmap, 20)? as usize;
        if header_len < MMAP_HEADER_FIXED || header_len > mmap.len() {
            return Err(Error::Corrupt("vidx: invalid header length".into()));
        }
        if crc32fast::hash(&mmap[16..header_len]) != fixed_u32(&mmap, 12)? {
            return Err(Error::Corrupt("vidx: header crc mismatch".into()));
        }
        let dump_version = fixed_u64(&mmap, 24)?;
        let node_count = fixed_u32(&mmap, 32)? as usize;
        let directory_offset = usize::try_from(fixed_u64(&mmap, 36)?)
            .map_err(|_| Error::Corrupt("vidx: directory overflow".into()))?;
        let directory_bytes = node_count
            .checked_add(1)
            .and_then(|count| count.checked_mul(8))
            .ok_or_else(|| Error::Corrupt("vidx: directory overflow".into()))?;
        if directory_offset < header_len
            || directory_offset
                .checked_add(directory_bytes)
                .is_none_or(|end| end != mmap.len())
        {
            return Err(Error::Corrupt("vidx: invalid directory".into()));
        }
        if crc32fast::hash(&mmap[header_len..]) != fixed_u32(&mmap, 16)? {
            return Err(Error::Corrupt("vidx: body crc mismatch".into()));
        }
        let top_level = mmap[44];
        let metric = match mmap[45] {
            0 => VectorMetric::Cosine,
            1 => VectorMetric::Dot,
            2 => VectorMetric::L2,
            _ => return Err(Error::Corrupt("vidx: invalid metric".into())),
        };
        let quantized = mmap[46] != 0;
        let entry_raw = fixed_u32(&mmap, 48)?;
        let same_def = metric == def.metric
            && quantized == def.quantized
            && fixed_u32(&mmap, 52)? as usize == def.m
            && fixed_u32(&mmap, 56)? as usize == def.ef_construction;
        if !same_def {
            return Err(Error::Corrupt("vidx: index definition changed".into()));
        }
        let table_len = fixed_u16(&mmap, 68)? as usize;
        let column_len = fixed_u16(&mmap, 70)? as usize;
        let table_bytes = mmap
            .get(MMAP_HEADER_FIXED..MMAP_HEADER_FIXED + table_len)
            .ok_or_else(|| Error::Corrupt("vidx: truncated table identity".into()))?;
        let column_bytes = mmap
            .get(MMAP_HEADER_FIXED + table_len..header_len)
            .ok_or_else(|| Error::Corrupt("vidx: truncated column identity".into()))?;
        if table_bytes != table.as_bytes()
            || column_bytes != column.as_bytes()
            || MMAP_HEADER_FIXED + table_len + column_len != header_len
        {
            return Err(Error::Corrupt("vidx: identity mismatch".into()));
        }

        let mut levels = Vec::with_capacity(node_count);
        let mut deleted = Vec::with_capacity(node_count);
        let mut id_to_label = HashMap::with_capacity(node_count);
        let mut dimension = None;
        for label in 0..node_count {
            let node = parse_mapped_node(&mmap, directory_offset, node_count, quantized, label)?;
            if node.level_count == 0 || node.level_count as usize > MAX_LEVELS {
                return Err(Error::Corrupt("vidx: bad level count".into()));
            }
            if dimension
                .replace(node.dim)
                .is_some_and(|dim| dim != node.dim)
            {
                return Err(Error::Corrupt("vidx: inconsistent dimensions".into()));
            }
            levels.push(node.level_count);
            deleted.push(node.deleted);
            if !node.deleted {
                id_to_label.insert(Arc::from(node.id), label);
            }
        }
        let entry = if entry_raw == u32::MAX {
            None
        } else if entry_raw as usize >= node_count || levels[entry_raw as usize] <= top_level {
            return Err(Error::Corrupt("vidx: invalid entry point".into()));
        } else {
            Some(entry_raw)
        };
        let mapped = MappedHnsw {
            mmap,
            metric,
            quantized,
            node_count,
            directory_offset,
            levels,
            entry,
            top_level,
            id_to_label,
            deleted,
        };
        mapped.validate_links()?;
        // The global integrity pass has just touched the whole graph. Release
        // those clean file-backed pages so steady-state residency is driven by
        // actual ANN traversal, not by validation at open.
        // SAFETY: the mapping is read-only, file-backed and has no outstanding
        // slices here; discarding clean cache pages cannot change its contents.
        let _ = unsafe { mapped.mmap.unchecked_advise(UncheckedAdvice::DontNeed) };
        let _ = mapped.mmap.advise(Advice::Random);
        Ok((
            VecIdx {
                mapped: vec![mapped],
                backend: HnswIndex::new(def.metric, def.m, def.ef_construction, def.quantized),
                labels: Vec::new(),
                id_to_label: HashMap::new(),
                deleted: Vec::new(),
            },
            dump_version,
        ))
    }
}

impl MappedHnsw {
    fn metadata_memory_bytes(&self) -> usize {
        self.levels
            .len()
            .saturating_add(self.deleted.len().div_ceil(8))
            .saturating_add(
                self.id_to_label
                    .keys()
                    .map(|id| id.len() + 56)
                    .sum::<usize>(),
            )
    }

    fn len(&self) -> usize {
        self.node_count
    }

    fn live_len(&self) -> usize {
        self.id_to_label.len()
    }

    fn ids(&self) -> Vec<String> {
        self.id_to_label.keys().map(|id| id.to_string()).collect()
    }

    fn remove(&mut self, id: &str) {
        if let Some(label) = self.id_to_label.remove(id) {
            self.deleted[label] = true;
        }
    }

    fn node(&self, label: usize) -> MappedNode<'_> {
        parse_mapped_node(
            &self.mmap,
            self.directory_offset,
            self.node_count,
            self.quantized,
            label,
        )
        .expect("mapped graph was validated on open")
    }

    fn validate_links(&self) -> Result<()> {
        for label in 0..self.node_count {
            for layer in 0..self.levels[label] as usize {
                for neighbor in self.neighbors(label as u32, layer) {
                    let neighbor = neighbor as usize;
                    if neighbor >= self.node_count || self.levels[neighbor] as usize <= layer {
                        return Err(Error::Corrupt("vidx: invalid neighbor layer".into()));
                    }
                }
            }
        }
        Ok(())
    }

    fn neighbors(&self, label: u32, layer: usize) -> MappedNeighbors<'_> {
        let node = self.node(label as usize);
        let mut pos = node.levels_pos + 1;
        for current in 0..node.level_count as usize {
            let count = fixed_u32(&self.mmap, pos).expect("validated link count") as usize;
            pos += 4;
            if current == layer {
                return MappedNeighbors {
                    bytes: &self.mmap[pos..pos + count * 4],
                    pos: 0,
                };
            }
            pos += count * 4;
        }
        MappedNeighbors { bytes: &[], pos: 0 }
    }

    fn dist_raw(&self, query: &[f32], query_norm: f32, label: u32) -> f32 {
        let node = self.node(label as usize);
        if self.quantized {
            let vector = &self.mmap[node.vector_pos..node.vector_pos + node.dim];
            match self.metric {
                VectorMetric::Cosine => {
                    let dot = query
                        .iter()
                        .zip(vector)
                        .map(|(x, value)| *x * (*value as i8 as f32))
                        .sum::<f32>()
                        * node.scale;
                    1.0 - dot / (query_norm * node.norm).max(f32::EPSILON)
                }
                VectorMetric::Dot => {
                    let dot = query
                        .iter()
                        .zip(vector)
                        .map(|(x, value)| *x * (*value as i8 as f32))
                        .sum::<f32>()
                        * node.scale;
                    1.0 - dot
                }
                VectorMetric::L2 => query
                    .iter()
                    .zip(vector)
                    .map(|(x, value)| {
                        let delta = *x - (*value as i8 as f32) * node.scale;
                        delta * delta
                    })
                    .sum::<f32>()
                    .sqrt(),
            }
        } else {
            let bytes = &self.mmap[node.vector_pos..node.vector_pos + node.dim * 4];
            match self.metric {
                VectorMetric::Cosine => {
                    let dot = query
                        .iter()
                        .zip(bytes.chunks_exact(4))
                        .map(|(x, bytes)| {
                            *x * f32::from_le_bytes(bytes.try_into().expect("four bytes"))
                        })
                        .sum::<f32>();
                    1.0 - dot / (query_norm * node.norm).max(f32::EPSILON)
                }
                VectorMetric::Dot => {
                    let dot = query
                        .iter()
                        .zip(bytes.chunks_exact(4))
                        .map(|(x, bytes)| {
                            *x * f32::from_le_bytes(bytes.try_into().expect("four bytes"))
                        })
                        .sum::<f32>();
                    1.0 - dot
                }
                VectorMetric::L2 => query
                    .iter()
                    .zip(bytes.chunks_exact(4))
                    .map(|(x, bytes)| {
                        let value = f32::from_le_bytes(bytes.try_into().expect("four bytes"));
                        let delta = *x - value;
                        delta * delta
                    })
                    .sum::<f32>()
                    .sqrt(),
            }
        }
    }

    fn greedy_descend(
        &self,
        query: &[f32],
        norm: f32,
        mut entry: u32,
        mut distance: f32,
        layer: usize,
    ) -> (u32, f32) {
        loop {
            let mut improved = false;
            for neighbor in self.neighbors(entry, layer) {
                let candidate = self.dist_raw(query, norm, neighbor);
                if candidate < distance {
                    entry = neighbor;
                    distance = candidate;
                    improved = true;
                }
            }
            if !improved {
                return (entry, distance);
            }
        }
    }

    fn search_layer(
        &self,
        query: &[f32],
        norm: f32,
        entry: (f32, u32),
        ef: usize,
    ) -> Vec<(f32, u32)> {
        let mut visited: VisitedSet = HashSet::default();
        let mut candidates: BinaryHeap<Reverse<Cand>> = BinaryHeap::new();
        let mut results: BinaryHeap<Cand> = BinaryHeap::new();
        visited.insert(entry.1);
        candidates.push(Reverse(Cand(entry.0, entry.1)));
        results.push(Cand(entry.0, entry.1));
        while let Some(Reverse(Cand(distance, label))) = candidates.pop() {
            let worst = results
                .peek()
                .map(|candidate| candidate.0)
                .unwrap_or(f32::INFINITY);
            if distance > worst && results.len() >= ef {
                break;
            }
            for neighbor in self.neighbors(label, 0) {
                if !visited.insert(neighbor) {
                    continue;
                }
                let distance = self.dist_raw(query, norm, neighbor);
                let worst = results
                    .peek()
                    .map(|candidate| candidate.0)
                    .unwrap_or(f32::INFINITY);
                if results.len() < ef || distance < worst {
                    candidates.push(Reverse(Cand(distance, neighbor)));
                    results.push(Cand(distance, neighbor));
                    if results.len() > ef {
                        results.pop();
                    }
                }
            }
        }
        let mut output: Vec<_> = results
            .into_iter()
            .map(|Cand(distance, label)| (distance, label))
            .collect();
        output.sort_by(|left, right| left.0.total_cmp(&right.0));
        output
    }

    fn search(&self, query: &[f32], k: usize, ef: usize) -> Vec<(String, f32)> {
        let Some(mut entry) = self.entry else {
            return Vec::new();
        };
        let norm = query.iter().map(|value| value * value).sum::<f32>().sqrt();
        let mut distance = self.dist_raw(query, norm, entry);
        for layer in (1..=self.top_level as usize).rev() {
            (entry, distance) = self.greedy_descend(query, norm, entry, distance, layer);
        }
        let mut output = Vec::with_capacity(k);
        for (distance, label) in self.search_layer(query, norm, (distance, entry), ef.max(k)) {
            let label = label as usize;
            if self.deleted.get(label).copied().unwrap_or(true) {
                continue;
            }
            let node = self.node(label);
            if self.id_to_label.get(node.id) != Some(&label) {
                continue;
            }
            output.push((node.id.to_owned(), distance));
            if output.len() == k {
                break;
            }
        }
        output
    }
}

struct MappedNeighbors<'a> {
    bytes: &'a [u8],
    pos: usize,
}

impl Iterator for MappedNeighbors<'_> {
    type Item = u32;

    fn next(&mut self) -> Option<Self::Item> {
        let bytes = self.bytes.get(self.pos..self.pos + 4)?;
        self.pos += 4;
        Some(u32::from_le_bytes(bytes.try_into().expect("four bytes")))
    }
}

fn parse_mapped_node<'a>(
    mmap: &'a [u8],
    directory_offset: usize,
    node_count: usize,
    quantized: bool,
    label: usize,
) -> Result<MappedNode<'a>> {
    if label >= node_count {
        return Err(Error::Corrupt("vidx: node out of range".into()));
    }
    let start = usize::try_from(fixed_u64(mmap, directory_offset + label * 8)?)
        .map_err(|_| Error::Corrupt("vidx: node offset overflow".into()))?;
    let end = usize::try_from(fixed_u64(mmap, directory_offset + (label + 1) * 8)?)
        .map_err(|_| Error::Corrupt("vidx: node offset overflow".into()))?;
    if start >= end || start < MMAP_HEADER_FIXED || end > directory_offset {
        return Err(Error::Corrupt("vidx: invalid node bounds".into()));
    }
    let mut pos = start;
    let id_len = fixed_u16(mmap, pos)? as usize;
    pos += 2;
    let id_bytes = take_fixed(mmap, &mut pos, id_len, end)?;
    let id = std::str::from_utf8(id_bytes)
        .map_err(|_| Error::Corrupt("vidx: invalid id utf8".into()))?;
    let deleted = *take_fixed(mmap, &mut pos, 1, end)?
        .first()
        .expect("one byte")
        != 0;
    let norm = f32::from_le_bytes(
        take_fixed(mmap, &mut pos, 4, end)?
            .try_into()
            .expect("four bytes"),
    );
    let dim = fixed_u32(mmap, pos)? as usize;
    pos += 4;
    let scale = if quantized {
        let scale = f32::from_le_bytes(
            take_fixed(mmap, &mut pos, 4, end)?
                .try_into()
                .expect("four bytes"),
        );
        let vector_pos = pos;
        take_fixed(mmap, &mut pos, dim, end)?;
        (scale, vector_pos)
    } else {
        let vector_pos = pos;
        take_fixed(
            mmap,
            &mut pos,
            dim.checked_mul(4)
                .ok_or_else(|| Error::Corrupt("vidx: dimension overflow".into()))?,
            end,
        )?;
        (1.0, vector_pos)
    };
    let levels_pos = pos;
    let level_count = *take_fixed(mmap, &mut pos, 1, end)?
        .first()
        .expect("one byte");
    for _ in 0..level_count {
        let count = fixed_u32(mmap, pos)? as usize;
        pos += 4;
        let bytes = take_fixed(
            mmap,
            &mut pos,
            count
                .checked_mul(4)
                .ok_or_else(|| Error::Corrupt("vidx: link count overflow".into()))?,
            end,
        )?;
        for neighbor in bytes.chunks_exact(4) {
            if u32::from_le_bytes(neighbor.try_into().expect("four bytes")) as usize >= node_count {
                return Err(Error::Corrupt("vidx: neighbor out of range".into()));
            }
        }
    }
    if pos != end {
        return Err(Error::Corrupt("vidx: trailing node bytes".into()));
    }
    Ok(MappedNode {
        id,
        deleted,
        norm,
        dim,
        vector_pos: scale.1,
        scale: scale.0,
        levels_pos,
        level_count,
    })
}

struct VectorDumpWriter {
    writer: BufWriter<File>,
    crc: crc32fast::Hasher,
    position: u64,
}

impl VectorDumpWriter {
    fn new(file: File, header_len: usize) -> Result<Self> {
        let mut writer = BufWriter::with_capacity(1024 * 1024, file);
        writer.write_all(&vec![0; header_len])?;
        Ok(Self {
            writer,
            crc: crc32fast::Hasher::new(),
            position: header_len as u64,
        })
    }

    fn write(&mut self, bytes: &[u8]) -> Result<()> {
        self.writer.write_all(bytes)?;
        self.crc.update(bytes);
        self.position = self.position.saturating_add(bytes.len() as u64);
        Ok(())
    }

    fn position(&self) -> u64 {
        self.position
    }

    fn body_crc(&mut self) -> u32 {
        std::mem::replace(&mut self.crc, crc32fast::Hasher::new()).finalize()
    }

    fn finish(mut self, header: &[u8]) -> Result<()> {
        self.writer.seek(SeekFrom::Start(0))?;
        self.writer.write_all(header)?;
        self.writer.flush()?;
        self.writer.get_ref().sync_all()?;
        Ok(())
    }
}

#[cfg(target_endian = "little")]
fn write_f32_slice(writer: &mut VectorDumpWriter, values: &[f32]) -> Result<()> {
    // SAFETY: f32 has no invalid bit patterns and the target byte order is
    // exactly the file byte order. The resulting slice does not outlive input.
    let bytes = unsafe {
        std::slice::from_raw_parts(values.as_ptr().cast::<u8>(), std::mem::size_of_val(values))
    };
    writer.write(bytes)
}

#[cfg(target_endian = "big")]
fn write_f32_slice(writer: &mut VectorDumpWriter, values: &[f32]) -> Result<()> {
    for value in values {
        writer.write(&value.to_le_bytes())?;
    }
    Ok(())
}

#[cfg(target_endian = "little")]
fn write_u32_slice(writer: &mut VectorDumpWriter, values: &[u32]) -> Result<()> {
    // SAFETY: u32 has no invalid bit patterns and the target byte order is
    // exactly the file byte order. The resulting slice does not outlive input.
    let bytes = unsafe {
        std::slice::from_raw_parts(values.as_ptr().cast::<u8>(), std::mem::size_of_val(values))
    };
    writer.write(bytes)
}

#[cfg(target_endian = "big")]
fn write_u32_slice(writer: &mut VectorDumpWriter, values: &[u32]) -> Result<()> {
    for value in values {
        writer.write(&value.to_le_bytes())?;
    }
    Ok(())
}

fn fixed_u16(bytes: &[u8], offset: usize) -> Result<u16> {
    let bytes = bytes
        .get(offset..offset + 2)
        .ok_or_else(|| Error::Corrupt("vidx: unexpected end".into()))?;
    Ok(u16::from_le_bytes(bytes.try_into().expect("two bytes")))
}

fn fixed_u32(bytes: &[u8], offset: usize) -> Result<u32> {
    let bytes = bytes
        .get(offset..offset + 4)
        .ok_or_else(|| Error::Corrupt("vidx: unexpected end".into()))?;
    Ok(u32::from_le_bytes(bytes.try_into().expect("four bytes")))
}

fn fixed_u64(bytes: &[u8], offset: usize) -> Result<u64> {
    let bytes = bytes
        .get(offset..offset + 8)
        .ok_or_else(|| Error::Corrupt("vidx: unexpected end".into()))?;
    Ok(u64::from_le_bytes(bytes.try_into().expect("eight bytes")))
}

fn take_fixed<'a>(bytes: &'a [u8], pos: &mut usize, len: usize, limit: usize) -> Result<&'a [u8]> {
    let end = pos
        .checked_add(len)
        .filter(|end| *end <= limit)
        .ok_or_else(|| Error::Corrupt("vidx: unexpected end".into()))?;
    let value = &bytes[*pos..end];
    *pos = end;
    Ok(value)
}

// --- graph persistence -------------------------------------------------------
//
// File layout: 8-byte magic, u32 crc32 of the body, u64 body length, body.
// The body carries the index identity (table, column, metric, m,
// ef_construction) for validation, the commit version the dump reflects,
// and the full graph. Any validation failure means "rebuild from canonical
// data" — never an error surfaced to the user.

const VIDX_MAGIC: &[u8; 8] = b"ESQLVIDX";
const VIDX_FORMAT_V3: u32 = 3;
const VIDX_FORMAT_MMAP: u32 = 4;
const MAX_LEVELS: usize = 64;

fn read_str16(buf: &[u8], pos: &mut usize) -> Result<String> {
    let len = read_u16(buf, pos)? as usize;
    let end = pos
        .checked_add(len)
        .ok_or_else(|| Error::Corrupt("vidx: length overflow".into()))?;
    let slice = buf
        .get(*pos..end)
        .ok_or_else(|| Error::Corrupt("vidx: unexpected end".into()))?;
    *pos = end;
    std::str::from_utf8(slice)
        .map(|s| s.to_owned())
        .map_err(|_| Error::Corrupt("vidx: invalid utf8".into()))
}

fn metric_code(m: VectorMetric) -> u8 {
    match m {
        VectorMetric::Cosine => 0,
        VectorMetric::Dot => 1,
        VectorMetric::L2 => 2,
    }
}

impl VecIdx {
    /// Deserialize and validate a dump. Returns the index and the commit
    /// version it reflects. Every failure mode is `Corrupt`, which callers
    /// treat as "rebuild from canonical data".
    pub fn load_bytes(
        bytes: &[u8],
        table: &str,
        column: &str,
        def: &VectorIndexDef,
    ) -> Result<(VecIdx, u64)> {
        if bytes.len() < 20 || &bytes[..8] != VIDX_MAGIC {
            return Err(Error::Corrupt("vidx: bad magic".into()));
        }
        let stored_crc = u32::from_le_bytes(bytes[8..12].try_into().unwrap());
        let body_len = u64::from_le_bytes(bytes[12..20].try_into().unwrap()) as usize;
        let body = bytes
            .get(20..20 + body_len)
            .ok_or_else(|| Error::Corrupt("vidx: truncated body".into()))?;
        if crc32fast::hash(body) != stored_crc {
            return Err(Error::Corrupt("vidx: crc mismatch".into()));
        }
        let mut pos = 0usize;
        if read_u32(body, &mut pos)? != VIDX_FORMAT_V3 {
            return Err(Error::Corrupt("vidx: unsupported format".into()));
        }
        if read_str16(body, &mut pos)? != table || read_str16(body, &mut pos)? != column {
            return Err(Error::Corrupt("vidx: identity mismatch".into()));
        }
        let same_def = read_u8(body, &mut pos)? == metric_code(def.metric)
            && read_u32(body, &mut pos)? as usize == def.m
            && read_u32(body, &mut pos)? as usize == def.ef_construction
            && read_u8(body, &mut pos)? == def.quantized as u8;
        if !same_def {
            return Err(Error::Corrupt("vidx: index definition changed".into()));
        }
        let dump_version = read_u64(body, &mut pos)?;
        let top_level = read_u8(body, &mut pos)?;
        if top_level as usize >= MAX_LEVELS {
            return Err(Error::Corrupt("vidx: implausible top level".into()));
        }
        let entry_raw = read_u32(body, &mut pos)?;
        let rng = read_u64(body, &mut pos)?;
        let n = read_u32(body, &mut pos)? as usize;
        if n as u64 > body.len() as u64 {
            return Err(Error::Corrupt("vidx: implausible node count".into()));
        }

        let mut labels: Vec<Arc<str>> = Vec::with_capacity(n);
        let mut deleted: Vec<bool> = Vec::with_capacity(n);
        let mut store = if def.quantized {
            VecStore::I8 {
                dim: 0,
                values: Vec::new(),
                scales: Vec::with_capacity(n),
            }
        } else {
            VecStore::F32 {
                dim: 0,
                values: Vec::new(),
            }
        };
        let mut norms = Vec::with_capacity(n);
        let mut links: Vec<Vec<Vec<u32>>> = Vec::with_capacity(n);
        for _ in 0..n {
            labels.push(Arc::from(read_str16(body, &mut pos)?));
            deleted.push(read_u8(body, &mut pos)? != 0);
            let dim = read_u32(body, &mut pos)? as usize;
            match &mut store {
                VecStore::F32 {
                    dim: stored_dim,
                    values,
                } => {
                    if dim.checked_mul(4).is_none_or(|b| pos + b > body.len()) {
                        return Err(Error::Corrupt("vidx: truncated vector".into()));
                    }
                    if *stored_dim == 0 {
                        *stored_dim = dim;
                        values.reserve(n.saturating_mul(dim));
                    } else if *stored_dim != dim {
                        return Err(Error::Corrupt("vidx: inconsistent dimensions".into()));
                    }
                    let start = values.len();
                    for _ in 0..dim {
                        values.push(f32::from_le_bytes(body[pos..pos + 4].try_into().unwrap()));
                        pos += 4;
                    }
                    norms.push(values[start..].iter().map(|x| x * x).sum::<f32>().sqrt());
                }
                VecStore::I8 {
                    dim: stored_dim,
                    values,
                    scales,
                } => {
                    if dim.checked_add(4).is_none_or(|b| pos + b > body.len()) {
                        return Err(Error::Corrupt("vidx: truncated vector".into()));
                    }
                    if *stored_dim == 0 {
                        *stored_dim = dim;
                        values.reserve(n.saturating_mul(dim));
                    } else if *stored_dim != dim {
                        return Err(Error::Corrupt("vidx: inconsistent dimensions".into()));
                    }
                    let scale = f32::from_le_bytes(body[pos..pos + 4].try_into().unwrap());
                    pos += 4;
                    let start = values.len();
                    values.extend(body[pos..pos + dim].iter().map(|&b| b as i8));
                    pos += dim;
                    let sumsq: i64 = values[start..].iter().map(|&b| b as i64 * b as i64).sum();
                    norms.push(scale * (sumsq as f32).sqrt());
                    scales.push(scale);
                }
            }
            let level_count = read_u8(body, &mut pos)? as usize;
            if level_count == 0 || level_count > MAX_LEVELS {
                return Err(Error::Corrupt("vidx: bad level count".into()));
            }
            let mut node_links = Vec::with_capacity(level_count);
            for _ in 0..level_count {
                let cnt = read_u32(body, &mut pos)? as usize;
                if cnt.checked_mul(4).is_none_or(|b| pos + b > body.len()) {
                    return Err(Error::Corrupt("vidx: truncated links".into()));
                }
                let mut level = Vec::with_capacity(cnt);
                for _ in 0..cnt {
                    let neighbor = read_u32(body, &mut pos)?;
                    if neighbor as usize >= n {
                        return Err(Error::Corrupt("vidx: neighbor out of range".into()));
                    }
                    level.push(neighbor);
                }
                node_links.push(level);
            }
            links.push(node_links);
        }

        // Structural invariants that keep search panic-free: the entry point
        // must reach top_level, and any node listed at layer L must have L.
        let entry = if entry_raw == u32::MAX {
            None
        } else {
            let e = entry_raw as usize;
            if e >= n || links[e].len() <= top_level as usize {
                return Err(Error::Corrupt("vidx: invalid entry point".into()));
            }
            Some(entry_raw)
        };
        for node_links in &links {
            for (layer, level) in node_links.iter().enumerate() {
                for &nb in level {
                    if links[nb as usize].len() <= layer {
                        return Err(Error::Corrupt("vidx: neighbor below its layer".into()));
                    }
                }
            }
        }

        let mut id_to_label = HashMap::with_capacity(n);
        for (i, id) in labels.iter().enumerate() {
            if !deleted[i] {
                id_to_label.insert(id.clone(), i);
            }
        }

        let m = def.m.clamp(2, 256);
        let backend = HnswIndex {
            metric: def.metric,
            m,
            m0: m * 2,
            ef_construction: def.ef_construction.max(m),
            ml: 1.0 / (m as f64).ln(),
            store,
            norms,
            links,
            entry,
            top_level,
            rng,
        };
        Ok((
            VecIdx {
                mapped: Vec::new(),
                backend,
                labels,
                id_to_label,
                deleted,
            },
            dump_version,
        ))
    }
}
