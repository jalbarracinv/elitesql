//! The row a read returns: values in payload order with the column names
//! shared between rows.
//!
//! `Record` used to be a `BTreeMap<String, Value>`. Decoding one column then
//! cost an allocation for the name plus a tree node, which the 2026-09-12
//! measurements put at 52 ns per column against SQLite's 13 ns. Here the
//! names of a row live in one refcounted slice that every row of the same
//! shape shares (see [`LayoutCache`]), and the values in one `Vec` beside it,
//! so decoding a column is a comparison and a push.
//!
//! Lookup by name is a linear scan. Tables are tens of columns wide at most,
//! and the scan compares string slices without touching the heap, so it beats
//! the tree it replaces at every width the engine supports.

use std::fmt;
use std::sync::Arc;

use crate::Value;

/// Column names of a row. `Shared` is the common case: the decoder hands out
/// the same slice to every row of one shape. `Owned` appears when a record is
/// built or mutated column by column.
#[derive(Clone)]
enum Names {
    Shared(Arc<[Box<str>]>),
    Owned(Vec<Box<str>>),
}

impl Names {
    fn as_slice(&self) -> &[Box<str>] {
        match self {
            Names::Shared(names) => names,
            Names::Owned(names) => names,
        }
    }

    /// Make the names mutable, copying them out of the shared slice once.
    fn to_mut(&mut self) -> &mut Vec<Box<str>> {
        if let Names::Shared(shared) = self {
            *self = Names::Owned(shared.to_vec());
        }
        match self {
            Names::Owned(names) => names,
            Names::Shared(_) => unreachable!("converted above"),
        }
    }
}

/// One row: the columns it carries, in order, with their values.
///
/// The order is the order the columns were decoded or inserted in (schema
/// order for a stored row), not alphabetical order as in the `BTreeMap` this
/// type replaced. Equality ignores order.
#[derive(Clone)]
pub struct Record {
    names: Names,
    values: Vec<Value>,
}

impl Record {
    pub fn new() -> Self {
        Record {
            names: Names::Owned(Vec::new()),
            values: Vec::new(),
        }
    }

    pub fn with_capacity(columns: usize) -> Self {
        Record {
            names: Names::Owned(Vec::with_capacity(columns)),
            values: Vec::with_capacity(columns),
        }
    }

    /// Build a row from a shared layout. `values` must be as long as `names`.
    pub(crate) fn from_layout(names: Arc<[Box<str>]>, values: Vec<Value>) -> Self {
        debug_assert_eq!(names.len(), values.len());
        Record {
            names: Names::Shared(names),
            values,
        }
    }

    pub fn len(&self) -> usize {
        self.values.len()
    }

    pub fn is_empty(&self) -> bool {
        self.values.is_empty()
    }

    /// Position of a column, or `None`.
    #[inline]
    pub fn position(&self, name: &str) -> Option<usize> {
        let wanted = name.as_bytes();
        self.names
            .as_slice()
            .iter()
            .position(|stored| stored.as_bytes() == wanted)
    }

    #[inline]
    pub fn get(&self, name: &str) -> Option<&Value> {
        self.position(name).map(|at| &self.values[at])
    }

    pub fn get_mut(&mut self, name: &str) -> Option<&mut Value> {
        self.position(name).map(|at| &mut self.values[at])
    }

    pub fn contains_key(&self, name: &str) -> bool {
        self.position(name).is_some()
    }

    /// Value at a position, for callers that already resolved the column.
    #[inline]
    pub fn value_at(&self, at: usize) -> Option<&Value> {
        self.values.get(at)
    }

    pub fn name_at(&self, at: usize) -> Option<&str> {
        self.names.as_slice().get(at).map(|name| &**name)
    }

    /// Insert or replace a column, returning the previous value.
    pub fn insert(&mut self, name: impl Into<Box<str>>, value: Value) -> Option<Value> {
        let name = name.into();
        match self.position(&name) {
            Some(at) => Some(std::mem::replace(&mut self.values[at], value)),
            None => {
                self.names.to_mut().push(name);
                self.values.push(value);
                None
            }
        }
    }

