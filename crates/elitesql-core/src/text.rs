//! BM25 full-text indexing with an immutable mmap-backed base plus a small
//! mutable delta. Data pages are shared with the other derived indexes.

use std::cmp::Ordering;
use std::collections::{btree_map, BTreeMap, BTreeSet, BinaryHeap, HashMap, HashSet};
use std::path::Path;
use std::sync::Arc;

use serde::{Deserialize, Serialize};

use crate::error::{Error, Result};
use crate::paged::{ExternalPagedWriter, PagedIndex, PagedPrefixCursor};
use crate::run_manifest::DerivedRunMeta;

/// Persisted definition of a full-text index (stored in the catalog).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
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
const FORMAT_KEY: &[u8] = &[0];
const FORMAT_VALUE: &[u8] = b"ESQLTID2";
const DOC_TAG: u8 = 1;
const POSTING_TAG: u8 = 2;
const DELETE: u8 = 0;
const ADD: u8 = 1;

pub(crate) fn tokenize(text: &str) -> Vec<String> {
    text.split(|c: char| !c.is_alphanumeric())
        .filter(|t| t.len() >= 2 && t.len() <= 64)
        .map(|t| t.to_lowercase())
        .collect()
}

/// Inverted index over one (table, column).
pub(crate) struct TextIdx {
    pub(crate) generation: u64,
    pub(crate) runs: Vec<TextRun>,
    /// Mutable postings since the immutable base was published.
    postings: HashMap<String, BTreeMap<String, u32>>,
    doc_len: HashMap<String, u32>,
    delta_total_len: u64,
    /// Base documents hidden by updates/deletes.
    removed: HashSet<String>,
    removed_postings: HashMap<String, BTreeSet<String>>,
    removed_total_len: u64,
    persisted_doc_count: u64,
    persisted_total_len: u64,
}

pub(crate) struct TextRun {
    pub(crate) meta: DerivedRunMeta,
    pub(crate) index: Arc<PagedIndex>,
}

impl TextIdx {
    pub fn new() -> TextIdx {
        TextIdx {
            generation: 0,
            runs: Vec::new(),
            postings: HashMap::new(),
            doc_len: HashMap::new(),
            delta_total_len: 0,
            removed: HashSet::new(),
            removed_postings: HashMap::new(),
            removed_total_len: 0,
            persisted_doc_count: 0,
            persisted_total_len: 0,
        }
    }

    pub(crate) fn paged_runs(
        generation: u64,
        runs: Vec<TextRun>,
        persisted_doc_count: u64,
        persisted_total_len: u64,
    ) -> Result<Self> {
        for run in &runs {
            validate_run(&run.index)?;
        }
        Ok(Self {
            generation,
            runs,
            postings: HashMap::new(),
            doc_len: HashMap::new(),
            delta_total_len: 0,
            removed: HashSet::new(),
            removed_postings: HashMap::new(),
            removed_total_len: 0,
            persisted_doc_count,
            persisted_total_len,
        })
    }

    pub(crate) fn run_metas(&self) -> Vec<DerivedRunMeta> {
        self.runs.iter().map(|run| run.meta.clone()).collect()
    }

    pub(crate) fn delta_memory_bytes(&self) -> usize {
        let postings = self
            .postings
            .iter()
            .map(|(term, ids)| term.len() + 96 + ids.keys().map(|id| id.len() + 56).sum::<usize>())
            .sum::<usize>();
        let lengths = self.doc_len.keys().map(|id| id.len() + 48).sum::<usize>();
        let removed = self.removed.iter().map(|id| id.len() + 48).sum::<usize>();
        let removed_postings = self
            .removed_postings
            .iter()
            .map(|(term, ids)| term.len() + 96 + ids.iter().map(|id| id.len() + 48).sum::<usize>())
            .sum::<usize>();
        postings
            .saturating_add(lengths)
            .saturating_add(removed)
            .saturating_add(removed_postings)
    }

