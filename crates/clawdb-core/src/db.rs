use std::collections::{BTreeMap, HashMap};
use std::fs::{self, File, OpenOptions};
use std::io::Write;
use std::os::unix::fs::FileExt;
use std::path::{Path, PathBuf};
use std::sync::RwLock;

use ulid::Ulid;

use crate::error::{Error, Result};
use crate::schema::{Catalog, TableSchema, FORMAT_VERSION, ID_COLUMN};
use crate::segment::{
    encode_entry, scan_segment, segment_file_name, EntryRef, SEGMENT_MAX_BYTES,
};
use crate::value::{decode_value, encode_value, read_u16, Value};

const MARKER_FILE: &str = "CLAWDB";
const CATALOG_FILE: &str = "catalog.json";
const SEGMENTS_DIR: &str = "segments";

/// A record is a map from column name to value. On reads the implicit
/// primary key is included under the key `"id"`.
pub type Record = BTreeMap<String, Value>;

/// A stable read position. Reads through a snapshot see the database
/// exactly as it was when the snapshot was taken. Phase 0 keeps every
/// version (no compaction), so snapshots never expire.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub struct Snapshot {
    pub version: u64,
}

/// One version of one record, as tracked by the in-memory primary index.
#[derive(Debug, Clone, Copy)]
struct VersionEntry {
    version: u64,
    segment: u32,
    payload_offset: u64,
    payload_len: u32,
    tombstone: bool,
}

struct ActiveSegment {
    id: u32,
    file: File,
    len: u64,
}

struct State {
    catalog: Catalog,
    /// table -> id -> versions in ascending order.
    index: HashMap<String, BTreeMap<String, Vec<VersionEntry>>>,
    /// Read handles per segment, used with positional reads.
    readers: HashMap<u32, File>,
    active: ActiveSegment,
    next_version: u64,
}

/// An embedded ClawDB database backed by a self-contained directory.
///
/// Phase 0 scope: append-only storage, versioned records with tombstones,
/// ULID primary keys, snapshot reads and an in-memory primary index that is
/// rebuilt on open. No WAL, no fsync, no crash-safety guarantees yet beyond
/// dropping a torn tail entry on open.
pub struct Db {
    dir: PathBuf,
    state: RwLock<State>,
}

impl Db {
    /// Create a new database directory. Fails if one already exists at `path`.
    pub fn create(path: impl AsRef<Path>) -> Result<Db> {
        let dir = path.as_ref().to_path_buf();
        if dir.join(MARKER_FILE).exists() {
            return Err(Error::InvalidArgument(format!(
                "database already exists at {}",
                dir.display()
            )));
        }
        fs::create_dir_all(dir.join(SEGMENTS_DIR))?;
        let mut marker = File::create(dir.join(MARKER_FILE))?;
        marker.write_all(format!("clawdb format_version={FORMAT_VERSION}\n").as_bytes())?;
        Catalog::new().save(&dir.join(CATALOG_FILE))?;

        let seg_path = dir.join(SEGMENTS_DIR).join(segment_file_name(1));
        let file = OpenOptions::new().append(true).create_new(true).open(&seg_path)?;
        let reader = File::open(&seg_path)?;
        let mut readers = HashMap::new();
        readers.insert(1, reader);

        Ok(Db {
            dir,
            state: RwLock::new(State {
                catalog: Catalog::new(),
                index: HashMap::new(),
                readers,
                active: ActiveSegment { id: 1, file, len: 0 },
                next_version: 1,
            }),
        })
    }

