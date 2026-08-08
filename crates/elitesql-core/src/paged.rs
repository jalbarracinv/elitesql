//! Immutable, mmap-backed sorted key/value runs shared by derived indexes.
//!
//! Only the compact page directory is resident. Data pages are checksummed and
//! validated when touched, allowing the operating system to reclaim cold pages.

use std::cmp::Ordering;
use std::fs::{File, OpenOptions};
use std::io::{BufWriter, Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};

use memmap2::{Advice, Mmap, MmapOptions};
use ulid::Ulid;

use crate::error::{Error, Result};

const MAGIC: &[u8; 8] = b"ESQLPAGE";
const DIRECTORY_MAGIC_V1: &[u8; 4] = b"DIR1";
const DIRECTORY_MAGIC_V2: &[u8; 4] = b"DIR2";
const FORMAT_V1: u32 = 1;
const FORMAT: u32 = 2;
const HEADER_LEN: usize = 48;
const DEFAULT_PAGE_SIZE: usize = 4096;
const WRITER_BUFFER_SIZE: usize = 1024 * 1024;
const MAX_MERGE_FAN_IN: usize = 32;

#[derive(Clone, Copy)]
struct PageView<'a> {
    offset: usize,
    payload_offset: usize,
    payload_len: usize,
    first_key: &'a [u8],
    last_key: &'a [u8],
}

/// Read-only sorted run. The mmap is the page cache; only `pages` occupies
/// mandatory heap memory.
pub(crate) struct PagedIndex {
    mmap: Mmap,
    /// Offsets of page-directory records inside `mmap` (variable-length in V1
    /// and fixed-width in V2). Keys remain file-backed instead of being copied
    /// into two heap allocations per page.
    pages: Vec<usize>,
    format: u32,
    dump_version: u64,
    entry_count: u64,
}

pub(crate) struct PagedPrefixCursor<'a> {
    index: &'a PagedIndex,
    prefix: Vec<u8>,
    page_index: usize,
    pos: usize,
    end: usize,
    done: bool,
}

impl PagedIndex {
    pub(crate) fn open(path: &Path) -> Result<Self> {
        let file = File::open(path)?;
        // SAFETY: derived index files are immutable. Publishers write a new
        // inode and atomically rename it before readers open the mapping.
        let mmap = unsafe { MmapOptions::new().map(&file) }?;
        if mmap.len() < HEADER_LEN || &mmap[..8] != MAGIC {
            return Err(Error::Corrupt("paged index: bad header".into()));
        }
        let format = read_u32(&mmap, 8)?;
        if format != FORMAT_V1 && format != FORMAT {
            return Err(Error::Corrupt("paged index: unsupported format".into()));
        }
        let header_crc = read_u32(&mmap, 44)?;
        if crc32fast::hash(&mmap[..44]) != header_crc {
            return Err(Error::Corrupt("paged index: header crc mismatch".into()));
        }
        let page_size = read_u32(&mmap, 12)? as usize;
        if !(256..=1024 * 1024).contains(&page_size) {
            return Err(Error::Corrupt("paged index: invalid page size".into()));
        }
        let dump_version = read_u64(&mmap, 16)?;
        let entry_count = read_u64(&mmap, 24)?;
        let directory_offset = usize::try_from(read_u64(&mmap, 32)?)
            .map_err(|_| Error::Corrupt("paged index: directory overflow".into()))?;
        let directory_count = read_u32(&mmap, 40)? as usize;
        let directory_magic = if format == FORMAT_V1 {
            DIRECTORY_MAGIC_V1
        } else {
            DIRECTORY_MAGIC_V2
        };
        if mmap.get(directory_offset..directory_offset.saturating_add(4)) != Some(directory_magic) {
            return Err(Error::Corrupt("paged index: bad directory".into()));
        }
        let mut pos = directory_offset + 4;
        let mut pages = Vec::with_capacity(directory_count);
        if format == FORMAT_V1 {
            for _ in 0..directory_count {
                let directory_entry = pos;
                let offset = usize::try_from(read_u64_at(&mmap, &mut pos)?)
                    .map_err(|_| Error::Corrupt("paged index: page offset overflow".into()))?;
                let payload_len = read_u32_at(&mmap, &mut pos)? as usize;
                let first_len = read_u32_at(&mmap, &mut pos)? as usize;
                let last_len = read_u32_at(&mmap, &mut pos)? as usize;
                let first_key = take(&mmap, &mut pos, first_len)?;
                let last_key = take(&mmap, &mut pos, last_len)?;
                if first_key > last_key
                    || offset < HEADER_LEN
                    || offset
                        .checked_add(8 + payload_len)
                        .is_none_or(|end| end > directory_offset)
                {
                    return Err(Error::Corrupt("paged index: invalid page directory".into()));
                }
                pages.push(directory_entry);
            }
        } else {
            for _ in 0..directory_count {
                let directory_entry = pos;
                let offset = usize::try_from(read_u64_at(&mmap, &mut pos)?)
                    .map_err(|_| Error::Corrupt("paged index: page offset overflow".into()))?;
                if offset < HEADER_LEN
                    || offset
                        .checked_add(16)
                        .is_none_or(|end| end > directory_offset)
                {
                    return Err(Error::Corrupt("paged index: invalid page directory".into()));
                }
                let payload_len = read_u32(&mmap, offset)? as usize;
                let first_len = read_u32(&mmap, offset + 8)? as usize;
                let last_len = read_u32(&mmap, offset + 12)? as usize;
                let first_start = offset
                    .checked_add(16)
                    .ok_or_else(|| Error::Corrupt("paged index: page overflow".into()))?;
                let first_end = first_start
                    .checked_add(first_len)
                    .ok_or_else(|| Error::Corrupt("paged index: page overflow".into()))?;
                let last_end = first_end
                    .checked_add(last_len)
                    .ok_or_else(|| Error::Corrupt("paged index: page overflow".into()))?;
                last_end
                    .checked_add(payload_len)
                    .filter(|end| *end <= directory_offset)
                    .ok_or_else(|| Error::Corrupt("paged index: invalid page directory".into()))?;
                let first_key = mmap
                    .get(first_start..first_end)
                    .ok_or_else(|| Error::Corrupt("paged index: truncated first key".into()))?;
                let last_key = mmap
                    .get(first_end..last_end)
                    .ok_or_else(|| Error::Corrupt("paged index: truncated last key".into()))?;
                if first_key > last_key {
                    return Err(Error::Corrupt("paged index: invalid page directory".into()));
                }
                pages.push(directory_entry);
            }
        }
        if pos != mmap.len() {
            return Err(Error::Corrupt(
                "paged index: trailing directory bytes".into(),
            ));
        }
        for pair in pages.windows(2) {
            if page_view(&mmap, format, pair[0]).first_key
                > page_view(&mmap, format, pair[1]).first_key
            {
                return Err(Error::Corrupt("paged index: unsorted directory".into()));
            }
        }
        let _ = mmap.advise(Advice::Random);
        Ok(Self {
            mmap,
            pages,
            format,
            dump_version,
            entry_count,
        })
    }