    /// Index (or re-index) a document. `remove` must be called first when
    /// replacing existing content.
    pub fn add(&mut self, id: &str, text: &str) {
        let tokens = tokenize(text);
        if tokens.is_empty() {
            return;
        }
        self.delta_total_len += tokens.len() as u64;
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
        if let Some(len) = self.doc_len.remove(id) {
            self.delta_total_len = self.delta_total_len.saturating_sub(len as u64);
            for token in tokenize(old_text) {
                if let Some(ids) = self.postings.get_mut(&token) {
                    ids.remove(id);
                    if ids.is_empty() {
                        self.postings.remove(&token);
                    }
                }
            }
            return;
        }
        let len = tokenize(old_text).len() as u64;
        if !self.runs.is_empty() && len > 0 && self.removed.insert(id.to_owned()) {
            self.removed_total_len += len;
            let mut terms = tokenize(old_text);
            terms.sort_unstable();
            terms.dedup();
            for term in terms {
                self.removed_postings
                    .entry(term)
                    .or_default()
                    .insert(id.to_owned());
            }
        }
    }

    /// Exact BM25 top-k with memory proportional to query terms plus `limit`,
    /// not to the number of matching documents. Posting streams are merged by
    /// document id so each score can be finalized and discarded immediately.
    pub fn search_top_k(
        &self,
        query: &str,
        limit: usize,
        mut accept: impl FnMut(&str) -> Result<bool>,
    ) -> Result<Vec<(String, f32)>> {
        if limit == 0 {
            return Ok(Vec::new());
        }
        let n = self.doc_count() as f32;
        if n == 0.0 {
            return Ok(Vec::new());
        }
        let avg_len = self.total_len() as f32 / n;
        let mut terms = tokenize(query);
        terms.sort_unstable();
        terms.dedup();

        let mut streams = Vec::new();
        for term in &terms {
            let mut counter = TermStream::new(self, term)?;
            let mut df = 0usize;
            while counter.next()?.is_some() {
                df += 1;
            }
            let df = df as f32;
            if df == 0.0 {
                continue;
            }
            let idf = (1.0 + (n - df + 0.5) / (df + 0.5)).ln();
            let mut stream = TermStream::new(self, term)?;
            let head = stream.next()?;
            streams.push((idf, stream, head));
        }

        let mut best = BinaryHeap::with_capacity(limit.saturating_add(1));
        loop {
            let Some(id) = streams
                .iter()
                .filter_map(|(_, _, head)| head.as_ref().map(|posting| posting.id.as_str()))
                .min()
                .map(str::to_owned)
            else {
                break;
            };
            let mut score = 0.0f32;
            for (idf, stream, head) in &mut streams {
                if head.as_ref().is_some_and(|posting| posting.id == id) {
                    let posting = head.take().expect("matching head");
                    score += *idf * bm25_norm(posting.tf, posting.dl, avg_len);
                    *head = stream.next()?;
                }
            }
            if !accept(&id)? {
                continue;
            }
            let candidate = Ranked { id, score };
            if best.len() < limit {
                best.push(candidate);
            } else if best
                .peek()
                .is_some_and(|worst| candidate.better_than(worst))
            {
                best.pop();
                best.push(candidate);
            }
        }
        let mut ranked: Vec<(String, f32)> = best
            .into_iter()
            .map(|candidate| (candidate.id, candidate.score))
            .collect();
        ranked.sort_by(|a, b| b.1.total_cmp(&a.1).then_with(|| a.0.cmp(&b.0)));
        Ok(ranked)
    }

    pub(crate) fn write_delta_paged(
        &self,
        target: &Path,
        temp_dir: &Path,
        dump_version: u64,
        budget: usize,
    ) -> Result<()> {
        let mut writer = ExternalPagedWriter::new(target, temp_dir, dump_version, budget)?;
        write_format(&mut writer)?;
        for (id, &dl) in &self.doc_len {
            writer.add(&doc_key(id), &document_value(dump_version, ADD, dl))?;
        }
        for id in &self.removed {
            writer.add(&doc_key(id), &document_value(dump_version, DELETE, 0))?;
        }
        for (term, ids) in &self.postings {
            for (id, &tf) in ids {
                let dl = *self.doc_len.get(id).unwrap_or(&1);
                writer.add(
                    &posting_key(term, id),
                    &posting_value(dump_version, ADD, tf, dl),
                )?;
            }
        }
        for (term, ids) in &self.removed_postings {
            for id in ids {
                writer.add(
                    &posting_key(term, id),
                    &posting_value(dump_version, DELETE, 0, 0),
                )?;
            }
        }
        writer.finish()
    }

    pub(crate) fn freeze_delta(&mut self, generation: u64) {
        self.persisted_doc_count = self.doc_count();
        self.persisted_total_len = self.total_len();
        self.generation = generation;
        self.postings.clear();
        self.doc_len.clear();
        self.delta_total_len = 0;
        self.removed.clear();
        self.removed_postings.clear();
        self.removed_total_len = 0;
    }