    /// Open an existing database, rebuilding the primary index from the
    /// segment files. A torn tail entry in the last segment (partial write
    /// from a killed process) is truncated away.
    pub fn open(path: impl AsRef<Path>) -> Result<Db> {
        let dir = path.as_ref().to_path_buf();
        if !dir.join(MARKER_FILE).exists() {
            return Err(Error::InvalidArgument(format!(
                "not a clawdb database: {}",
                dir.display()
            )));
        }
        let catalog = Catalog::load(&dir.join(CATALOG_FILE))?;

        let seg_dir = dir.join(SEGMENTS_DIR);
        let mut segment_ids: Vec<u32> = Vec::new();
        for dirent in fs::read_dir(&seg_dir)? {
            let name = dirent?.file_name();
            let name = name.to_string_lossy();
            if let Some(stem) = name.strip_suffix(".seg") {
                if let Ok(id) = stem.parse::<u32>() {
                    segment_ids.push(id);
                }
            }
        }
        segment_ids.sort_unstable();
        if segment_ids.is_empty() {
            let seg_path = seg_dir.join(segment_file_name(1));
            OpenOptions::new().append(true).create_new(true).open(&seg_path)?;
            segment_ids.push(1);
        }

        let mut entries: Vec<(u32, EntryRef)> = Vec::new();
        let mut readers = HashMap::new();
        let mut active_len = 0u64;
        let last_id = *segment_ids.last().expect("non-empty");
        for &seg_id in &segment_ids {
            let seg_path = seg_dir.join(segment_file_name(seg_id));
            let data = fs::read(&seg_path)?;
            let mut segment_entries = Vec::new();
            let outcome = scan_segment(&data, &mut segment_entries);
            if !outcome.clean {
                if seg_id == last_id {
                    // Torn tail from a killed process: drop the partial entry.
                    let f = OpenOptions::new().write(true).open(&seg_path)?;
                    f.set_len(outcome.valid_len)?;
                } else {
                    return Err(Error::Corrupt(format!(
                        "segment {} has an invalid entry before the tail",
                        segment_file_name(seg_id)
                    )));
                }
            }
            if seg_id == last_id {
                active_len = outcome.valid_len;
            }
            entries.extend(segment_entries.into_iter().map(|e| (seg_id, e)));
            readers.insert(seg_id, File::open(&seg_path)?);
        }

        // Segments are scanned in id order and entries within a segment are
        // in write order, so versions arrive ascending; apply sequentially.
        let mut index: HashMap<String, BTreeMap<String, Vec<VersionEntry>>> = HashMap::new();
        let mut max_version = 0u64;
        for (seg_id, e) in entries {
            max_version = max_version.max(e.version);
            index
                .entry(e.table)
                .or_default()
                .entry(e.id)
                .or_default()
                .push(VersionEntry {
                    version: e.version,
                    segment: seg_id,
                    payload_offset: e.payload_offset,
                    payload_len: e.payload_len,
                    tombstone: e.tombstone,
                });
        }

        let active_path = seg_dir.join(segment_file_name(last_id));
        let file = OpenOptions::new().append(true).open(&active_path)?;

        Ok(Db {
            dir,
            state: RwLock::new(State {
                catalog,
                index,
                readers,
                active: ActiveSegment {
                    id: last_id,
                    file,
                    len: active_len,
                },
                next_version: max_version + 1,
            }),
        })
    }

    pub fn open_or_create(path: impl AsRef<Path>) -> Result<Db> {
        if path.as_ref().join(MARKER_FILE).exists() {
            Db::open(path)
        } else {
            Db::create(path)
        }
    }

    pub fn create_table(&self, schema: TableSchema) -> Result<()> {
        schema.validate()?;
        let mut st = self.state.write().unwrap();
        if st.catalog.table(&schema.name).is_some() {
            return Err(Error::TableExists(schema.name));
        }
        st.catalog.tables.push(schema);
        st.catalog.save(&self.dir.join(CATALOG_FILE))
    }

    pub fn tables(&self) -> Vec<String> {
        let st = self.state.read().unwrap();
        st.catalog.tables.iter().map(|t| t.name.clone()).collect()
    }

    /// Insert a record. If the record carries an `"id"` text value it is used
    /// as the primary key; otherwise the engine generates a ULID. Returns the id.
    pub fn insert(&self, table: &str, record: Record) -> Result<String> {
        let mut st = self.state.write().unwrap();
        let schema = st
            .catalog
            .table(table)
            .ok_or_else(|| Error::TableNotFound(table.into()))?
            .clone();

        let id = match record.get(ID_COLUMN) {
            None => Ulid::new().to_string(),
            Some(Value::Text(s)) if !s.is_empty() => s.clone(),
            Some(Value::Text(_)) => {
                return Err(Error::InvalidArgument("id must not be empty".into()))
            }
            Some(_) => {
                return Err(Error::SchemaViolation("id must be a text value".into()))
            }
        };

        if let Some(last) = latest_version(&st, table, &id) {
            if !last.tombstone {
                return Err(Error::DuplicateId {
                    table: table.into(),
                    id,
                });
            }
        }

        let stored = build_stored(&schema, record)?;
        let payload = encode_record(&stored);
        let entry = self.append(&mut st, table, &id, Some(&payload))?;
        st.index
            .entry(table.to_owned())
            .or_default()
            .entry(id.clone())
            .or_default()
            .push(entry);
        Ok(id)
    }

    /// Latest visible version of a record, or `None` if absent or deleted.
    pub fn get(&self, table: &str, id: &str) -> Result<Option<Record>> {
        self.get_visible(table, id, u64::MAX)
    }

