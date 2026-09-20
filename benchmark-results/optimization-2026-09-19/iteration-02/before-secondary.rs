//! Secondary-index internals, separated from database state and lifecycle code.
use super::*;

pub(super) const SECONDARY_FORMAT_KEY: &[u8] = &[0];
// Bumped when the key encoding changes: a run written under an older marker
// fails to load and the loader rebuilds it from canonical data.
pub(super) const SECONDARY_FORMAT_VALUE: &[u8] = b"ESQLSID4";
pub(super) const SECONDARY_ENTRY_TAG: u8 = 1;
pub(super) const SECONDARY_DELETE: u8 = 0;
pub(super) const SECONDARY_ADD: u8 = 1;

pub(super) struct SecRun {
    pub(super) meta: DerivedRunMeta,
    pub(super) index: Arc<PagedIndex>,
}

pub(super) struct SecIdx {
    pub(super) generation: u64,
    pub(super) runs: Vec<SecRun>,
    /// Final additions since the last immutable run was published.
    pub(super) delta: BTreeMap<Vec<u8>, BTreeSet<String>>,
    /// Final removals since publication. Tombstones are required even when
    /// the matching add lives in a non-base level.
    pub(super) removed: BTreeMap<Vec<u8>, BTreeSet<String>>,
    /// Immutable in-memory overlay being written by background maintenance.
    /// New commits land in `delta`/`removed` and therefore never mutate the
    /// generation owned by the worker.
    pub(super) frozen: Option<Arc<FrozenSecDelta>>,
}

pub(super) struct FrozenSecDelta {
    pub(super) generation: u64,
    pub(super) delta: BTreeMap<Vec<u8>, BTreeSet<String>>,
    pub(super) removed: BTreeMap<Vec<u8>, BTreeSet<String>>,
}

/// Ids of one equality batch, packed end to end.
///
/// A category page matches hundreds of rows and every one of them used to
/// arrive as its own `String`. Here the batch owns one growing arena and one
/// span per id, so a wide equality allocates per batch instead of per row.
#[derive(Default)]
pub(super) struct IdBatch {
    arena: String,
    spans: Vec<IdSpan>,
    /// Buffer for the id currently being merged, reused on every pass.
    scratch: String,
}

impl IdBatch {
    pub(super) fn clear(&mut self) {
        self.arena.clear();
        self.spans.clear();
    }

    pub(super) fn push(&mut self, id: &str) {
        let span = push_id(&mut self.arena, id);
        self.spans.push(span);
    }

    pub(super) fn len(&self) -> usize {
        self.spans.len()
    }

    pub(super) fn iter(&self) -> impl Iterator<Item = &str> {
        self.spans.iter().map(|span| span.of(&self.arena))
    }

    pub(super) fn last(&self) -> Option<&str> {
        self.spans.last().map(|span| span.of(&self.arena))
    }
}

struct SecPairCursor<'a> {
    cursor: PagedPrefixCursor<'a>,
    prefix: Vec<u8>,
    /// Id of the head entry. It lives in a buffer the cursor refills instead
    /// of a fresh `String` per entry: an equality on a wide key advances the
    /// cursor once per member, and every one of those ids was allocated only
    /// to be compared against the merge head and dropped.
    id: String,
    head: Option<(u64, u8)>,
}

/// One entry from an ordered secondary walk. `key` is the complete tuple key;
/// callers use the physical id as the deterministic tie breaker.
pub(super) struct OrderedSecondaryEntry {
    pub(super) key: Vec<u8>,
    pub(super) id: String,
}

struct OrderedRunCursor<'a> {
    cursor: PagedPrefixCursor<'a>,
    head: Option<(Vec<u8>, u64, u8)>,
}

impl<'a> OrderedRunCursor<'a> {
    fn new(index: &'a PagedIndex, prefix: &[u8], after: Option<&[u8]>) -> Result<Self> {
        let mut cursor = Self {
            cursor: index.prefix_cursor_after(prefix, after),
            head: None,
        };
        cursor.advance()?;
        Ok(cursor)
    }