    /// Move a column's value out, leaving NULL in its place and the row's
    /// shape untouched.
    ///
    /// Projecting a row used to clone every value it emitted, which for a
    /// text column copied its bytes once per row. The rows a projection
    /// consumes are dropped straight after, so the copy bought nothing.
    pub(crate) fn take(&mut self, name: &str) -> Option<Value> {
        let at = self.position(name)?;
        Some(std::mem::replace(&mut self.values[at], Value::Null))
    }

    pub fn remove(&mut self, name: &str) -> Option<Value> {
        let at = self.position(name)?;
        self.names.to_mut().remove(at);
        Some(self.values.remove(at))
    }

    pub fn keys(&self) -> impl ExactSizeIterator<Item = &str> + '_ {
        self.names.as_slice().iter().map(|name| &**name)
    }

    pub fn values(&self) -> impl ExactSizeIterator<Item = &Value> + '_ {
        self.values.iter()
    }

    pub fn values_mut(&mut self) -> impl ExactSizeIterator<Item = &mut Value> + '_ {
        self.values.iter_mut()
    }

    /// The values in column order, for callers that already know the layout.
    pub fn value_slice(&self) -> &[Value] {
        &self.values
    }

    pub fn iter(&self) -> impl ExactSizeIterator<Item = (&str, &Value)> + '_ {
        self.names
            .as_slice()
            .iter()
            .map(|name| &**name)
            .zip(self.values.iter())
    }

    pub fn iter_mut(&mut self) -> impl ExactSizeIterator<Item = (&str, &mut Value)> + '_ {
        self.names
            .as_slice()
            .iter()
            .map(|name| &**name)
            .zip(self.values.iter_mut())
    }

    /// True when the columns are exactly `expected`, in that order: the
    /// encoder then reads values by position instead of by name.
    pub(crate) fn matches_layout<'a>(
        &self,
        expected: impl ExactSizeIterator<Item = &'a str>,
    ) -> bool {
        if expected.len() != self.values.len() {
            return false;
        }
        self.names
            .as_slice()
            .iter()
            .zip(expected)
            .all(|(stored, wanted)| &**stored == wanted)
    }

    /// Columns sorted by name, for output that must not depend on the order
    /// the payload happens to store.
    pub fn sorted_iter(&self) -> Vec<(&str, &Value)> {
        let mut pairs = self.iter().collect::<Vec<_>>();
        pairs.sort_unstable_by(|(a, _), (b, _)| a.cmp(b));
        pairs
    }
}

impl Default for Record {
    fn default() -> Self {
        Record::new()
    }
}

impl fmt::Debug for Record {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_map().entries(self.iter()).finish()
    }
}

/// Order-insensitive, like the map this type replaced.
impl PartialEq for Record {
    fn eq(&self, other: &Self) -> bool {
        if self.len() != other.len() {
            return false;
        }
        self.iter()
            .all(|(name, value)| other.get(name) == Some(value))
    }
}

impl std::ops::Index<&str> for Record {
    type Output = Value;

    fn index(&self, name: &str) -> &Value {
        self.get(name)
            .unwrap_or_else(|| panic!("no column '{name}' in record"))
    }
}

impl<K: Into<Box<str>>> FromIterator<(K, Value)> for Record {
    fn from_iter<I: IntoIterator<Item = (K, Value)>>(iter: I) -> Self {
        let iter = iter.into_iter();
        let (lower, _) = iter.size_hint();
        let mut record = Record::with_capacity(lower);
        for (name, value) in iter {
            record.insert(name, value);
        }
        record
    }
}

impl<K: Into<Box<str>>, const N: usize> From<[(K, Value); N]> for Record {
    fn from(entries: [(K, Value); N]) -> Self {
        entries.into_iter().collect()
    }
}

impl<K: Into<Box<str>>> Extend<(K, Value)> for Record {
    fn extend<I: IntoIterator<Item = (K, Value)>>(&mut self, iter: I) {
        for (name, value) in iter {
            self.insert(name, value);
        }
    }
}