    pub(crate) fn write_document(
        writer: &mut ExternalPagedWriter,
        id: &str,
        text: &str,
        version: u64,
    ) -> Result<Option<u32>> {
        let tokens = tokenize(text);
        if tokens.is_empty() {
            return Ok(None);
        }
        let dl = tokens.len() as u32;
        let mut frequencies = HashMap::<String, u32>::new();
        for token in tokens {
            *frequencies.entry(token).or_insert(0) += 1;
        }
        for (term, tf) in frequencies {
            writer.add(
                &posting_key(&term, id),
                &posting_value(version, ADD, tf, dl),
            )?;
        }
        writer.add(&doc_key(id), &document_value(version, ADD, dl))?;
        Ok(Some(dl))
    }

    pub(crate) fn write_format(writer: &mut ExternalPagedWriter) -> Result<()> {
        write_format(writer)
    }

    pub(crate) fn doc_stats(&self) -> (u64, u64) {
        (self.doc_count(), self.total_len())
    }

    fn doc_count(&self) -> u64 {
        self.persisted_doc_count
            .saturating_sub(self.removed.len() as u64)
            .saturating_add(self.doc_len.len() as u64)
    }

    fn total_len(&self) -> u64 {
        self.persisted_total_len
            .saturating_sub(self.removed_total_len)
            .saturating_add(self.delta_total_len)
    }
}

struct Posting {
    id: String,
    tf: u32,
    dl: u32,
}

struct TermStream<'a> {
    index: &'a TextIdx,
    persisted: Vec<TextPostingCursor<'a>>,
    delta: Option<btree_map::Iter<'a, String, u32>>,
    delta_head: Option<Posting>,
}

struct TextPostingCursor<'a> {
    cursor: PagedPrefixCursor<'a>,
    prefix: Vec<u8>,
    head: Option<(String, u64, u8, u32, u32)>,
}

impl<'a> TextPostingCursor<'a> {
    fn new(index: &'a PagedIndex, term: &str) -> Result<Self> {
        let prefix = posting_prefix(term);
        let mut cursor = Self {
            cursor: index.prefix_cursor(&prefix),
            prefix,
            head: None,
        };
        cursor.advance()?;
        Ok(cursor)
    }

    fn advance(&mut self) -> Result<()> {
        self.head = None;
        let Some((key, value)) = self.cursor.next()? else {
            return Ok(());
        };
        let id = key
            .strip_prefix(self.prefix.as_slice())
            .ok_or_else(|| Error::Corrupt("text index: invalid posting prefix".into()))?;
        let id = std::str::from_utf8(id)
            .map_err(|_| Error::Corrupt("text index: invalid id utf8".into()))?;
        let (version, operation, tf, dl) = parse_posting_value(value)?;
        self.head = Some((id.to_owned(), version, operation, tf, dl));
        Ok(())
    }
}

impl<'a> TermStream<'a> {
    fn new(index: &'a TextIdx, term: &str) -> Result<Self> {
        let mut stream = Self {
            index,
            persisted: index
                .runs
                .iter()
                .filter(|run| run.index.may_contain_prefix(&posting_prefix(term)))
                .map(|run| TextPostingCursor::new(&run.index, term))
                .collect::<Result<Vec<_>>>()?,
            delta: index.postings.get(term).map(BTreeMap::iter),
            delta_head: None,
        };
        stream.advance_delta();
        Ok(stream)
    }

    fn next(&mut self) -> Result<Option<Posting>> {
        loop {
            let next_persisted = self
                .persisted
                .iter()
                .filter_map(|cursor| cursor.head.as_ref().map(|head| head.0.as_str()))
                .min();
            let next_delta = self.delta_head.as_ref().map(|posting| posting.id.as_str());
            let Some(id) = next_persisted
                .into_iter()
                .chain(next_delta)
                .min()
                .map(str::to_owned)
            else {
                return Ok(None);
            };
            let mut newest: Option<(u64, u8, u32, u32)> = None;
            for cursor in &mut self.persisted {
                while cursor.head.as_ref().is_some_and(|head| head.0 == id) {
                    let (_, version, operation, tf, dl) =
                        cursor.head.take().expect("matching posting head");
                    if newest.is_none_or(|current| (version, operation, tf, dl) > current) {
                        newest = Some((version, operation, tf, dl));
                    }
                    cursor.advance()?;
                }
            }
            if self
                .delta_head
                .as_ref()
                .is_some_and(|posting| posting.id == id)
            {
                let posting = self.delta_head.take().expect("matching delta posting");
                newest = Some((u64::MAX, ADD, posting.tf, posting.dl));
                self.advance_delta();
            } else if self.index.removed.contains(&id) {
                newest = Some((u64::MAX, DELETE, 0, 0));
            }
            let Some((_, operation, tf, dl)) = newest else {
                continue;
            };
            if operation == ADD {
                return Ok(Some(Posting { id, tf, dl }));
            }
        }
    }