    pub(crate) fn dump_version(&self) -> u64 {
        self.dump_version
    }

    fn first_key(&self) -> Option<&[u8]> {
        self.pages.first().map(|page| self.page(*page).first_key)
    }

    fn last_key(&self) -> Option<&[u8]> {
        self.pages.last().map(|page| self.page(*page).last_key)
    }

    pub(crate) fn may_contain_key(&self, key: &[u8]) -> bool {
        self.pages.first().is_some_and(|first| {
            self.page(*first).first_key <= key
                && self
                    .pages
                    .last()
                    .is_some_and(|last| key <= self.page(*last).last_key)
        })
    }

    pub(crate) fn may_contain_prefix(&self, prefix: &[u8]) -> bool {
        self.pages.first().is_some_and(|first| {
            (self.page(*first).first_key <= prefix
                || self.page(*first).first_key.starts_with(prefix))
                && self
                    .pages
                    .last()
                    .is_some_and(|last| self.page(*last).last_key >= prefix)
        })
    }

    fn page(&self, directory_entry: usize) -> PageView<'_> {
        page_view(&self.mmap, self.format, directory_entry)
    }

    /// Visit values for one key in sorted order. Returning `false` stops at
    /// once, apart from the already checksummed page currently being parsed.
    pub(crate) fn visit_key(
        &self,
        key: &[u8],
        mut visit: impl FnMut(&[u8]) -> Result<bool>,
    ) -> Result<()> {
        let first = self
            .pages
            .partition_point(|page| self.page(*page).last_key < key);
        for directory_entry in &self.pages[first..] {
            let page = self.page(*directory_entry);
            if page.first_key > key {
                break;
            }
            if page.last_key < key {
                continue;
            }
            let keep_going = self.scan_page_until(page, |candidate, value| {
                if candidate == key {
                    visit(value)
                } else {
                    Ok(true)
                }
            })?;
            if !keep_going {
                break;
            }
        }
        Ok(())
    }

    pub(crate) fn prefix_cursor(&self, prefix: &[u8]) -> PagedPrefixCursor<'_> {
        let page_index = self
            .pages
            .partition_point(|page| self.page(*page).last_key < prefix);
        PagedPrefixCursor {
            index: self,
            prefix: prefix.to_vec(),
            page_index,
            pos: 0,
            end: 0,
            done: false,
        }
    }

    pub(crate) fn scan(&self, mut visit: impl FnMut(&[u8], &[u8]) -> Result<()>) -> Result<()> {
        for directory_entry in &self.pages {
            let page = self.page(*directory_entry);
            self.scan_page(page, &mut visit)?;
        }
        Ok(())
    }

    fn scan_page(
        &self,
        page: PageView<'_>,
        mut visit: impl FnMut(&[u8], &[u8]) -> Result<()>,
    ) -> Result<()> {
        let stored_len = read_u32(&self.mmap, page.offset)? as usize;
        let stored_crc = read_u32(&self.mmap, page.offset + 4)?;
        if stored_len != page.payload_len {
            return Err(Error::Corrupt("paged index: page length mismatch".into()));
        }
        let payload = self
            .mmap
            .get(page.payload_offset..page.payload_offset + page.payload_len)
            .ok_or_else(|| Error::Corrupt("paged index: truncated page".into()))?;
        if crc32fast::hash(payload) != stored_crc {
            return Err(Error::Corrupt("paged index: page crc mismatch".into()));
        }
        let mut pos = 0usize;
        while pos < payload.len() {
            let key_len = read_u32_at(payload, &mut pos)? as usize;
            let value_len = read_u32_at(payload, &mut pos)? as usize;
            let key = take(payload, &mut pos, key_len)?;
            let value = take(payload, &mut pos, value_len)?;
            visit(key, value)?;
        }
        Ok(())
    }

    fn scan_page_until(
        &self,
        page: PageView<'_>,
        mut visit: impl FnMut(&[u8], &[u8]) -> Result<bool>,
    ) -> Result<bool> {
        let stored_len = read_u32(&self.mmap, page.offset)? as usize;
        let stored_crc = read_u32(&self.mmap, page.offset + 4)?;
        if stored_len != page.payload_len {
            return Err(Error::Corrupt("paged index: page length mismatch".into()));
        }
        let payload = self
            .mmap
            .get(page.payload_offset..page.payload_offset + page.payload_len)
            .ok_or_else(|| Error::Corrupt("paged index: truncated page".into()))?;
        if crc32fast::hash(payload) != stored_crc {
            return Err(Error::Corrupt("paged index: page crc mismatch".into()));
        }
        let mut pos = 0usize;
        while pos < payload.len() {
            let key_len = read_u32_at(payload, &mut pos)? as usize;
            let value_len = read_u32_at(payload, &mut pos)? as usize;
            let key = take(payload, &mut pos, key_len)?;
            let value = take(payload, &mut pos, value_len)?;
            if !visit(key, value)? {
                return Ok(false);
            }
        }
        Ok(true)
    }
}