    fn head_pair(&self) -> Option<&[u8]> {
        self.head.as_ref().map(|(pair, _, _)| pair.as_slice())
    }

    fn take_head(&mut self) -> Option<(Vec<u8>, u64, u8)> {
        self.head.take()
    }

    fn advance(&mut self) -> Result<()> {
        self.head = match self.cursor.next()? {
            Some((pair, value)) => {
                let (generation, operation) = decode_secondary_operation(value)?;
                Some((pair.to_vec(), generation, operation))
            }
            None => None,
        };
        Ok(())
    }
}

struct OrderedMemCursor<'a> {
    entries: std::collections::btree_map::Range<'a, Vec<u8>, BTreeSet<String>>,
    prefix: &'a [u8],
    after: Option<&'a [u8]>,
    current_key: Option<&'a Vec<u8>>,
    ids: Option<std::collections::btree_set::Iter<'a, String>>,
    head: Option<Vec<u8>>,
}

impl<'a> OrderedMemCursor<'a> {
    fn new(
        map: &'a BTreeMap<Vec<u8>, BTreeSet<String>>,
        prefix: &'a [u8],
        after: Option<&'a [u8]>,
    ) -> Self {
        use std::ops::Bound::{Included, Unbounded};
        let mut cursor = Self {
            entries: map.range::<[u8], _>((Included(prefix), Unbounded)),
            prefix,
            after,
            current_key: None,
            ids: None,
            head: None,
        };
        cursor.advance();
        cursor
    }

    fn head_pair(&self) -> Option<&[u8]> {
        self.head.as_deref()
    }

    fn advance(&mut self) {
        self.head = None;
        loop {
            if let Some(ids) = &mut self.ids {
                if let Some(id) = ids.next() {
                    let pair = secondary_pair_key(
                        self.current_key.expect("id iterator has a tuple key"),
                        id,
                    );
                    if self.after.is_none_or(|after| pair.as_slice() > after) {
                        self.head = Some(pair);
                        return;
                    }
                    continue;
                }
            }
            self.ids = None;
            let Some((key, ids)) = self.entries.next() else {
                return;
            };
            if !key.starts_with(self.prefix) {
                return;
            }
            self.current_key = Some(key);
            self.ids = Some(ids.iter());
        }
    }
}

impl<'a> SecPairCursor<'a> {
    fn new(index: &'a PagedIndex, key: &[u8], after: Option<&str>) -> Result<Self> {
        let prefix = secondary_pair_prefix(key);
        let after_key = after.map(|after| secondary_pair_key(key, after));
        let mut cursor = Self {
            cursor: index.prefix_cursor_after(&prefix, after_key.as_deref()),
            prefix,
            id: String::new(),
            head: None,
        };
        cursor.advance()?;
        Ok(cursor)
    }

    /// The id the cursor sits on, or `None` once the run is exhausted.
    fn head_id(&self) -> Option<&str> {
        self.head.map(|_| self.id.as_str())
    }

    fn advance(&mut self) -> Result<()> {
        self.head = None;
        self.id.clear();
        let Some((key, value)) = self.cursor.next()? else {
            return Ok(());
        };
        let id = key
            .strip_prefix(self.prefix.as_slice())
            .ok_or_else(|| Error::Corrupt("secondary index: invalid pair prefix".into()))?;
        let id = std::str::from_utf8(id)
            .map_err(|_| Error::Corrupt("secondary index: invalid id utf8".into()))?;
        let (version, operation) = decode_secondary_operation(value)?;
        self.id.push_str(id);
        self.head = Some((version, operation));
        Ok(())
    }
}

impl SecIdx {
    pub(super) fn resident(map: BTreeMap<Vec<u8>, BTreeSet<String>>) -> Self {
        Self {
            generation: 0,
            runs: Vec::new(),
            delta: map,
            removed: BTreeMap::new(),
            frozen: None,
        }
    }

    pub(super) fn paged_runs(generation: u64, runs: Vec<SecRun>) -> Result<Self> {
        for run in &runs {
            validate_secondary_run(&run.index)?;
        }
        Ok(Self {
            generation,
            runs,
            delta: BTreeMap::new(),
            removed: BTreeMap::new(),
            frozen: None,
        })
    }