/// Owned iteration yields `String` keys, as the map this replaced did.
impl IntoIterator for Record {
    type Item = (String, Value);
    type IntoIter = std::iter::Zip<
        std::iter::Map<std::vec::IntoIter<Box<str>>, fn(Box<str>) -> String>,
        std::vec::IntoIter<Value>,
    >;

    fn into_iter(self) -> Self::IntoIter {
        let names = match self.names {
            Names::Shared(shared) => shared.to_vec(),
            Names::Owned(names) => names,
        };
        let to_string: fn(Box<str>) -> String = String::from;
        names.into_iter().map(to_string).zip(self.values)
    }
}

impl<'a> IntoIterator for &'a Record {
    type Item = (&'a str, &'a Value);
    type IntoIter = std::iter::Zip<
        std::iter::Map<std::slice::Iter<'a, Box<str>>, fn(&'a Box<str>) -> &'a str>,
        std::slice::Iter<'a, Value>,
    >;

    fn into_iter(self) -> Self::IntoIter {
        let as_str: fn(&'a Box<str>) -> &'a str = |name| &**name;
        self.names
            .as_slice()
            .iter()
            .map(as_str)
            .zip(self.values.iter())
    }
}

/// Layouts this thread has decoded recently. A scan re-decodes one shape
/// millions of times, and a join alternates between a handful, so four slots
/// keep the hit rate at essentially one while a miss costs one allocation per
/// column — exactly what every decode used to cost.
const CACHED_LAYOUTS: usize = 4;

thread_local! {
    static LAYOUTS: std::cell::RefCell<Vec<Arc<[Box<str>]>>> =
        const { std::cell::RefCell::new(Vec::new()) };
}

/// Matches the column names a payload yields, in order, against the cached
/// layouts, without allocating while they agree.
///
/// Every still-matching candidate has agreed on the same prefix, so one
/// counter tracks how far they all got.
pub(crate) struct LayoutMatcher<'a> {
    cache: &'a mut Vec<Arc<[Box<str>]>>,
    /// Bit `i` set while cache slot `i` still matches.
    alive: u32,
    /// Slot the shared prefix can be read back from, valid while `owned` is
    /// `None` and at least one candidate has ever matched.
    prefix: Option<usize>,
    matched: usize,
    owned: Option<Vec<Box<str>>>,
}

impl<'a> LayoutMatcher<'a> {
    fn new(cache: &'a mut Vec<Arc<[Box<str>]>>) -> Self {
        let alive = if cache.is_empty() {
            0
        } else {
            (1u32 << cache.len()) - 1
        };
        LayoutMatcher {
            cache,
            alive,
            prefix: (alive != 0).then_some(0),
            matched: 0,
            owned: None,
        }
    }

    /// Record the next kept column name. The bytes are only validated as
    /// UTF-8 when no cached layout vouches for them.
    #[inline]
    pub(crate) fn observe(&mut self, name: &[u8]) -> std::result::Result<(), NameError> {
        if let Some(owned) = &mut self.owned {
            if owned.iter().any(|stored| stored.as_bytes() == name) {
                return Err(NameError::Duplicate);
            }
            owned.push(Self::to_name(name)?);
            self.matched += 1;
            return Ok(());
        }
        let mut alive = self.alive;
        let mut still = 0u32;
        let mut first = None;
        while alive != 0 {
            let slot = alive.trailing_zeros() as usize;
            alive &= alive - 1;
            let layout = &self.cache[slot];
            if layout.len() > self.matched && layout[self.matched].as_bytes() == name {
                still |= 1 << slot;
                first.get_or_insert(slot);
            }
        }
        if still == 0 {
            // Nothing agrees any more: keep the prefix every candidate had
            // matched and collect the rest of the names by hand.
            let mut owned = match self.prefix {
                Some(slot) => self.cache[slot][..self.matched].to_vec(),
                None => Vec::with_capacity(self.matched + 1),
            };
            owned.push(Self::to_name(name)?);
            self.owned = Some(owned);
            self.alive = 0;
        } else {
            self.alive = still;
            self.prefix = first;
        }
        self.matched += 1;
        Ok(())
    }