impl<'a> PagedPrefixCursor<'a> {
    pub(crate) fn next(&mut self) -> Result<Option<(&'a [u8], &'a [u8])>> {
        while !self.done && (self.pos < self.end || self.page_index < self.index.pages.len()) {
            if self.pos == self.end {
                let page = self.index.page(self.index.pages[self.page_index]);
                if page.first_key > self.prefix.as_slice()
                    && !page.first_key.starts_with(&self.prefix)
                {
                    self.done = true;
                    break;
                }
                let stored_len = read_u32(&self.index.mmap, page.offset)? as usize;
                let stored_crc = read_u32(&self.index.mmap, page.offset + 4)?;
                if stored_len != page.payload_len {
                    return Err(Error::Corrupt("paged index: page length mismatch".into()));
                }
                let start = page.payload_offset;
                let end = start + page.payload_len;
                let payload = self
                    .index
                    .mmap
                    .get(start..end)
                    .ok_or_else(|| Error::Corrupt("paged index: truncated page".into()))?;
                if crc32fast::hash(payload) != stored_crc {
                    return Err(Error::Corrupt("paged index: page crc mismatch".into()));
                }
                self.pos = start;
                self.end = end;
                self.page_index += 1;
            }

            while self.pos < self.end {
                let key_len = read_u32_at(&self.index.mmap, &mut self.pos)? as usize;
                let value_len = read_u32_at(&self.index.mmap, &mut self.pos)? as usize;
                if self
                    .pos
                    .checked_add(key_len)
                    .and_then(|end| end.checked_add(value_len))
                    .is_none_or(|end| end > self.end)
                {
                    return Err(Error::Corrupt("paged index: truncated page entry".into()));
                }
                let key = take(&self.index.mmap, &mut self.pos, key_len)?;
                let value = take(&self.index.mmap, &mut self.pos, value_len)?;
                if key.starts_with(&self.prefix) {
                    return Ok(Some((key, value)));
                }
                if key > self.prefix.as_slice() {
                    self.done = true;
                    return Ok(None);
                }
            }
        }
        Ok(None)
    }
}

fn page_view(mmap: &[u8], format: u32, directory_entry: usize) -> PageView<'_> {
    // Every directory record and conversion is validated exhaustively by
    // `PagedIndex::open`; immutable mmap contents cannot change afterwards.
    let offset = usize::try_from(read_u64(mmap, directory_entry).expect("validated page offset"))
        .expect("validated page offset fits usize");
    let (payload_len, first_len, last_len, first_start) = if format == FORMAT_V1 {
        (
            read_u32(mmap, directory_entry + 8).expect("validated page length") as usize,
            read_u32(mmap, directory_entry + 12).expect("validated first-key length") as usize,
            read_u32(mmap, directory_entry + 16).expect("validated last-key length") as usize,
            directory_entry + 20,
        )
    } else {
        (
            read_u32(mmap, offset).expect("validated page length") as usize,
            read_u32(mmap, offset + 8).expect("validated first-key length") as usize,
            read_u32(mmap, offset + 12).expect("validated last-key length") as usize,
            offset + 16,
        )
    };
    let first_end = first_start + first_len;
    let last_end = first_end + last_len;
    let payload_offset = if format == FORMAT_V1 {
        offset + 8
    } else {
        last_end
    };
    PageView {
        offset,
        payload_offset,
        payload_len,
        first_key: &mmap[first_start..first_end],
        last_key: &mmap[first_end..last_end],
    }
}