    pub(super) fn run_metas(&self) -> Vec<DerivedRunMeta> {
        self.runs.iter().map(|run| run.meta.clone()).collect()
    }

    pub(super) fn ids(&self, key: &[u8]) -> Result<BTreeSet<String>> {
        let mut batch = IdBatch::default();
        self.ids_batch_into(key, None, usize::MAX, &mut batch)?;
        Ok(batch.iter().map(str::to_owned).collect())
    }

    pub(super) fn contains_pair(&self, key: &[u8], id: &str) -> Result<bool> {
        if self.removed.get(key).is_some_and(|ids| ids.contains(id)) {
            return Ok(false);
        }
        if self.delta.get(key).is_some_and(|ids| ids.contains(id)) {
            return Ok(true);
        }
        self.persisted_pair(key, id)
    }

    /// Whether a published generation still offers this pair, ignoring the
    /// mutable overlay. A pair that no generation carries needs no tombstone
    /// when it is removed: there is nothing for one to hide.
    pub(super) fn persisted_pair(&self, key: &[u8], id: &str) -> Result<bool> {
        let mut newest = None;
        let pair = secondary_pair_key(key, id);
        for run in &self.runs {
            run.index.visit_key(&pair, |encoded| {
                let operation = decode_secondary_operation(encoded)?;
                if newest.is_none_or(|current| operation > current) {
                    newest = Some(operation);
                }
                Ok(true)
            })?;
        }
        if let Some(frozen) = &self.frozen {
            for (map, op) in [
                (&frozen.delta, SECONDARY_ADD),
                (&frozen.removed, SECONDARY_DELETE),
            ] {
                if map.get(key).is_some_and(|ids| ids.contains(id)) {
                    let operation = (frozen.generation, op);
                    if newest.is_none_or(|current| {
                        operation.0 > current.0
                            || (op == SECONDARY_DELETE && operation.0 == current.0)
                    }) {
                        newest = Some(operation);
                    }
                }
            }
        }
        Ok(newest.is_some_and(|(_, op)| op == SECONDARY_ADD))
    }

