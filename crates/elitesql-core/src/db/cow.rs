//! Sorted maps and sets whose copies share their storage.
//!
//! The derived indexes a commit changes are also what readers take out of the
//! state lock. A `CowMap` keeps its entries in chunks behind `Arc`: a clone
//! costs one reference count per chunk, and a write to a map that a reader
//! also holds copies the chunk it changes (one reference per entry) and the
//! entry itself, nothing else. Entries are ordered by key and a chunk splits
//! once it doubles, so lookups stay a binary search over the chunks and then
//! within one.
use std::borrow::Borrow;
use std::sync::Arc;

/// Entries per chunk; a chunk splits in two past twice this.
const CHUNK_ENTRIES: usize = 64;

type Entry<K, V> = Arc<(K, V)>;

#[derive(Clone)]
struct Chunk<K, V> {
    entries: Vec<Entry<K, V>>,
}

pub(crate) struct CowMap<K, V> {
    /// Never holds an empty chunk.
    chunks: Vec<Arc<Chunk<K, V>>>,
    len: usize,
}

impl<K, V> Clone for CowMap<K, V> {
    fn clone(&self) -> Self {
        Self {
            chunks: self.chunks.clone(),
            len: self.len,
        }
    }
}

impl<K, V> Default for CowMap<K, V> {
    fn default() -> Self {
        Self {
            chunks: Vec::new(),
            len: 0,
        }
    }
}

impl<K: Ord + Clone, V: Clone> CowMap<K, V> {
    #[cfg(test)]
    pub(crate) fn len(&self) -> usize {
        self.len
    }

    pub(crate) fn is_empty(&self) -> bool {
        self.len == 0
    }

    /// The chunk that holds `key` if any does: the last whose first key is
    /// not greater than `key`, or the first chunk.
    fn chunk_for<Q>(&self, key: &Q) -> usize
    where
        K: Borrow<Q>,
        Q: Ord + ?Sized,
    {
        self.chunks
            .partition_point(|chunk| chunk.entries[0].0.borrow() <= key)
            .saturating_sub(1)
    }

    /// `(chunk, position)` of `key`, or where it would be inserted.
    fn locate<Q>(&self, key: &Q) -> Result<(usize, usize), (usize, usize)>
    where
        K: Borrow<Q>,
        Q: Ord + ?Sized,
    {
        if self.chunks.is_empty() {
            return Err((0, 0));
        }
        let chunk = self.chunk_for(key);
        match self.chunks[chunk]
            .entries
            .binary_search_by(|entry| entry.0.borrow().cmp(key))
        {
            Ok(at) => Ok((chunk, at)),
            Err(at) => Err((chunk, at)),
        }
    }

    pub(crate) fn get<Q>(&self, key: &Q) -> Option<&V>
    where
        K: Borrow<Q>,
        Q: Ord + ?Sized,
    {
        let (chunk, at) = self.locate(key).ok()?;
        Some(&self.chunks[chunk].entries[at].1)
    }

    pub(crate) fn contains_key<Q>(&self, key: &Q) -> bool
    where
        K: Borrow<Q>,
        Q: Ord + ?Sized,
    {
        self.get(key).is_some()
    }

    pub(crate) fn get_mut<Q>(&mut self, key: &Q) -> Option<&mut V>
    where
        K: Borrow<Q>,
        Q: Ord + ?Sized,
    {
        let (chunk, at) = self.locate(key).ok()?;
        let chunk = Arc::make_mut(&mut self.chunks[chunk]);
        Some(&mut Arc::make_mut(&mut chunk.entries[at]).1)
    }

    /// The value of `key`, inserting `V::default()` first when absent.
    pub(crate) fn get_or_insert_default(&mut self, key: K) -> &mut V
    where
        V: Default,
    {
        let (chunk, at) = match self.locate(&key) {
            Ok(found) => found,
            Err(_) => {
                self.insert(key.clone(), V::default());
                self.locate(&key).expect("just inserted")
            }
        };
        let chunk = Arc::make_mut(&mut self.chunks[chunk]);
        &mut Arc::make_mut(&mut chunk.entries[at]).1
    }

