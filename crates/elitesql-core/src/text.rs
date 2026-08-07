//! Basic full-text search (Phase 5): an in-memory inverted index with BM25
//! ranking over `text` columns. Like every derived structure in EliteSQL, it
//! tracks the latest committed state, is maintained at commit, and is
//! rebuilt from canonical data on open and compaction.
//!
//! Tokenizer (V1): Unicode alphanumeric runs, lowercased, 2..=64 chars.
//! No stemming, no stop words — deliberately predictable.

use std::collections::HashMap;

use serde::{Deserialize, Serialize};

/// Persisted definition of a full-text index (stored in the catalog).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TextIndexDef {
    pub column: String,
}

/// One full-text hit; higher `score` is better (BM25).
#[derive(Debug, Clone)]
pub struct TextHit {
    pub id: String,
    pub score: f32,
    pub record: crate::Record,
}

const BM25_K1: f32 = 1.2;
const BM25_B: f32 = 0.75;

pub(crate) fn tokenize(text: &str) -> Vec<String> {
    text.split(|c: char| !c.is_alphanumeric())
        .filter(|t| t.len() >= 2 && t.len() <= 64)
        .map(|t| t.to_lowercase())
        .collect()
}

/// Inverted index over one (table, column).
pub(crate) struct TextIdx {
    /// term -> (record id -> term frequency).
    postings: HashMap<String, HashMap<String, u32>>,
    /// record id -> document length in tokens.
    doc_len: HashMap<String, u32>,
    total_len: u64,
}

impl TextIdx {
    pub fn new() -> TextIdx {
        TextIdx {
            postings: HashMap::new(),
            doc_len: HashMap::new(),
            total_len: 0,
        }
    }

    /// Index (or re-index) a document. `remove` must be called first when
    /// replacing existing content.
    pub fn add(&mut self, id: &str, text: &str) {
        let tokens = tokenize(text);
        if tokens.is_empty() {
            return;
        }
        self.total_len += tokens.len() as u64;
        self.doc_len.insert(id.to_owned(), tokens.len() as u32);
        for token in tokens {
            *self
                .postings
                .entry(token)
                .or_default()
                .entry(id.to_owned())
                .or_insert(0) += 1;
        }
    }

    /// Un-index a document given its previous content.
    pub fn remove(&mut self, id: &str, old_text: &str) {
        let Some(len) = self.doc_len.remove(id) else { return };
        self.total_len = self.total_len.saturating_sub(len as u64);
        for token in tokenize(old_text) {
            if let Some(ids) = self.postings.get_mut(&token) {
                ids.remove(id);
                if ids.is_empty() {
                    self.postings.remove(&token);
                }
            }
        }
    }

    /// BM25-ranked ids for a query, best first. Scores every posting of
    /// every query term; the caller filters and truncates.
    pub fn search(&self, query: &str) -> Vec<(String, f32)> {
        let n = self.doc_len.len() as f32;
        if n == 0.0 {
            return Vec::new();
        }
        let avg_len = self.total_len as f32 / n;
        let mut terms = tokenize(query);
        terms.sort_unstable();
        terms.dedup();

        let mut scores: HashMap<&str, f32> = HashMap::new();
        for term in &terms {
            let Some(ids) = self.postings.get(term) else { continue };
            let df = ids.len() as f32;
            let idf = (1.0 + (n - df + 0.5) / (df + 0.5)).ln();
            for (id, &tf) in ids {
                let tf = tf as f32;
                let dl = *self.doc_len.get(id).unwrap_or(&1) as f32;
                let norm = tf * (BM25_K1 + 1.0)
                    / (tf + BM25_K1 * (1.0 - BM25_B + BM25_B * dl / avg_len));
                *scores.entry(id.as_str()).or_insert(0.0) += idf * norm;
            }
        }
        let mut ranked: Vec<(String, f32)> =
            scores.into_iter().map(|(id, s)| (id.to_owned(), s)).collect();
        ranked.sort_by(|a, b| b.1.total_cmp(&a.1).then_with(|| a.0.cmp(&b.0)));
        ranked
    }
}