    /// Merge one cursor per immutable run plus the bounded mutable overlay.
    /// Versioned tombstones make the result independent of level order.
    /// Ids of one equality batch, packed into `out`.
    ///
    /// The merge compares one id at a time against the head of every run and
    /// overlay. Handing that id out as a `String` cost an allocation for each
    /// entry the merge looked at, including the ones a delete tombstone then
    /// removed; `out` reuses one buffer for the comparison and one arena for
    /// the ids that survive.
    pub(super) fn ids_batch_into(
        &self,
        key: &[u8],
        after: Option<&str>,
        limit: usize,
        out: &mut IdBatch,
    ) -> Result<()> {
        use std::ops::Bound::{Excluded, Unbounded};

        out.clear();
        if limit == 0 {
            return Ok(());
        }
        let prefix = secondary_pair_prefix(key);
        let mut cursors = self
            .runs
            .iter()
            .filter(|run| run.index.may_contain_prefix(&prefix))
            .map(|run| SecPairCursor::new(&run.index, key, after))
            .collect::<Result<Vec<_>>>()?;
        let mut added = self.delta.get(key).map(|ids| match after {
            Some(after) => ids.range::<str, _>((Excluded(after), Unbounded)).peekable(),
            None => ids.range::<str, _>((Unbounded, Unbounded)).peekable(),
        });
        let mut removed = self.removed.get(key).map(|ids| match after {
            Some(after) => ids.range::<str, _>((Excluded(after), Unbounded)).peekable(),
            None => ids.range::<str, _>((Unbounded, Unbounded)).peekable(),
        });
        let mut frozen_added = self.frozen.as_ref().and_then(|frozen| {
            frozen.delta.get(key).map(|ids| match after {
                Some(after) => ids.range::<str, _>((Excluded(after), Unbounded)).peekable(),
                None => ids.range::<str, _>((Unbounded, Unbounded)).peekable(),
            })
        });
        let mut frozen_removed = self.frozen.as_ref().and_then(|frozen| {
            frozen.removed.get(key).map(|ids| match after {
                Some(after) => ids.range::<str, _>((Excluded(after), Unbounded)).peekable(),
                None => ids.range::<str, _>((Unbounded, Unbounded)).peekable(),
            })
        });
        loop {
            let next_persisted = cursors.iter().filter_map(SecPairCursor::head_id).min();
            let next_added = added
                .as_mut()
                .and_then(|iter| iter.peek().map(|id| id.as_str()));
            let next_removed = removed
                .as_mut()
                .and_then(|iter| iter.peek().map(|id| id.as_str()));
            let next_frozen_added = frozen_added
                .as_mut()
                .and_then(|iter| iter.peek().map(|id| id.as_str()));
            let next_frozen_removed = frozen_removed
                .as_mut()
                .and_then(|iter| iter.peek().map(|id| id.as_str()));
            let Some(next) = next_persisted
                .into_iter()
                .chain(next_frozen_added)
                .chain(next_frozen_removed)
                .chain(next_added)
                .chain(next_removed)
                .min()
            else {
                break;
            };
            // Copied out of the heads so they can be advanced below; the
            // buffer is the same one on every pass.
            out.scratch.clear();
            out.scratch.push_str(next);
            let id = std::mem::take(&mut out.scratch);
            let mut newest: Option<(u64, u8)> = None;
            for cursor in &mut cursors {
                while cursor.head_id() == Some(id.as_str()) {
                    let (version, operation) = cursor.head.take().expect("matching secondary head");
                    if newest.is_none_or(|current| (version, operation) > current) {
                        newest = Some((version, operation));
                    }
                    cursor.advance()?;
                }
            }
            let frozen_generation = self.frozen.as_ref().map(|frozen| frozen.generation);
            if frozen_added.as_mut().is_some_and(|iter| {
                iter.peek()
                    .is_some_and(|candidate| candidate.as_str() == id)
            }) {
                frozen_added.as_mut().expect("checked above").next();
                let operation = (
                    frozen_generation.expect("frozen iterator has generation"),
                    SECONDARY_ADD,
                );
                if newest.is_none_or(|current| operation.0 > current.0) {
                    newest = Some(operation);
                }
            }
            if frozen_removed.as_mut().is_some_and(|iter| {
                iter.peek()
                    .is_some_and(|candidate| candidate.as_str() == id)
            }) {
                frozen_removed.as_mut().expect("checked above").next();
                let generation = frozen_generation.expect("frozen iterator has generation");
                if newest.is_none_or(|current| generation >= current.0) {
                    newest = Some((generation, SECONDARY_DELETE));
                }
            }
            if added.as_mut().is_some_and(|iter| {
                iter.peek()
                    .is_some_and(|candidate| candidate.as_str() == id)
            }) {
                added.as_mut().expect("checked above").next();
                newest = Some((u64::MAX, SECONDARY_ADD));
            }
            if removed.as_mut().is_some_and(|iter| {
                iter.peek()
                    .is_some_and(|candidate| candidate.as_str() == id)
            }) {
                removed.as_mut().expect("checked above").next();
                newest = Some((u64::MAX, SECONDARY_DELETE));
            }
            let keep = newest.is_some_and(|(_, operation)| operation == SECONDARY_ADD);
            if keep {
                out.push(&id);
            }
            // Give the buffer back whether the id was kept or not, so the
            // next pass writes into the same allocation.
            out.scratch = id;
            if keep && out.len() == limit {
                break;
            }
        }
        Ok(())
    }