    fn to_name(name: &[u8]) -> std::result::Result<Box<str>, NameError> {
        // Off the matching path nothing has vouched for these bytes yet; on
        // it, they are equal to a name that was validated when it was first
        // decoded, so a scan validates each column name once, not once a row.
        std::str::from_utf8(name)
            .map(|name| name.into())
            .map_err(|_| NameError::InvalidUtf8)
    }

    /// Attach `values` to the layout the payload turned out to have.
    fn finish(self, values: Vec<Value>) -> Record {
        debug_assert_eq!(values.len(), self.matched);
        let LayoutMatcher {
            cache,
            alive,
            prefix,
            matched,
            owned,
        } = self;
        if owned.is_none() {
            let mut bits = alive;
            while bits != 0 {
                let slot = bits.trailing_zeros() as usize;
                bits &= bits - 1;
                if cache[slot].len() == matched {
                    let layout = cache[slot].clone();
                    if slot != 0 {
                        cache[..=slot].rotate_right(1);
                    }
                    return Record::from_layout(layout, values);
                }
            }
        }
        let names = owned.unwrap_or_else(|| match prefix {
            Some(slot) => cache[slot][..matched].to_vec(),
            None => Vec::new(),
        });
        let layout: Arc<[Box<str>]> = Arc::from(names.into_boxed_slice());
        cache.insert(0, layout.clone());
        cache.truncate(CACHED_LAYOUTS);
        Record::from_layout(layout, values)
    }
}

/// The shared layout for a known sequence of column names, taken from the
/// thread-local cache when the shape has been seen before.
///
/// A payload that stores its column names has to rediscover the layout row by
/// row. One that stores only values does not: the names come from the
/// catalog, so a batch of rows resolves the layout once here and hands the
/// same slice to every row it decodes, which costs one atomic increment.
pub(crate) fn layout_for<'a, I>(names: I) -> Arc<[Box<str>]>
where
    I: Iterator<Item = &'a str> + Clone,
{
    LAYOUTS.with(|cache| {
        let mut cache = cache.borrow_mut();
        if let Some(slot) = cache.iter().position(|layout| {
            let mut wanted = names.clone();
            layout.iter().all(|stored| wanted.next() == Some(&**stored)) && wanted.next().is_none()
        }) {
            let layout = cache[slot].clone();
            if slot != 0 {
                cache[..=slot].rotate_right(1);
            }
            return layout;
        }
        let layout: Arc<[Box<str>]> = names
            .map(|name| name.into())
            .collect::<Vec<Box<str>>>()
            .into();
        cache.insert(0, layout.clone());
        cache.truncate(CACHED_LAYOUTS);
        layout
    })
}

/// Why a column name could not be taken at face value.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum NameError {
    Duplicate,
    InvalidUtf8,
}