    /// Read a record as of a snapshot.
    pub fn get_at(&self, snapshot: Snapshot, table: &str, id: &str) -> Result<Option<Record>> {
        self.get_visible(table, id, snapshot.version)
    }

    /// Apply a partial patch to an existing record. The full new version is
    /// appended; prior versions stay readable through older snapshots.
    pub fn update(&self, table: &str, id: &str, patch: Record) -> Result<()> {
        let mut st = self.state.write().unwrap();
        let schema = st
            .catalog
            .table(table)
            .ok_or_else(|| Error::TableNotFound(table.into()))?
            .clone();
        if patch.contains_key(ID_COLUMN) {
            return Err(Error::InvalidArgument(
                "the primary key cannot be updated".into(),
            ));
        }
        let entry = match visible_version(&st, table, id, u64::MAX) {
            Some(e) => e,
            None => {
                return Err(Error::RecordNotFound {
                    table: table.into(),
                    id: id.into(),
                })
            }
        };
        let mut current = read_record(&st, &entry)?;
        for (name, value) in patch {
            let col = schema.column(&name).ok_or_else(|| {
                Error::SchemaViolation(format!("unknown column '{name}'"))
            })?;
            check_value(col, &value)?;
            current.insert(name, value);
        }
        let stored: Vec<(String, Value)> = schema
            .columns
            .iter()
            .map(|c| {
                let v = current.remove(&c.name).unwrap_or(Value::Null);
                (c.name.clone(), v)
            })
            .collect();
        let payload = encode_record(&stored);
        let entry = self.append(&mut st, table, id, Some(&payload))?;
        st.index
            .entry(table.to_owned())
            .or_default()
            .entry(id.to_owned())
            .or_default()
            .push(entry);
        Ok(())
    }

    /// Delete a record by appending a tombstone. Returns false if the record
    /// does not exist (or is already deleted). Older snapshots still see it.
    pub fn delete(&self, table: &str, id: &str) -> Result<bool> {
        let mut st = self.state.write().unwrap();
        if st.catalog.table(table).is_none() {
            return Err(Error::TableNotFound(table.into()));
        }
        if visible_version(&st, table, id, u64::MAX).is_none() {
            return Ok(false);
        }
        let entry = self.append(&mut st, table, id, None)?;
        st.index
            .entry(table.to_owned())
            .or_default()
            .entry(id.to_owned())
            .or_default()
            .push(entry);
        Ok(true)
    }

    /// All visible records of a table, ordered by id.
    pub fn scan(&self, table: &str) -> Result<Vec<(String, Record)>> {
        self.scan_visible(table, u64::MAX)
    }

    /// All records of a table as of a snapshot, ordered by id.
    pub fn scan_at(&self, snapshot: Snapshot, table: &str) -> Result<Vec<(String, Record)>> {
        self.scan_visible(table, snapshot.version)
    }

    /// Take a stable read position at the current committed version.
    pub fn snapshot(&self) -> Snapshot {
        let st = self.state.read().unwrap();
        Snapshot {
            version: st.next_version - 1,
        }
    }

    // --- internals ---------------------------------------------------------

    fn get_visible(&self, table: &str, id: &str, max_version: u64) -> Result<Option<Record>> {
        let st = self.state.read().unwrap();
        if st.catalog.table(table).is_none() {
            return Err(Error::TableNotFound(table.into()));
        }
        match visible_version(&st, table, id, max_version) {
            None => Ok(None),
            Some(entry) => {
                let mut record = read_record(&st, &entry)?;
                record.insert(ID_COLUMN.to_owned(), Value::Text(id.to_owned()));
                Ok(Some(record))
            }
        }
    }

    fn scan_visible(&self, table: &str, max_version: u64) -> Result<Vec<(String, Record)>> {
        let st = self.state.read().unwrap();
        if st.catalog.table(table).is_none() {
            return Err(Error::TableNotFound(table.into()));
        }
        let mut out = Vec::new();
        if let Some(ids) = st.index.get(table) {
            for (id, versions) in ids {
                let entry = versions.iter().rev().find(|e| e.version <= max_version);
                if let Some(entry) = entry {
                    if !entry.tombstone {
                        let mut record = read_record(&st, entry)?;
                        record.insert(ID_COLUMN.to_owned(), Value::Text(id.clone()));
                        out.push((id.clone(), record));
                    }
                }
            }
        }
        Ok(out)
    }