/// Streaming writer for entries already sorted by `(key, value)`.
pub(crate) struct PagedWriter {
    file: BufWriter<File>,
    page_size: usize,
    page: Vec<u8>,
    page_first: Option<Vec<u8>>,
    page_last: Vec<u8>,
    previous_key: Vec<u8>,
    previous_value: Vec<u8>,
    has_previous: bool,
    pages: Vec<u64>,
    dump_version: u64,
    entry_count: u64,
}

/// Memory-bounded external sorter feeding a [`PagedWriter`]. Input entries may
/// arrive in any order; sorted temporary runs are merged into the final file.
pub(crate) struct ExternalPagedWriter {
    target: PathBuf,
    temp_dir: PathBuf,
    dump_version: u64,
    budget: usize,
    buffered_bytes: usize,
    buffer: Vec<(Vec<u8>, Vec<u8>)>,
    runs: Vec<PathBuf>,
}

impl ExternalPagedWriter {
    pub(crate) fn new(
        target: &Path,
        temp_dir: &Path,
        dump_version: u64,
        budget: usize,
    ) -> Result<Self> {
        std::fs::create_dir_all(temp_dir)?;
        Ok(Self {
            target: target.to_path_buf(),
            temp_dir: temp_dir.to_path_buf(),
            dump_version,
            budget: budget.max(1),
            buffered_bytes: 0,
            buffer: Vec::new(),
            runs: Vec::new(),
        })
    }

    pub(crate) fn add(&mut self, key: &[u8], value: &[u8]) -> Result<()> {
        let bytes = key.len().saturating_add(value.len()).saturating_add(48);
        if !self.buffer.is_empty() && self.buffered_bytes.saturating_add(bytes) > self.budget {
            self.flush_run()?;
        }
        self.buffer.push((key.to_vec(), value.to_vec()));
        self.buffered_bytes = self.buffered_bytes.saturating_add(bytes);
        if self.buffered_bytes >= self.budget {
            self.flush_run()?;
        }
        Ok(())
    }

    /// Change the generation before publication. Entries and temporary runs
    /// are generation-independent; compaction uses this after the final
    /// output-segment length becomes known.
    pub(crate) fn set_dump_version(&mut self, dump_version: u64) {
        self.dump_version = dump_version;
    }

    pub(crate) fn finish(mut self) -> Result<()> {
        self.flush_run()?;
        self.collapse_runs()?;
        let mut writer = PagedWriter::create(&self.target, self.dump_version, None)?;
        merge_external_runs(&self.runs, |key, value| writer.add(key, value))?;
        writer.finish()?;
        self.cleanup();
        Ok(())
    }

    fn collapse_runs(&mut self) -> Result<()> {
        while self.runs.len() > MAX_MERGE_FAN_IN {
            let old = self.runs.clone();
            let mut merged = Vec::new();
            for group in old.chunks(MAX_MERGE_FAN_IN) {
                let path = self
                    .temp_dir
                    .join(format!("paged-merge-{}.run", Ulid::new()));
                // Register before writing so Drop also cleans a partial run.
                self.runs.push(path.clone());
                let mut file = File::create(&path)?;
                merge_external_runs(group, |key, value| {
                    file.write_all(&(key.len() as u32).to_le_bytes())?;
                    file.write_all(&(value.len() as u32).to_le_bytes())?;
                    file.write_all(key)?;
                    file.write_all(value)?;
                    Ok(())
                })?;
                merged.push(path);
            }
            for path in &old {
                let _ = std::fs::remove_file(path);
            }
            self.runs = merged;
        }
        Ok(())
    }

    fn flush_run(&mut self) -> Result<()> {
        if self.buffer.is_empty() {
            return Ok(());
        }
        self.buffer.sort_unstable();
        self.buffer.dedup();
        let path = self
            .temp_dir
            .join(format!("paged-sort-{}.run", Ulid::new()));
        let mut file = File::create(&path)?;
        for (key, value) in &self.buffer {
            file.write_all(&(key.len() as u32).to_le_bytes())?;
            file.write_all(&(value.len() as u32).to_le_bytes())?;
            file.write_all(key)?;
            file.write_all(value)?;
        }
        self.runs.push(path);
        self.buffer.clear();
        self.buffered_bytes = 0;
        Ok(())
    }

    fn cleanup(&mut self) {
        for path in self.runs.drain(..) {
            let _ = std::fs::remove_file(path);
        }
    }
}

impl Drop for ExternalPagedWriter {
    fn drop(&mut self) {
        self.cleanup();
    }
}