/// Decode one row through the layout cache: `build` reports every kept column
/// name to the matcher and returns the values in the same order.
pub(crate) fn build_row<E>(
    build: impl FnOnce(&mut LayoutMatcher<'_>) -> std::result::Result<Vec<Value>, E>,
) -> std::result::Result<Record, E> {
    LAYOUTS.with(|cache| {
        let mut cache = cache.borrow_mut();
        let mut matcher = LayoutMatcher::new(&mut cache);
        let values = build(&mut matcher)?;
        Ok(matcher.finish(values))
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn row(pairs: &[(&str, i64)]) -> Record {
        pairs
            .iter()
            .map(|(name, value)| (*name, Value::Int64(*value)))
            .collect()
    }

    #[test]
    fn lookup_insert_remove_behave_like_the_map_this_replaced() {
        let mut record = row(&[("b", 2), ("a", 1)]);
        assert_eq!(record.get("a"), Some(&Value::Int64(1)));
        assert_eq!(record.get("missing"), None);
        assert_eq!(record.insert("a", Value::Int64(9)), Some(Value::Int64(1)));
        assert_eq!(record.insert("c", Value::Int64(3)), None);
        assert_eq!(record.len(), 3);
        assert_eq!(record.remove("b"), Some(Value::Int64(2)));
        assert_eq!(record.remove("b"), None);
        assert_eq!(
            record.keys().collect::<Vec<_>>(),
            vec!["a", "c"],
            "insertion order is preserved"
        );
    }

    #[test]
    fn equality_ignores_column_order() {
        assert_eq!(row(&[("a", 1), ("b", 2)]), row(&[("b", 2), ("a", 1)]));
        assert_ne!(row(&[("a", 1)]), row(&[("a", 1), ("b", 2)]));
        assert_ne!(row(&[("a", 1)]), row(&[("a", 2)]));
    }

    #[test]
    fn mutating_a_shared_layout_does_not_disturb_its_other_rows() {
        let layout: Arc<[Box<str>]> = Arc::from(vec!["a".into(), "b".into()].into_boxed_slice());
        let first = Record::from_layout(layout.clone(), vec![Value::Int64(1), Value::Int64(2)]);
        let mut second = Record::from_layout(layout, vec![Value::Int64(3), Value::Int64(4)]);
        second.insert("c", Value::Int64(5));
        assert_eq!(second.len(), 3);
        assert_eq!(first.len(), 2);
        assert_eq!(first.keys().collect::<Vec<_>>(), vec!["a", "b"]);
    }

    /// The second row of a shape reuses the first row's names.
    #[test]
    fn the_cache_shares_one_layout_between_rows_of_the_same_shape() {
        let names = ["id", "name", "price"];
        let build = |base: i64| {
            build_row::<NameError>(|matcher| {
                let mut values = Vec::new();
                for (at, name) in names.iter().enumerate() {
                    matcher.observe(name.as_bytes())?;
                    values.push(Value::Int64(base + at as i64));
                }
                Ok(values)
            })
            .expect("row")
        };
        let first = build(0);
        let second = build(10);
        let (Names::Shared(a), Names::Shared(b)) = (&first.names, &second.names) else {
            panic!("decoded rows share their names");
        };
        assert!(Arc::ptr_eq(a, b), "the second row reused the layout");
        assert_eq!(second.get("price"), Some(&Value::Int64(12)));
    }

    #[test]
    fn a_different_shape_falls_back_and_is_cached_in_turn() {
        let shapes: [&[&str]; 3] = [&["a", "b"], &["a", "c"], &["a", "b"]];
        let mut rows = Vec::new();
        for shape in shapes {
            rows.push(
                build_row::<NameError>(|matcher| {
                    let mut values = Vec::new();
                    for (at, name) in shape.iter().enumerate() {
                        matcher.observe(name.as_bytes())?;
                        values.push(Value::Int64(at as i64));
                    }
                    Ok(values)
                })
                .expect("row"),
            );
        }
        assert_eq!(rows[0].keys().collect::<Vec<_>>(), vec!["a", "b"]);
        assert_eq!(rows[1].keys().collect::<Vec<_>>(), vec!["a", "c"]);
        let (Names::Shared(first), Names::Shared(third)) = (&rows[0].names, &rows[2].names) else {
            panic!("decoded rows share their names");
        };
        assert!(Arc::ptr_eq(first, third), "both shapes stay cached");
    }

    #[test]
    fn a_shorter_row_than_the_cached_layout_is_not_a_match() {
        let long = build_row::<NameError>(|matcher| {
            matcher.observe(b"a")?;
            matcher.observe(b"b")?;
            Ok(vec![Value::Int64(1), Value::Int64(2)])
        })
        .expect("row");
        let short = build_row::<NameError>(|matcher| {
            matcher.observe(b"a")?;
            Ok(vec![Value::Int64(1)])
        })
        .expect("row");
        assert_eq!(long.len(), 2);
        assert_eq!(short.keys().collect::<Vec<_>>(), vec!["a"]);
    }

    #[test]
    fn a_repeated_column_is_reported() {
        let repeated = build_row::<NameError>(|matcher| {
            matcher.observe(b"zz")?;
            matcher.observe(b"zz")?;
            Ok(Vec::new())
        });
        assert_eq!(repeated.err(), Some(NameError::Duplicate));
        let invalid = build_row::<NameError>(|matcher| {
            matcher.observe(&[0xff, 0xfe])?;
            Ok(Vec::new())
        });
        assert_eq!(invalid.err(), Some(NameError::InvalidUtf8));
    }
}