    /// Merge immutable runs plus mutable/frozen overlays in physical tuple
    /// order. The prefix is a sequence of complete encoded tuple components;
    /// `after` is the last full persisted pair returned to the caller.
    pub(super) fn ordered_entries_into(
        &self,
        tuple_prefix: &[u8],
        after: Option<&[u8]>,
        limit: usize,
        out: &mut Vec<OrderedSecondaryEntry>,
    ) -> Result<()> {
        out.clear();
        if limit == 0 {
            return Ok(());
        }
        let mut physical_prefix = Vec::with_capacity(1 + tuple_prefix.len());
        physical_prefix.push(SECONDARY_ENTRY_TAG);
        physical_prefix.extend_from_slice(tuple_prefix);
        let mut runs = self
            .runs
            .iter()
            .filter(|run| run.index.may_contain_prefix(&physical_prefix))
            .map(|run| OrderedRunCursor::new(&run.index, &physical_prefix, after))
            .collect::<Result<Vec<_>>>()?;
        let mut delta = OrderedMemCursor::new(&self.delta, tuple_prefix, after);
        let mut removed = OrderedMemCursor::new(&self.removed, tuple_prefix, after);
        let mut frozen_delta = self
            .frozen
            .as_ref()
            .map(|frozen| OrderedMemCursor::new(&frozen.delta, tuple_prefix, after));
        let mut frozen_removed = self
            .frozen
            .as_ref()
            .map(|frozen| OrderedMemCursor::new(&frozen.removed, tuple_prefix, after));

        loop {
            let next = runs
                .iter()
                .filter_map(OrderedRunCursor::head_pair)
                .chain(delta.head_pair())
                .chain(removed.head_pair())
                .chain(frozen_delta.as_ref().and_then(OrderedMemCursor::head_pair))
                .chain(
                    frozen_removed
                        .as_ref()
                        .and_then(OrderedMemCursor::head_pair),
                )
                .min()
                .map(<[u8]>::to_vec);
            let Some(pair) = next else {
                break;
            };
            let mut newest: Option<(u64, u8)> = None;
            for run in &mut runs {
                while run.head_pair() == Some(pair.as_slice()) {
                    let (_, generation, operation) = run.take_head().expect("matching run head");
                    if newest.is_none_or(|current| (generation, operation) > current) {
                        newest = Some((generation, operation));
                    }
                    run.advance()?;
                }
            }
            let frozen_generation = self.frozen.as_ref().map(|frozen| frozen.generation);
            if frozen_delta
                .as_ref()
                .is_some_and(|cursor| cursor.head_pair() == Some(pair.as_slice()))
            {
                frozen_delta.as_mut().expect("checked above").advance();
                let operation = (
                    frozen_generation.expect("frozen cursor has generation"),
                    SECONDARY_ADD,
                );
                if newest.is_none_or(|current| operation.0 > current.0) {
                    newest = Some(operation);
                }
            }
            if frozen_removed
                .as_ref()
                .is_some_and(|cursor| cursor.head_pair() == Some(pair.as_slice()))
            {
                frozen_removed.as_mut().expect("checked above").advance();
                let generation = frozen_generation.expect("frozen cursor has generation");
                if newest.is_none_or(|current| generation >= current.0) {
                    newest = Some((generation, SECONDARY_DELETE));
                }
            }
            if delta.head_pair() == Some(pair.as_slice()) {
                delta.advance();
                newest = Some((u64::MAX, SECONDARY_ADD));
            }
            if removed.head_pair() == Some(pair.as_slice()) {
                removed.advance();
                newest = Some((u64::MAX, SECONDARY_DELETE));
            }
            if newest.is_some_and(|(_, operation)| operation == SECONDARY_ADD) {
                let (key, id) = secondary_pair_parts(&pair)?;
                out.push(OrderedSecondaryEntry {
                    key: key.to_vec(),
                    id: id.to_owned(),
                });
                if out.len() == limit {
                    break;
                }
            }
        }
        Ok(())
    }

    pub(super) fn add(&mut self, key: Vec<u8>, id: &str) {
        if let Some(removed) = self.removed.get_mut(&key) {
            removed.remove(id);
            if removed.is_empty() {
                self.removed.remove(&key);
            }
        }
        self.delta.entry(key).or_default().insert(id.to_owned());
    }