/// Merge already-sorted immutable paged runs directly into another paged
/// run. Only one head entry per input is retained; unlike
/// [`ExternalPagedWriter`], this performs no re-sorting or temporary spill.
pub(crate) fn merge_paged_indexes(
    target: &Path,
    inputs: &[&PagedIndex],
    dump_version: u64,
) -> Result<()> {
    let mut key_ordered: Vec<_> = inputs
        .iter()
        .copied()
        .filter(|index| !index.pages.is_empty())
        .collect();
    key_ordered.sort_unstable_by(|left, right| left.first_key().cmp(&right.first_key()));
    let disjoint_v2 = key_ordered.iter().all(|index| index.format == FORMAT)
        && key_ordered.windows(2).all(|pair| {
            pair[0]
                .last_key()
                .zip(pair[1].first_key())
                .is_some_and(|(left, right)| left < right)
        });
    if disjoint_v2 {
        let mut writer = PagedWriter::create(target, dump_version, None)?;
        for index in key_ordered {
            writer.append_v2_pages(index)?;
        }
        return writer.finish();
    }

    let mut cursors: Vec<_> = inputs
        .iter()
        .map(|index| index.prefix_cursor(&[]))
        .collect();
    let mut heads: Vec<_> = cursors
        .iter_mut()
        .map(PagedPrefixCursor::next)
        .collect::<Result<_>>()?;
    let mut writer = PagedWriter::create(target, dump_version, None)?;
    let mut previous_key = Vec::new();
    let mut previous_value = Vec::new();
    let mut has_previous = false;
    loop {
        let next = heads
            .iter()
            .enumerate()
            .filter_map(|(index, entry)| entry.map(|entry| (index, entry)))
            .min_by(|(_, left), (_, right)| left.cmp(right))
            .map(|(index, _)| index);
        let Some(index) = next else { break };
        let (key, value) = heads[index].take().expect("selected run has a head");
        if !has_previous || previous_key.as_slice() != key || previous_value.as_slice() != value {
            writer.add(key, value)?;
            previous_key.clear();
            previous_key.extend_from_slice(key);
            previous_value.clear();
            previous_value.extend_from_slice(value);
            has_previous = true;
        }
        heads[index] = cursors[index].next()?;
    }
    writer.finish()
}

/// Merge sorted operation runs while retaining only the lexicographically
/// greatest value for each key. Mutation indexes encode the operation version
/// first in the value, so this discards superseded adds/tombstones without
/// materializing the key space.
pub(crate) fn merge_paged_indexes_latest_value(
    target: &Path,
    inputs: &[&PagedIndex],
    dump_version: u64,
) -> Result<()> {
    let mut cursors: Vec<_> = inputs
        .iter()
        .map(|index| index.prefix_cursor(&[]))
        .collect();
    let mut heads: Vec<_> = cursors
        .iter_mut()
        .map(PagedPrefixCursor::next)
        .collect::<Result<_>>()?;
    let mut writer = PagedWriter::create(target, dump_version, None)?;
    let mut key = Vec::new();
    let mut newest = Vec::new();
    loop {
        let Some(next_key) = heads
            .iter()
            .filter_map(|entry| entry.map(|(key, _)| key))
            .min()
        else {
            break;
        };
        key.clear();
        key.extend_from_slice(next_key);
        newest.clear();
        let mut have_newest = false;
        for index in 0..heads.len() {
            while heads[index].is_some_and(|(candidate, _)| candidate == key.as_slice()) {
                let (_, value) = heads[index].take().expect("matching head");
                if !have_newest || value > newest.as_slice() {
                    newest.clear();
                    newest.extend_from_slice(value);
                    have_newest = true;
                }
                heads[index] = cursors[index].next()?;
            }
        }
        debug_assert!(have_newest);
        writer.add(&key, &newest)?;
    }
    writer.finish()
}

struct ExternalRunReader {
    reader: File,
}

impl ExternalRunReader {
    fn open(path: &Path) -> Result<Self> {
        Ok(Self {
            reader: File::open(path)?,
        })
    }

    fn next(&mut self) -> Result<Option<(Vec<u8>, Vec<u8>)>> {
        let Some(key_len) = read_optional_u32(&mut self.reader)? else {
            return Ok(None);
        };
        let value_len = read_required_u32(&mut self.reader)?;
        let mut key = vec![0; key_len as usize];
        let mut value = vec![0; value_len as usize];
        self.reader.read_exact(&mut key)?;
        self.reader.read_exact(&mut value)?;
        Ok(Some((key, value)))
    }
}