    pub(crate) fn insert(&mut self, key: K, value: V) -> Option<V> {
        match self.locate(&key) {
            Ok((chunk, at)) => {
                let chunk = Arc::make_mut(&mut self.chunks[chunk]);
                let entry = Arc::make_mut(&mut chunk.entries[at]);
                Some(std::mem::replace(&mut entry.1, value))
            }
            Err((chunk, at)) => {
                self.len += 1;
                if self.chunks.is_empty() {
                    self.chunks.push(Arc::new(Chunk {
                        entries: vec![Arc::new((key, value))],
                    }));
                    return None;
                }
                // Past the last key: append, opening a chunk when the last
                // one is full, so increasing keys never split anything.
                let last = self.chunks.len() - 1;
                if chunk == last
                    && at == self.chunks[last].entries.len()
                    && self.chunks[last].entries.len() >= CHUNK_ENTRIES
                {
                    self.chunks.push(Arc::new(Chunk {
                        entries: vec![Arc::new((key, value))],
                    }));
                    return None;
                }
                let target = Arc::make_mut(&mut self.chunks[chunk]);
                target.entries.insert(at, Arc::new((key, value)));
                if target.entries.len() > 2 * CHUNK_ENTRIES {
                    let upper = target.entries.split_off(target.entries.len() / 2);
                    self.chunks
                        .insert(chunk + 1, Arc::new(Chunk { entries: upper }));
                }
                None
            }
        }
    }

    pub(crate) fn remove<Q>(&mut self, key: &Q) -> Option<V>
    where
        K: Borrow<Q>,
        Q: Ord + ?Sized,
    {
        let (chunk, at) = self.locate(key).ok()?;
        self.len -= 1;
        let target = Arc::make_mut(&mut self.chunks[chunk]);
        let entry = target.entries.remove(at);
        if target.entries.is_empty() {
            self.chunks.remove(chunk);
        }
        Some(match Arc::try_unwrap(entry) {
            Ok((_, value)) => value,
            Err(shared) => shared.1.clone(),
        })
    }

    pub(crate) fn clear(&mut self) {
        self.chunks.clear();
        self.len = 0;
    }

    pub(crate) fn iter(&self) -> impl Iterator<Item = (&K, &V)> + '_ {
        self.chunks
            .iter()
            .flat_map(|chunk| chunk.entries.iter().map(|entry| (&entry.0, &entry.1)))
    }

    pub(crate) fn keys(&self) -> impl Iterator<Item = &K> + '_ {
        self.iter().map(|(key, _)| key)
    }

    /// Entries from `start` on, `start` included when `inclusive`.
    pub(crate) fn range_from<'a, Q>(
        &'a self,
        start: &Q,
        inclusive: bool,
    ) -> impl Iterator<Item = (&'a K, &'a V)> + 'a
    where
        K: Borrow<Q>,
        Q: Ord + ?Sized,
    {
        let (first_chunk, first_at) = match self.locate(start) {
            Ok((chunk, at)) => (chunk, if inclusive { at } else { at + 1 }),
            Err((chunk, at)) => (chunk, at),
        };
        self.chunks
            .iter()
            .enumerate()
            .skip(first_chunk)
            .flat_map(move |(index, chunk)| {
                let skip = if index == first_chunk { first_at } else { 0 };
                chunk.entries[skip.min(chunk.entries.len())..]
                    .iter()
                    .map(|entry| (&entry.0, &entry.1))
            })
    }
}

impl<'a, K: Ord + Clone, V: Clone> IntoIterator for &'a CowMap<K, V> {
    type Item = (&'a K, &'a V);
    type IntoIter = Box<dyn Iterator<Item = (&'a K, &'a V)> + 'a>;

    fn into_iter(self) -> Self::IntoIter {
        Box::new(self.iter())
    }
}

/// A sorted set sharing its storage like `CowMap`.

#[derive(Clone)]
pub(crate) struct CowSet<T>(CowMap<T, ()>);

impl<T> Default for CowSet<T> {
    fn default() -> Self {
        Self(CowMap::default())
    }
}

impl<T: Ord + Clone> CowSet<T> {
    #[cfg(test)]
    pub(crate) fn len(&self) -> usize {
        self.0.len()
    }

    pub(crate) fn is_empty(&self) -> bool {
        self.0.is_empty()
    }

    pub(crate) fn contains<Q>(&self, value: &Q) -> bool
    where
        T: Borrow<Q>,
        Q: Ord + ?Sized,
    {
        self.0.contains_key(value)
    }

    /// Whether `value` was absent.
    pub(crate) fn insert(&mut self, value: T) -> bool {
        self.0.insert(value, ()).is_none()
    }

    /// Whether `value` was present.
    pub(crate) fn remove<Q>(&mut self, value: &Q) -> bool
    where
        T: Borrow<Q>,
        Q: Ord + ?Sized,
    {
        self.0.remove(value).is_some()
    }