    pub(super) fn remove(&mut self, key: &[u8], id: &str) {
        if let Some(delta) = self.delta.get_mut(key) {
            delta.remove(id);
            if delta.is_empty() {
                self.delta.remove(key);
            }
        }
        // A row written and deleted between two publications never reached a
        // generation, so nothing has to be hidden from a later reader.
        // Recording its removal anyway left an entry that every subsequent
        // lookup of that key walked past: a cart emptied a few thousand times
        // made reading it seventeen times slower with one row left in it.
        // A read failure here keeps the old, conservative tombstone.
        if (!self.runs.is_empty() || self.frozen.is_some())
            && self.persisted_pair(key, id).unwrap_or(true)
        {
            self.removed
                .entry(key.to_vec())
                .or_default()
                .insert(id.to_owned());
        }
    }

    pub(super) fn delta_memory_bytes(&self) -> usize {
        fn map_bytes(map: &BTreeMap<Vec<u8>, BTreeSet<String>>) -> usize {
            map.iter()
                .map(|(key, ids)| {
                    key.len() + 96 + ids.iter().map(|id| id.len() + 48).sum::<usize>()
                })
                .sum()
        }
        map_bytes(&self.delta) + map_bytes(&self.removed)
    }

    pub(super) fn frozen_delta_memory_bytes(&self) -> usize {
        fn map_bytes(map: &BTreeMap<Vec<u8>, BTreeSet<String>>) -> usize {
            map.iter()
                .map(|(key, ids)| {
                    key.len() + 96 + ids.iter().map(|id| id.len() + 48).sum::<usize>()
                })
                .sum()
        }
        self.frozen.as_ref().map_or(0, |frozen| {
            map_bytes(&frozen.delta).saturating_add(map_bytes(&frozen.removed))
        })
    }

    pub(super) fn freeze_delta(&mut self, generation: u64) -> Option<Arc<FrozenSecDelta>> {
        if self.frozen.is_some() || (self.delta.is_empty() && self.removed.is_empty()) {
            return None;
        }
        let frozen = Arc::new(FrozenSecDelta {
            generation,
            delta: std::mem::take(&mut self.delta),
            removed: std::mem::take(&mut self.removed),
        });
        self.frozen = Some(frozen.clone());
        Some(frozen)
    }

    pub(super) fn frozen_matches(&self, frozen: &Arc<FrozenSecDelta>) -> bool {
        self.frozen
            .as_ref()
            .is_some_and(|current| Arc::ptr_eq(current, frozen))
    }

    pub(super) fn clear_frozen(&mut self, frozen: &Arc<FrozenSecDelta>) {
        if self.frozen_matches(frozen) {
            self.frozen = None;
        }
    }
}

pub(super) fn secondary_pair_prefix(key: &[u8]) -> Vec<u8> {
    let mut pair = Vec::with_capacity(2 + key.len());
    pair.push(SECONDARY_ENTRY_TAG);
    pair.extend_from_slice(key);
    // `0xff` cannot occur in a UTF-8 physical identity. Reading from the
    // end leaves encoded values free to contain any byte while their ordered
    // bytes remain directly after the entry tag.
    pair.push(0xff);
    pair
}

pub(super) fn secondary_pair_key(key: &[u8], id: &str) -> Vec<u8> {
    let mut pair = secondary_pair_prefix(key);
    pair.extend_from_slice(id.as_bytes());
    pair
}

pub(super) fn secondary_pair_parts(pair: &[u8]) -> Result<(&[u8], &str)> {
    if pair.first() != Some(&SECONDARY_ENTRY_TAG) {
        return Err(Error::Corrupt("secondary index: invalid pair key".into()));
    }
    let separator = pair[1..]
        .iter()
        .rposition(|byte| *byte == 0xff)
        .map(|offset| offset + 1)
        .ok_or_else(|| Error::Corrupt("secondary index: missing pair terminator".into()))?;
    let id = std::str::from_utf8(&pair[separator + 1..])
        .map_err(|_| Error::Corrupt("secondary index: invalid id utf8".into()))?;
    Ok((&pair[1..separator], id))
}

pub(super) fn secondary_operation(version: u64, operation: u8) -> [u8; 9] {
    let mut value = [0; 9];
    value[..8].copy_from_slice(&version.to_be_bytes());
    value[8] = operation;
    value
}