fn merge_external_runs(
    paths: &[PathBuf],
    mut visit: impl FnMut(&[u8], &[u8]) -> Result<()>,
) -> Result<()> {
    let mut readers: Vec<ExternalRunReader> = paths
        .iter()
        .map(|path| ExternalRunReader::open(path))
        .collect::<Result<_>>()?;
    let mut heads: Vec<Option<(Vec<u8>, Vec<u8>)>> = readers
        .iter_mut()
        .map(ExternalRunReader::next)
        .collect::<Result<_>>()?;
    let mut previous: Option<(Vec<u8>, Vec<u8>)> = None;
    loop {
        let next = heads
            .iter()
            .enumerate()
            .filter_map(|(index, entry)| entry.as_ref().map(|entry| (index, entry)))
            .min_by(|(_, left), (_, right)| left.cmp(right))
            .map(|(index, _)| index);
        let Some(index) = next else { break };
        let entry = heads[index].take().expect("selected run has a head");
        if previous.as_ref() != Some(&entry) {
            visit(&entry.0, &entry.1)?;
            previous = Some(entry);
        }
        heads[index] = readers[index].next()?;
    }
    Ok(())
}

fn read_optional_u32(reader: &mut impl Read) -> Result<Option<u32>> {
    let mut bytes = [0u8; 4];
    let mut read = 0usize;
    while read < bytes.len() {
        let count = reader.read(&mut bytes[read..])?;
        if count == 0 {
            if read == 0 {
                return Ok(None);
            }
            return Err(Error::Corrupt("truncated paged sort run".into()));
        }
        read += count;
    }
    Ok(Some(u32::from_le_bytes(bytes)))
}

fn read_required_u32(reader: &mut impl Read) -> Result<u32> {
    read_optional_u32(reader)?.ok_or_else(|| Error::Corrupt("truncated paged sort run".into()))
}

impl PagedWriter {
    pub(crate) fn create(path: &Path, dump_version: u64, page_size: Option<usize>) -> Result<Self> {
        let page_size = page_size.unwrap_or(DEFAULT_PAGE_SIZE);
        if !(256..=1024 * 1024).contains(&page_size) {
            return Err(Error::InvalidArgument(
                "paged index page size must be between 256 bytes and 1 MiB".into(),
            ));
        }
        let file = OpenOptions::new()
            .create(true)
            .truncate(true)
            .read(true)
            .write(true)
            .open(path)?;
        let mut file = BufWriter::with_capacity(WRITER_BUFFER_SIZE, file);
        file.write_all(&[0; HEADER_LEN])?;
        Ok(Self {
            file,
            page_size,
            page: Vec::with_capacity(page_size),
            page_first: None,
            page_last: Vec::new(),
            previous_key: Vec::new(),
            previous_value: Vec::new(),
            has_previous: false,
            pages: Vec::new(),
            dump_version,
            entry_count: 0,
        })
    }

    pub(crate) fn add(&mut self, key: &[u8], value: &[u8]) -> Result<()> {
        if self.has_previous
            && (self.previous_key.as_slice(), self.previous_value.as_slice()).cmp(&(key, value))
                == Ordering::Greater
        {
            return Err(Error::InvalidArgument(
                "paged index entries must be sorted".into(),
            ));
        }
        let entry_len = 8usize
            .checked_add(key.len())
            .and_then(|len| len.checked_add(value.len()))
            .ok_or_else(|| Error::InvalidArgument("paged index entry too large".into()))?;
        if !self.page.is_empty() && self.page.len() + entry_len > self.page_size {
            self.flush_page()?;
        }
        self.page
            .extend_from_slice(&(key.len() as u32).to_le_bytes());
        self.page
            .extend_from_slice(&(value.len() as u32).to_le_bytes());
        self.page.extend_from_slice(key);
        self.page.extend_from_slice(value);
        self.page_first.get_or_insert_with(|| key.to_vec());
        self.page_last.clear();
        self.page_last.extend_from_slice(key);
        self.previous_key.clear();
        self.previous_key.extend_from_slice(key);
        self.previous_value.clear();
        self.previous_value.extend_from_slice(value);
        self.has_previous = true;
        self.entry_count += 1;
        Ok(())
    }

    /// Copy already checksummed V2 pages without decoding and rebuilding
    /// every key/value entry. The caller guarantees that whole input ranges
    /// are strictly ordered and non-overlapping.
    fn append_v2_pages(&mut self, index: &PagedIndex) -> Result<()> {
        debug_assert_eq!(index.format, FORMAT);
        debug_assert!(self.page.is_empty() && !self.has_previous);
        for directory_entry in &index.pages {
            let page = index.page(*directory_entry);
            let stored_len = read_u32(&index.mmap, page.offset)? as usize;
            let stored_crc = read_u32(&index.mmap, page.offset + 4)?;
            if stored_len != page.payload_len {
                return Err(Error::Corrupt("paged index: page length mismatch".into()));
            }
            let payload_end = page
                .payload_offset
                .checked_add(page.payload_len)
                .ok_or_else(|| Error::Corrupt("paged index: page overflow".into()))?;
            let payload = index
                .mmap
                .get(page.payload_offset..payload_end)
                .ok_or_else(|| Error::Corrupt("paged index: truncated page".into()))?;
            if crc32fast::hash(payload) != stored_crc {
                return Err(Error::Corrupt("paged index: page crc mismatch".into()));
            }
            let raw = index
                .mmap
                .get(page.offset..payload_end)
                .ok_or_else(|| Error::Corrupt("paged index: truncated page".into()))?;
            let output_offset = self.file.stream_position()?;
            self.file.write_all(raw)?;
            self.pages.push(output_offset);
        }
        self.entry_count = self.entry_count.saturating_add(index.entry_count);
        Ok(())
    }