    fn advance_delta(&mut self) {
        self.delta_head = self
            .delta
            .as_mut()
            .and_then(Iterator::next)
            .map(|(id, &tf)| Posting {
                id: id.clone(),
                tf,
                dl: *self.index.doc_len.get(id).unwrap_or(&1),
            });
    }
}

struct Ranked {
    id: String,
    score: f32,
}

impl Ranked {
    fn better_than(&self, other: &Self) -> bool {
        self.score > other.score || (self.score == other.score && self.id < other.id)
    }
}

impl PartialEq for Ranked {
    fn eq(&self, other: &Self) -> bool {
        self.score.to_bits() == other.score.to_bits() && self.id == other.id
    }
}

impl Eq for Ranked {}

impl PartialOrd for Ranked {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}

impl Ord for Ranked {
    fn cmp(&self, other: &Self) -> Ordering {
        other
            .score
            .total_cmp(&self.score)
            .then_with(|| self.id.cmp(&other.id))
    }
}

fn bm25_norm(tf: u32, dl: u32, avg_len: f32) -> f32 {
    let tf = tf as f32;
    let dl = dl as f32;
    tf * (BM25_K1 + 1.0) / (tf + BM25_K1 * (1.0 - BM25_B + BM25_B * dl / avg_len))
}

fn posting_prefix(term: &str) -> Vec<u8> {
    let mut key = Vec::with_capacity(5 + term.len());
    key.push(POSTING_TAG);
    key.extend_from_slice(&(term.len() as u32).to_be_bytes());
    key.extend_from_slice(term.as_bytes());
    key
}

fn posting_key(term: &str, id: &str) -> Vec<u8> {
    let mut key = posting_prefix(term);
    key.extend_from_slice(id.as_bytes());
    key
}

fn doc_key(id: &str) -> Vec<u8> {
    let mut key = Vec::with_capacity(1 + id.len());
    key.push(DOC_TAG);
    key.extend_from_slice(id.as_bytes());
    key
}

fn posting_value(version: u64, operation: u8, tf: u32, dl: u32) -> [u8; 17] {
    let mut value = [0; 17];
    value[..8].copy_from_slice(&version.to_be_bytes());
    value[8] = operation;
    value[9..13].copy_from_slice(&tf.to_be_bytes());
    value[13..].copy_from_slice(&dl.to_be_bytes());
    value
}

fn parse_posting_value(value: &[u8]) -> Result<(u64, u8, u32, u32)> {
    if value.len() != 17 || !matches!(value[8], DELETE | ADD) {
        return Err(Error::Corrupt("text index: invalid posting value".into()));
    }
    Ok((
        u64::from_be_bytes(value[..8].try_into().expect("eight bytes")),
        value[8],
        u32::from_be_bytes(value[9..13].try_into().expect("four bytes")),
        u32::from_be_bytes(value[13..].try_into().expect("four bytes")),
    ))
}

fn document_value(version: u64, operation: u8, dl: u32) -> [u8; 13] {
    let mut value = [0; 13];
    value[..8].copy_from_slice(&version.to_be_bytes());
    value[8] = operation;
    value[9..].copy_from_slice(&dl.to_be_bytes());
    value
}

fn write_format(writer: &mut ExternalPagedWriter) -> Result<()> {
    writer.add(FORMAT_KEY, FORMAT_VALUE)
}

pub(crate) fn validate_run(index: &PagedIndex) -> Result<()> {
    let mut valid = false;
    index.visit_key(FORMAT_KEY, |value| {
        valid = value == FORMAT_VALUE;
        Ok(false)
    })?;
    if valid {
        Ok(())
    } else {
        Err(Error::Corrupt("text index: unsupported run format".into()))
    }
}