pub(super) fn decode_secondary_operation(value: &[u8]) -> Result<(u64, u8)> {
    if value.len() != 9 || !matches!(value[8], SECONDARY_DELETE | SECONDARY_ADD) {
        return Err(Error::Corrupt("secondary index: invalid operation".into()));
    }
    Ok((
        u64::from_be_bytes(value[..8].try_into().expect("eight bytes")),
        value[8],
    ))
}

pub(super) fn validate_secondary_run(index: &PagedIndex) -> Result<()> {
    let mut valid = false;
    index.visit_key(SECONDARY_FORMAT_KEY, |value| {
        valid = value == SECONDARY_FORMAT_VALUE;
        Ok(false)
    })?;
    if valid {
        Ok(())
    } else {
        Err(Error::Corrupt(
            "secondary index: unsupported run format".into(),
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pair_keeps_value_key_before_identity() {
        let key = [0x10, 0, 0xff, 0x20];
        let pair = secondary_pair_key(&key, "row-α");
        assert_eq!(
            &pair[..key.len() + 2],
            &[SECONDARY_ENTRY_TAG, 0x10, 0, 0xff, 0x20, 0xff]
        );
        assert_eq!(secondary_pair_prefix(&key), pair[..key.len() + 2]);
        assert_eq!(secondary_pair_parts(&pair).unwrap(), (&key[..], "row-α"));
    }

    #[test]
    fn pair_rejects_missing_terminator_and_invalid_identity() {
        assert!(secondary_pair_parts(&[SECONDARY_ENTRY_TAG, 0x10]).is_err());
        assert!(secondary_pair_parts(&[SECONDARY_ENTRY_TAG, 0x10, 0xff, 0x80]).is_err());
        assert!(secondary_pair_parts(&[0, 0xff, b'x']).is_err());
    }

    #[test]
    fn pairs_preserve_ordered_scalar_keys_and_round_trip_every_value_encoding() {
        let ordered = [
            Value::Int64(i64::MIN),
            Value::Int64(-1),
            Value::Int64(0),
            Value::Int64(i64::MAX),
        ];
        let pairs = ordered
            .iter()
            .map(|value| secondary_pair_key(&index_key(value), "row"))
            .collect::<Vec<_>>();
        assert!(pairs.windows(2).all(|pair| pair[0] < pair[1]));

        for value in [
            Value::Null,
            Value::Bool(true),
            Value::Float64(-0.0),
            Value::Text("a\0b".into()),
            Value::Blob(vec![0, 0xff, 1]),
            Value::Timestamp(-1),
            Value::Date(i32::MIN),
            Value::Time(i64::MAX),
            Value::Json(serde_json::json!({"nul": "\\u0000"})),
            Value::Vector(vec![0.0, -1.0]),
        ] {
            let key = index_key(&value);
            let pair = secondary_pair_key(&key, "row");
            assert_eq!(secondary_pair_parts(&pair).unwrap(), (&key[..], "row"));
        }
    }

    #[test]
    fn ordered_walk_uses_tuple_prefix_and_resident_deltas() {
        let mut index = SecIdx::resident(BTreeMap::new());
        let category = index_key(&Value::Text("books".into()));
        let mut low = category.clone();
        low.extend(index_key(&Value::Int64(10)));
        let mut high = category.clone();
        high.extend(index_key(&Value::Int64(20)));
        let other = index_key(&Value::Text("games".into()));
        index.add(high, "later");
        index.add(other, "other");
        index.add(low, "first");

        let mut entries = Vec::new();
        index
            .ordered_entries_into(&category, None, 8, &mut entries)
            .unwrap();
        assert_eq!(
            entries
                .iter()
                .map(|entry| entry.id.as_str())
                .collect::<Vec<_>>(),
            ["first", "later"]
        );
        let after = secondary_pair_key(&entries[0].key, &entries[0].id);
        index
            .ordered_entries_into(&category, Some(&after), 8, &mut entries)
            .unwrap();
        assert_eq!(
            entries
                .iter()
                .map(|entry| entry.id.as_str())
                .collect::<Vec<_>>(),
            ["later"]
        );
    }
}