    /// Set the generation after streaming input whose final canonical length
    /// participates in that generation has been determined.
    pub(crate) fn set_dump_version(&mut self, dump_version: u64) {
        self.dump_version = dump_version;
    }

    pub(crate) fn finish(mut self) -> Result<()> {
        self.flush_page()?;
        let directory_offset = self.file.stream_position()?;
        self.file.write_all(DIRECTORY_MAGIC_V2)?;
        for offset in &self.pages {
            self.file.write_all(&offset.to_le_bytes())?;
        }
        let mut header = Vec::with_capacity(HEADER_LEN);
        header.extend_from_slice(MAGIC);
        header.extend_from_slice(&FORMAT.to_le_bytes());
        header.extend_from_slice(&(self.page_size as u32).to_le_bytes());
        header.extend_from_slice(&self.dump_version.to_le_bytes());
        header.extend_from_slice(&self.entry_count.to_le_bytes());
        header.extend_from_slice(&directory_offset.to_le_bytes());
        header.extend_from_slice(&(self.pages.len() as u32).to_le_bytes());
        header.extend_from_slice(&crc32fast::hash(&header).to_le_bytes());
        debug_assert_eq!(header.len(), HEADER_LEN);
        self.file.seek(SeekFrom::Start(0))?;
        self.file.write_all(&header)?;
        self.file.flush()?;
        let file = self
            .file
            .into_inner()
            .map_err(|error| Error::Io(error.into_error()))?;
        file.sync_all()?;
        Ok(())
    }

    fn flush_page(&mut self) -> Result<()> {
        if self.page.is_empty() {
            return Ok(());
        }
        let offset = self.file.stream_position()? as usize;
        self.file
            .write_all(&(self.page.len() as u32).to_le_bytes())?;
        self.file
            .write_all(&crc32fast::hash(&self.page).to_le_bytes())?;
        let first_key = self.page_first.as_ref().expect("non-empty page");
        self.file
            .write_all(&(first_key.len() as u32).to_le_bytes())?;
        self.file
            .write_all(&(self.page_last.len() as u32).to_le_bytes())?;
        self.file.write_all(first_key)?;
        self.file.write_all(&self.page_last)?;
        self.file.write_all(&self.page)?;
        self.pages.push(offset as u64);
        self.page.clear();
        self.page_first = None;
        self.page_last.clear();
        Ok(())
    }
}

fn read_u32(buf: &[u8], offset: usize) -> Result<u32> {
    let bytes: [u8; 4] = buf
        .get(offset..offset.saturating_add(4))
        .ok_or_else(|| Error::Corrupt("paged index: unexpected end".into()))?
        .try_into()
        .expect("four bytes");
    Ok(u32::from_le_bytes(bytes))
}

fn read_u64(buf: &[u8], offset: usize) -> Result<u64> {
    let bytes: [u8; 8] = buf
        .get(offset..offset.saturating_add(8))
        .ok_or_else(|| Error::Corrupt("paged index: unexpected end".into()))?
        .try_into()
        .expect("eight bytes");
    Ok(u64::from_le_bytes(bytes))
}

fn read_u32_at(buf: &[u8], pos: &mut usize) -> Result<u32> {
    let value = read_u32(buf, *pos)?;
    *pos += 4;
    Ok(value)
}

fn read_u64_at(buf: &[u8], pos: &mut usize) -> Result<u64> {
    let value = read_u64(buf, *pos)?;
    *pos += 8;
    Ok(value)
}

