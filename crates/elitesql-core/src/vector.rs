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
    F32(Vec<Vec<f32>>),
    I8(Vec<(Vec<i8>, f32)>),
}

impl VecStore {
    fn len(&self) -> usize {
        match self {
            VecStore::F32(v) => v.len(),
            VecStore::I8(v) => v.len(),
        }
    }

    /// Append, returning the stored vector's norm (of what was stored,
    /// i.e. post-quantization, so cosine stays self-consistent).
    fn push(&mut self, v: &[f32]) -> f32 {
        match self {
            VecStore::F32(store) => {
                store.push(v.to_vec());
                v.iter().map(|x| x * x).sum::<f32>().sqrt()
            }
            VecStore::I8(store) => {
                let max_abs = v.iter().fold(0.0f32, |a, x| a.max(x.abs()));
                let scale = if max_abs == 0.0 { 1.0 } else { max_abs / 127.0 };
                let q: Vec<i8> = v
                    .iter()
                    .map(|x| (x / scale).round().clamp(-127.0, 127.0) as i8)
                    .collect();
                let sumsq: i64 = q.iter().map(|&b| b as i64 * b as i64).sum();
                store.push((q, scale));
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
                VecStore::I8(Vec::new())
            } else {
                VecStore::F32(Vec::new())
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
            VecStore::F32(vs) => self.dist_raw(&vs[a as usize], self.norms[a as usize], b),
            VecStore::I8(vs) => {
                let (qa, sa) = &vs[a as usize];
                let (qb, sb) = &vs[b as usize];
                match self.metric {
                    VectorMetric::Cosine => {
                        let dot_i: i64 = qa
                            .iter()
                            .zip(qb)
                            .map(|(&x, &y)| x as i64 * y as i64)
                            .sum();
                        let dot = dot_i as f32 * sa * sb;
                        1.0 - dot
                            / (self.norms[a as usize] * self.norms[b as usize]).max(f32::EPSILON)
                    }
                    VectorMetric::Dot => {
                        let dot_i: i64 = qa
                            .iter()
                            .zip(qb)
                            .map(|(&x, &y)| x as i64 * y as i64)
                            .sum();
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
            VecStore::F32(vs) => {
                let w = &vs[label as usize];
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
            VecStore::I8(vs) => {
                let (q, scale) = &vs[label as usize];
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
        let label = self.store.len() as u32;
        let level = self.random_level();
        let stored_norm = self.store.push(v);
        // Query-side norm stays the exact f32 norm; the stored norm reflects
        // what the index will compare against.
        let norm = v.iter().map(|x| x * x).sum::<f32>().sqrt();
        self.norms.push(stored_norm);
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
            backend: HnswIndex::new(def.metric, def.m, def.ef_construction, def.quantized),
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

    /// Ids currently indexed (latest labels only).
    pub fn ids(&self) -> Vec<String> {
        self.id_to_label.keys().cloned().collect()
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

// --- graph persistence -------------------------------------------------------
//
// File layout: 8-byte magic, u32 crc32 of the body, u64 body length, body.
// The body carries the index identity (table, column, metric, m,
// ef_construction) for validation, the commit version the dump reflects,
// and the full graph. Any validation failure means "rebuild from canonical
// data" — never an error surfaced to the user.

const VIDX_MAGIC: &[u8; 8] = b"ESQLVIDX";
const VIDX_FORMAT: u32 = 3;
const MAX_LEVELS: usize = 64;

fn write_str16(buf: &mut Vec<u8>, s: &str) {
    buf.extend_from_slice(&(s.len() as u16).to_le_bytes());
    buf.extend_from_slice(s.as_bytes());
}

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
    /// Serialize the full index state as of `dump_version`.
    pub fn dump_bytes(
        &self,
        table: &str,
        column: &str,
        def: &VectorIndexDef,
        dump_version: u64,
    ) -> Vec<u8> {
        let b = &self.backend;
        let n = b.store.len();
        let mut body = Vec::with_capacity(64 + n * 64);
        body.extend_from_slice(&VIDX_FORMAT.to_le_bytes());
        write_str16(&mut body, table);
        write_str16(&mut body, column);
        body.push(metric_code(def.metric));
        body.extend_from_slice(&(def.m as u32).to_le_bytes());
        body.extend_from_slice(&(def.ef_construction as u32).to_le_bytes());
        body.push(def.quantized as u8);
        body.extend_from_slice(&dump_version.to_le_bytes());
        body.push(b.top_level);
        body.extend_from_slice(&b.entry.unwrap_or(u32::MAX).to_le_bytes());
        body.extend_from_slice(&b.rng.to_le_bytes());
        body.extend_from_slice(&(n as u32).to_le_bytes());
        for i in 0..n {
            write_str16(&mut body, &self.labels[i]);
            body.push(self.deleted.contains(&i) as u8);
            match &b.store {
                VecStore::F32(vs) => {
                    let v = &vs[i];
                    body.extend_from_slice(&(v.len() as u32).to_le_bytes());
                    for x in v {
                        body.extend_from_slice(&x.to_le_bytes());
                    }
                }
                VecStore::I8(vs) => {
                    let (q, scale) = &vs[i];
                    body.extend_from_slice(&(q.len() as u32).to_le_bytes());
                    body.extend_from_slice(&scale.to_le_bytes());
                    body.extend(q.iter().map(|&b| b as u8));
                }
            }
            let levels = &b.links[i];
            body.push(levels.len() as u8);
            for level in levels {
                body.extend_from_slice(&(level.len() as u32).to_le_bytes());
                for &n in level {
                    body.extend_from_slice(&n.to_le_bytes());
                }
            }
        }
        let mut out = Vec::with_capacity(body.len() + 20);
        out.extend_from_slice(VIDX_MAGIC);
        out.extend_from_slice(&crc32fast::hash(&body).to_le_bytes());
        out.extend_from_slice(&(body.len() as u64).to_le_bytes());
        out.extend_from_slice(&body);
        out
    }

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
        if read_u32(body, &mut pos)? != VIDX_FORMAT {
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

        let mut labels = Vec::with_capacity(n);
        let mut deleted: HashSet<usize> = HashSet::new();
        let mut store = if def.quantized {
            VecStore::I8(Vec::with_capacity(n))
        } else {
            VecStore::F32(Vec::with_capacity(n))
        };
        let mut norms = Vec::with_capacity(n);
        let mut links: Vec<Vec<Vec<u32>>> = Vec::with_capacity(n);
        for i in 0..n {
            labels.push(read_str16(body, &mut pos)?);
            if read_u8(body, &mut pos)? != 0 {
                deleted.insert(i);
            }
            let dim = read_u32(body, &mut pos)? as usize;
            match &mut store {
                VecStore::F32(vs) => {
                    if dim.checked_mul(4).is_none_or(|b| pos + b > body.len()) {
                        return Err(Error::Corrupt("vidx: truncated vector".into()));
                    }
                    let mut v = Vec::with_capacity(dim);
                    for _ in 0..dim {
                        v.push(f32::from_le_bytes(body[pos..pos + 4].try_into().unwrap()));
                        pos += 4;
                    }
                    norms.push(v.iter().map(|x| x * x).sum::<f32>().sqrt());
                    vs.push(v);
                }
                VecStore::I8(vs) => {
                    if dim.checked_add(4).is_none_or(|b| pos + b > body.len()) {
                        return Err(Error::Corrupt("vidx: truncated vector".into()));
                    }
                    let scale = f32::from_le_bytes(body[pos..pos + 4].try_into().unwrap());
                    pos += 4;
                    let q: Vec<i8> = body[pos..pos + dim].iter().map(|&b| b as i8).collect();
                    pos += dim;
                    let sumsq: i64 = q.iter().map(|&b| b as i64 * b as i64).sum();
                    norms.push(scale * (sumsq as f32).sqrt());
                    vs.push((q, scale));
                }
            }
            let level_count = read_u8(body, &mut pos)? as usize;
            if level_count == 0 || level_count > MAX_LEVELS {
                return Err(Error::Corrupt("vidx: bad level count".into()));
            }
            let mut node_links = Vec::with_capacity(level_count);
            for _ in 0..level_count {
                let cnt = read_u32(body, &mut pos)? as usize;
                if cnt
                    .checked_mul(4)
                    .is_none_or(|b| pos + b > body.len())
                {
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
            if !deleted.contains(&i) {
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
                backend,
                labels,
                id_to_label,
                deleted,
            },
            dump_version,
        ))
    }
}