    pub(crate) fn iter(&self) -> impl Iterator<Item = &T> + '_ {
        self.0.keys()
    }

    /// Members after `after`, or all of them.
    pub(crate) fn range_after<'a, Q>(
        &'a self,
        after: Option<&Q>,
    ) -> Box<dyn Iterator<Item = &'a T> + 'a>
    where
        T: Borrow<Q>,
        Q: Ord + ?Sized,
    {
        match after {
            Some(after) => Box::new(self.0.range_from(after, false).map(|(value, _)| value)),
            None => Box::new(self.0.keys()),
        }
    }
}

impl<T: Ord + Clone> FromIterator<T> for CowSet<T> {
    fn from_iter<I: IntoIterator<Item = T>>(iter: I) -> Self {
        let mut set = Self::default();
        for value in iter {
            set.insert(value);
        }
        set
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::{BTreeMap, BTreeSet};

    /// Random inserts, removals and range reads agree with the standard
    /// collections, and a clone taken halfway keeps what it saw.
    #[test]
    fn cow_map_and_set_match_the_standard_collections_and_clones_are_stable() {
        let mut map: CowMap<Vec<u8>, CowSet<String>> = CowMap::default();
        let mut model: BTreeMap<Vec<u8>, BTreeSet<String>> = BTreeMap::new();
        let mut seed = 0x2545_F491_4F6C_DD1Du64;
        let mut next = || {
            seed ^= seed << 13;
            seed ^= seed >> 7;
            seed ^= seed << 17;
            seed
        };
        let mut frozen = None;
        for step in 0..30_000u64 {
            let key = vec![(next() % 40) as u8, (next() % 7) as u8];
            let id = format!("{:05}", next() % 3_000);
            if next() % 4 == 0 {
                if let Some(ids) = map.get_mut(&key) {
                    ids.remove(id.as_str());
                    if ids.is_empty() {
                        map.remove(&key);
                    }
                }
                if let Some(ids) = model.get_mut(&key) {
                    ids.remove(&id);
                    if ids.is_empty() {
                        model.remove(&key);
                    }
                }
            } else {
                map.get_or_insert_default(key.clone()).insert(id.clone());
                model.entry(key).or_default().insert(id);
            }
            if step == 15_000 {
                frozen = Some((map.clone(), model.clone()));
            }
        }
        let check = |map: &CowMap<Vec<u8>, CowSet<String>>,
                     model: &BTreeMap<Vec<u8>, BTreeSet<String>>| {
            assert_eq!(map.len(), model.len());
            let got: Vec<(Vec<u8>, Vec<String>)> = map
                .iter()
                .map(|(key, ids)| (key.clone(), ids.iter().cloned().collect()))
                .collect();
            let want: Vec<(Vec<u8>, Vec<String>)> = model
                .iter()
                .map(|(key, ids)| (key.clone(), ids.iter().cloned().collect()))
                .collect();
            assert_eq!(got, want);
            for (key, ids) in model.iter().step_by(3) {
                let set = map.get(key.as_slice()).unwrap();
                assert_eq!(set.len(), ids.len());
                for id in ids.iter().step_by(5) {
                    assert!(set.contains(id.as_str()));
                    let after: Vec<&String> = set.range_after(Some(id.as_str())).take(3).collect();
                    let want: Vec<&String> = ids
                        .range::<str, _>((
                            std::ops::Bound::Excluded(id.as_str()),
                            std::ops::Bound::Unbounded,
                        ))
                        .take(3)
                        .collect();
                    assert_eq!(after, want);
                }
                let from: Vec<&Vec<u8>> = map
                    .range_from(key.as_slice(), true)
                    .map(|(k, _)| k)
                    .take(4)
                    .collect();
                let want: Vec<&Vec<u8>> =
                    model.range(key.clone()..).map(|(k, _)| k).take(4).collect();
                assert_eq!(from, want);
                let past: Vec<&Vec<u8>> = map
                    .range_from(key.as_slice(), false)
                    .map(|(k, _)| k)
                    .take(4)
                    .collect();
                let want: Vec<&Vec<u8>> = model
                    .range::<Vec<u8>, _>((
                        std::ops::Bound::Excluded(key.clone()),
                        std::ops::Bound::Unbounded,
                    ))
                    .map(|(k, _)| k)
                    .take(4)
                    .collect();
                assert_eq!(past, want);
            }
            for chunk in &map.chunks {
                assert!(!chunk.entries.is_empty() && chunk.entries.len() <= 2 * CHUNK_ENTRIES);
            }
        };
        check(&map, &model);
        let (frozen_map, frozen_model) = frozen.unwrap();
        check(&frozen_map, &frozen_model);
    }
}