fn take<'a>(buf: &'a [u8], pos: &mut usize, len: usize) -> Result<&'a [u8]> {
    let end = pos
        .checked_add(len)
        .filter(|end| *end <= buf.len())
        .ok_or_else(|| Error::Corrupt("paged index: unexpected end".into()))?;
    let value = &buf[*pos..end];
    *pos = end;
    Ok(value)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn collect_values(index: &PagedIndex, key: &[u8]) -> Vec<Vec<u8>> {
        let mut values = Vec::new();
        index
            .visit_key(key, |value| {
                values.push(value.to_vec());
                Ok(true)
            })
            .unwrap();
        values
    }

    #[test]
    fn duplicate_keys_can_span_pages_and_stay_queryable() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("index.page");
        let mut writer = PagedWriter::create(&path, 42, Some(256)).unwrap();
        for value in 0u32..100 {
            writer.add(b"hot", &value.to_be_bytes()).unwrap();
        }
        writer.add(b"later", b"value").unwrap();
        writer.finish().unwrap();

        let index = PagedIndex::open(&path).unwrap();
        assert_eq!(index.dump_version(), 42);
        let values = collect_values(&index, b"hot");
        assert_eq!(values.len(), 100);
        assert_eq!(values[0], 0u32.to_be_bytes());
        assert_eq!(values[99], 99u32.to_be_bytes());
        assert_eq!(collect_values(&index, b"later"), vec![b"value".to_vec()]);
        assert!(collect_values(&index, b"missing").is_empty());
    }

    #[test]
    fn opens_legacy_v1_directory() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("legacy.page");
        let mut payload = Vec::new();
        payload.extend_from_slice(&1u32.to_le_bytes());
        payload.extend_from_slice(&1u32.to_le_bytes());
        payload.extend_from_slice(b"a");
        payload.extend_from_slice(b"1");
        let page_offset = HEADER_LEN as u64;
        let directory_offset = HEADER_LEN as u64 + 8 + payload.len() as u64;
        let mut bytes = Vec::new();
        bytes.extend_from_slice(MAGIC);
        bytes.extend_from_slice(&FORMAT_V1.to_le_bytes());
        bytes.extend_from_slice(&(DEFAULT_PAGE_SIZE as u32).to_le_bytes());
        bytes.extend_from_slice(&7u64.to_le_bytes());
        bytes.extend_from_slice(&1u64.to_le_bytes());
        bytes.extend_from_slice(&directory_offset.to_le_bytes());
        bytes.extend_from_slice(&1u32.to_le_bytes());
        bytes.extend_from_slice(&crc32fast::hash(&bytes).to_le_bytes());
        bytes.extend_from_slice(&(payload.len() as u32).to_le_bytes());
        bytes.extend_from_slice(&crc32fast::hash(&payload).to_le_bytes());
        bytes.extend_from_slice(&payload);
        bytes.extend_from_slice(DIRECTORY_MAGIC_V1);
        bytes.extend_from_slice(&page_offset.to_le_bytes());
        bytes.extend_from_slice(&(payload.len() as u32).to_le_bytes());
        bytes.extend_from_slice(&1u32.to_le_bytes());
        bytes.extend_from_slice(&1u32.to_le_bytes());
        bytes.extend_from_slice(b"a");
        bytes.extend_from_slice(b"a");
        std::fs::write(&path, bytes).unwrap();

        let index = PagedIndex::open(&path).unwrap();
        assert_eq!(index.dump_version(), 7);
        assert_eq!(collect_values(&index, b"a"), vec![b"1".to_vec()]);
    }

    #[test]
    fn writer_rejects_unsorted_input() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("index.page");
        let mut writer = PagedWriter::create(&path, 0, None).unwrap();
        writer.add(b"b", b"1").unwrap();
        assert!(matches!(
            writer.add(b"a", b"2"),
            Err(Error::InvalidArgument(_))
        ));
    }

    #[test]
    fn disjoint_v2_merge_copies_pages_without_repacking_entries() {
        let dir = tempfile::tempdir().unwrap();
        let left_path = dir.path().join("left.page");
        let right_path = dir.path().join("right.page");
        let merged_path = dir.path().join("merged.page");

        let mut left_writer = PagedWriter::create(&left_path, 1, Some(256)).unwrap();
        let mut right_writer = PagedWriter::create(&right_path, 1, Some(256)).unwrap();
        for value in 0u32..100 {
            left_writer
                .add(format!("a-{value:04}").as_bytes(), &value.to_be_bytes())
                .unwrap();
            right_writer
                .add(format!("z-{value:04}").as_bytes(), &value.to_be_bytes())
                .unwrap();
        }
        left_writer.finish().unwrap();
        right_writer.finish().unwrap();

        let left = PagedIndex::open(&left_path).unwrap();
        let right = PagedIndex::open(&right_path).unwrap();
        let source_pages = left.pages.len() + right.pages.len();
        merge_paged_indexes(&merged_path, &[&right, &left], 9).unwrap();

        let merged = PagedIndex::open(&merged_path).unwrap();
        assert_eq!(merged.dump_version(), 9);
        assert_eq!(merged.entry_count, 200);
        assert_eq!(merged.pages.len(), source_pages);
        assert_eq!(
            collect_values(&merged, b"a-0042"),
            vec![42u32.to_be_bytes()]
        );
        assert_eq!(
            collect_values(&merged, b"z-0099"),
            vec![99u32.to_be_bytes()]
        );
    }

    #[test]
    fn external_writer_bounds_fan_in_and_cleans_runs() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("external.page");
        let runs = dir.path().join("runs");
        let mut writer = ExternalPagedWriter::new(&path, &runs, 7, 64).unwrap();
        for value in (0u32..200).rev() {
            writer.add(b"hot", &value.to_be_bytes()).unwrap();
            writer.add(b"hot", &value.to_be_bytes()).unwrap();
        }
        writer.finish().unwrap();

        let index = PagedIndex::open(&path).unwrap();
        let values = collect_values(&index, b"hot");
        assert_eq!(values.len(), 200);
        assert_eq!(values[0], 0u32.to_be_bytes());
        assert_eq!(values[199], 199u32.to_be_bytes());
        assert_eq!(std::fs::read_dir(runs).unwrap().count(), 0);
    }
}