    /// Append one entry to the active segment, rotating first if it is full.
    fn append(
        &self,
        st: &mut State,
        table: &str,
        id: &str,
        payload: Option<&[u8]>,
    ) -> Result<VersionEntry> {
        if st.active.len >= SEGMENT_MAX_BYTES {
            let next_id = st.active.id + 1;
            let seg_path = self
                .dir
                .join(SEGMENTS_DIR)
                .join(segment_file_name(next_id));
            let file = OpenOptions::new().append(true).create_new(true).open(&seg_path)?;
            st.readers.insert(next_id, File::open(&seg_path)?);
            st.active = ActiveSegment {
                id: next_id,
                file,
                len: 0,
            };
        }
        let version = st.next_version;
        let (buf, payload_rel) = encode_entry(version, table, id, payload);
        st.active.file.write_all(&buf)?;
        let entry = VersionEntry {
            version,
            segment: st.active.id,
            payload_offset: st.active.len + payload_rel,
            payload_len: payload.map_or(0, |p| p.len() as u32),
            tombstone: payload.is_none(),
        };
        st.active.len += buf.len() as u64;
        st.next_version += 1;
        Ok(entry)
    }
}

fn latest_version(st: &State, table: &str, id: &str) -> Option<VersionEntry> {
    st.index
        .get(table)
        .and_then(|ids| ids.get(id))
        .and_then(|versions| versions.last())
        .copied()
}

fn visible_version(st: &State, table: &str, id: &str, max_version: u64) -> Option<VersionEntry> {
    st.index
        .get(table)
        .and_then(|ids| ids.get(id))
        .and_then(|versions| versions.iter().rev().find(|e| e.version <= max_version))
        .filter(|e| !e.tombstone)
        .copied()
}

fn read_record(st: &State, entry: &VersionEntry) -> Result<Record> {
    let reader = st
        .readers
        .get(&entry.segment)
        .ok_or_else(|| Error::Corrupt(format!("missing segment {}", entry.segment)))?;
    let mut buf = vec![0u8; entry.payload_len as usize];
    reader.read_exact_at(&mut buf, entry.payload_offset)?;
    decode_record(&buf)
}

fn check_value(col: &crate::schema::Column, value: &Value) -> Result<()> {
    if value.is_null() {
        if !col.nullable {
            return Err(Error::SchemaViolation(format!(
                "column '{}' is not nullable",
                col.name
            )));
        }
        return Ok(());
    }
    if !value.matches(col.ty) {
        return Err(Error::SchemaViolation(format!(
            "column '{}' expects {}, got {:?}",
            col.name, col.ty, value
        )));
    }
    Ok(())
}

/// Validate an insert against the schema and produce the stored field list
/// in schema column order. Missing nullable columns are stored as Null.
fn build_stored(schema: &TableSchema, mut record: Record) -> Result<Vec<(String, Value)>> {
    record.remove(ID_COLUMN);
    for name in record.keys() {
        if schema.column(name).is_none() {
            return Err(Error::SchemaViolation(format!("unknown column '{name}'")));
        }
    }
    let mut stored = Vec::with_capacity(schema.columns.len());
    for col in &schema.columns {
        let value = record.remove(&col.name).unwrap_or(Value::Null);
        check_value(col, &value)?;
        stored.push((col.name.clone(), value));
    }
    Ok(stored)
}

// Record payload layout: u16 field count, then per field a u16-length-prefixed
// column name followed by a tagged value. Self-describing so that schema
// evolution in later phases does not invalidate old segments.

fn encode_record(fields: &[(String, Value)]) -> Vec<u8> {
    let mut buf = Vec::with_capacity(64);
    buf.extend_from_slice(&(fields.len() as u16).to_le_bytes());
    for (name, value) in fields {
        buf.extend_from_slice(&(name.len() as u16).to_le_bytes());
        buf.extend_from_slice(name.as_bytes());
        encode_value(&mut buf, value);
    }
    buf
}

fn decode_record(buf: &[u8]) -> Result<Record> {
    let mut pos = 0usize;
    let count = read_u16(buf, &mut pos)? as usize;
    let mut record = Record::new();
    for _ in 0..count {
        let name_len = read_u16(buf, &mut pos)? as usize;
        let end = pos
            .checked_add(name_len)
            .ok_or_else(|| Error::Corrupt("length overflow".into()))?;
        let name_bytes = buf
            .get(pos..end)
            .ok_or_else(|| Error::Corrupt("unexpected end of record".into()))?;
        pos = end;
        let name = std::str::from_utf8(name_bytes)
            .map_err(|_| Error::Corrupt("invalid utf8 in column name".into()))?
            .to_owned();
        let value = decode_value(buf, &mut pos)?;
        record.insert(name, value);
    }
    Ok(record)
}
