//! SQL sort implementation shared by execution and its tests.
use super::*;

pub(super) struct SpillSorter<'a> {
    db: &'a Db,
    budget: usize,
    keep: Option<usize>,
    specs: Vec<SortSpec>,
    buffer: Vec<SortedOutputRow>,
    buffer_bytes: usize,
    largest_row: usize,
    spill_dir: PathBuf,
    runs: SpillFiles,
}

impl<'a> SpillSorter<'a> {
    pub(super) fn new(db: &'a Db, specs: Vec<SortSpec>, keep: Option<usize>) -> Result<Self> {
        let memory = db.memory_options();
        let spill_dir = memory
            .spill_directory
            .unwrap_or_else(|| std::env::temp_dir().join("elitesql-query-spill"));
        Ok(Self {
            db,
            budget: memory.query_working_bytes,
            keep,
            specs,
            buffer: Vec::new(),
            buffer_bytes: 0,
            largest_row: 0,
            spill_dir,
            runs: SpillFiles(Vec::new()),
        })
    }

    pub(super) fn push(&mut self, row: SortedOutputRow) -> Result<()> {
        crate::query_control::check_current()?;
        if self.keep == Some(0) {
            return Ok(());
        }
        let bytes = sorted_row_bytes(&row);
        self.largest_row = self.largest_row.max(bytes);
        if self.keep.is_some_and(|keep| self.buffer.len() == keep) {
            if compare_sorted_rows(&row, &self.buffer[0], &self.specs) != Ordering::Less {
                return Ok(());
            }
            let projected = self.buffer_bytes - sorted_row_bytes(&self.buffer[0]) + bytes;
            if projected <= self.budget {
                self.buffer[0] = row;
                self.buffer_bytes = projected;
                self.sift_worst_down(0);
                self.db.record_query_buffer(self.buffer_bytes);
                return Ok(());
            }
        }
        // One oversized row is allowed through by itself: the operator never
        // creates a second full-size copy before flushing it.
        if !self.buffer.is_empty() && self.buffer_bytes.saturating_add(bytes) > self.budget {
            self.flush_run()?;
        }
        self.buffer_bytes = self.buffer_bytes.saturating_add(bytes);
        self.buffer.push(row);
        if self.keep.is_some() {
            let mut child = self.buffer.len() - 1;
            while child > 0 {
                let parent = (child - 1) / 2;
                if compare_sorted_rows(&self.buffer[child], &self.buffer[parent], &self.specs)
                    != Ordering::Greater
                {
                    break;
                }
                self.buffer.swap(child, parent);
                child = parent;
            }
        }
        self.db.record_query_buffer(self.buffer_bytes);
        if self.buffer_bytes >= self.budget {
            self.flush_run()?;
        }
        Ok(())
    }

    fn sift_worst_down(&mut self, mut parent: usize) {
        loop {
            let left = parent * 2 + 1;
            if left >= self.buffer.len() {
                break;
            }
            let right = left + 1;
            let child = if right < self.buffer.len()
                && compare_sorted_rows(&self.buffer[right], &self.buffer[left], &self.specs)
                    == Ordering::Greater
            {
                right
            } else {
                left
            };
            if compare_sorted_rows(&self.buffer[parent], &self.buffer[child], &self.specs)
                != Ordering::Less
            {
                break;
            }
            self.buffer.swap(parent, child);
            parent = child;
        }
    }

    fn sort_and_prune(&mut self) {
        self.buffer
            .sort_by(|a, b| compare_sorted_rows(a, b, &self.specs));
        if let Some(keep) = self.keep {
            self.buffer.truncate(keep);
        }
    }

    fn flush_run(&mut self) -> Result<()> {
        if self.buffer.is_empty() {
            return Ok(());
        }
        self.sort_and_prune();
        fs::create_dir_all(&self.spill_dir)?;
        let path = self.spill_dir.join(format!("query-{}.run", Ulid::new()));
        self.runs.0.push(path.clone());
        let file = File::create(&path)?;
        let mut writer = BufWriter::new(file);
        for row in &self.buffer {
            write_sorted_row(&mut writer, row)?;
        }
        writer.flush()?;
        let bytes = writer.get_ref().metadata()?.len();
        self.db.record_query_spill(bytes);
        self.buffer.clear();
        self.buffer_bytes = 0;
        Ok(())
    }

    pub(super) fn finish(mut self, offset: usize, limit: Option<usize>) -> Result<Vec<Vec<Value>>> {
        let mut out = Vec::with_capacity(limit.unwrap_or(0).min(4096));
        self.for_each_sorted(offset, limit, |row| {
            out.push(row.values);
            Ok(())
        })?;
        Ok(out)
    }

    pub(super) fn for_each_sorted(
        &mut self,
        offset: usize,
        limit: Option<usize>,
        mut visit: impl FnMut(SortedOutputRow) -> Result<()>,
    ) -> Result<()> {
        if self.runs.0.is_empty() {
            self.sort_and_prune();
            let take = limit.unwrap_or(usize::MAX);
            for row in self.buffer.drain(..).skip(offset).take(take) {
                visit(row)?;
            }
            return Ok(());
        }
        self.flush_run()?;
        let fan_in = (self.budget / self.largest_row.saturating_add(8192).max(1)).clamp(2, 32);
        let reader_bytes = (self.budget / (fan_in * 4)).clamp(1, 8192);
        let specs = Arc::new(self.specs.clone());
        while self.runs.0.len() > fan_in {
            let previous = SpillFiles(std::mem::take(&mut self.runs.0));
            for group in previous.0.chunks(fan_in) {
                let path = self
                    .spill_dir
                    .join(format!("query-merge-{}.run", Ulid::new()));
                self.runs.0.push(path.clone());
                let mut writer = BufWriter::with_capacity(reader_bytes, File::create(&path)?);
                merge_sorted_runs(
                    self.db,
                    group,
                    specs.clone(),
                    self.keep.unwrap_or(usize::MAX),
                    reader_bytes,
                    |row| write_sorted_row(&mut writer, &row),
                )?;
                writer.flush()?;
                self.db
                    .record_query_spill(writer.get_ref().metadata()?.len());
            }
        }
        let take = limit.unwrap_or(usize::MAX);
        let stop = offset.saturating_add(take);
        let mut seen = 0usize;
        merge_sorted_runs(self.db, &self.runs.0, specs, stop, reader_bytes, |row| {
            if seen >= offset {
                visit(row)?;
            }
            seen += 1;
            Ok(())
        })
    }
}

struct MergeHead {
    row: SortedOutputRow,
    run: usize,
    specs: Arc<Vec<SortSpec>>,
}

impl PartialEq for MergeHead {
    fn eq(&self, other: &Self) -> bool {
        self.cmp(other) == Ordering::Equal
    }
}
impl Eq for MergeHead {}
impl PartialOrd for MergeHead {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}
impl Ord for MergeHead {
    fn cmp(&self, other: &Self) -> Ordering {
        compare_sorted_rows(&other.row, &self.row, &self.specs)
            .then_with(|| other.run.cmp(&self.run))
    }
}

pub(super) fn merge_sorted_runs(
    db: &Db,
    paths: &[PathBuf],
    specs: Arc<Vec<SortSpec>>,
    stop: usize,
    buffer_bytes: usize,
    mut visit: impl FnMut(SortedOutputRow) -> Result<()>,
) -> Result<()> {
    let mut readers = paths
        .iter()
        .map(|path| SpillRunReader::open(path, buffer_bytes))
        .collect::<Result<Vec<_>>>()?;
    let mut heap = BinaryHeap::new();
    // Include reader/writer buffers and live merge heads in operator telemetry.
    // The transient encoded frame is estimated by the largest decoded head;
    // this remains an estimate, not an allocator or RSS measurement.
    let buffers = buffer_bytes.saturating_mul(paths.len().saturating_add(1));
    let mut heads_bytes = 0usize;
    let mut largest_head = 0usize;
    for (run, reader) in readers.iter_mut().enumerate() {
        if let Some(row) = reader.next_row()? {
            let bytes = sorted_row_bytes(&row);
            heads_bytes = heads_bytes.saturating_add(bytes);
            largest_head = largest_head.max(bytes);
            heap.push(MergeHead {
                row,
                run,
                specs: specs.clone(),
            });
        }
    }
    db.record_query_buffer(
        buffers
            .saturating_add(heads_bytes)
            .saturating_add(largest_head),
    );
    for _ in 0..stop {
        crate::query_control::check_current()?;
        let Some(head) = heap.pop() else {
            break;
        };
        heads_bytes = heads_bytes.saturating_sub(sorted_row_bytes(&head.row));
        visit(head.row)?;
        if let Some(row) = readers[head.run].next_row()? {
            let bytes = sorted_row_bytes(&row);
            heads_bytes = heads_bytes.saturating_add(bytes);
            largest_head = largest_head.max(bytes);
            db.record_query_buffer(
                buffers
                    .saturating_add(heads_bytes)
                    .saturating_add(largest_head),
            );
            heap.push(MergeHead {
                row,
                run: head.run,
                specs: specs.clone(),
            });
        }
    }
    Ok(())
}

struct SpillRunReader {
    reader: BufReader<File>,
}

impl SpillRunReader {
    fn open(path: &PathBuf, buffer_bytes: usize) -> Result<Self> {
        Ok(Self {
            reader: BufReader::with_capacity(buffer_bytes, File::open(path)?),
        })
    }

    fn next_row(&mut self) -> Result<Option<SortedOutputRow>> {
        let mut len_bytes = [0u8; 4];
        let mut read = 0usize;
        while read < len_bytes.len() {
            let n = self.reader.read(&mut len_bytes[read..])?;
            if n == 0 {
                if read == 0 {
                    return Ok(None);
                }
                return Err(Error::Corrupt("truncated query spill frame".into()));
            }
            read += n;
        }
        let len = u32::from_le_bytes(len_bytes) as usize;
        let mut body = vec![0u8; len];
        self.reader.read_exact(&mut body)?;
        decode_sorted_row(&body).map(Some)
    }
}

pub(super) fn write_sorted_row(writer: &mut impl Write, row: &SortedOutputRow) -> Result<()> {
    let mut body = Vec::new();
    body.extend_from_slice(&row.sequence.to_le_bytes());
    body.extend_from_slice(&(row.keys.len() as u32).to_le_bytes());
    for value in &row.keys {
        encode_value(&mut body, value);
    }
    body.extend_from_slice(&(row.values.len() as u32).to_le_bytes());
    for value in &row.values {
        encode_value(&mut body, value);
    }
    let len = u32::try_from(body.len())
        .map_err(|_| Error::Sql("one query row is too large to spill".into()))?;
    writer.write_all(&len.to_le_bytes())?;
    writer.write_all(&body)?;
    Ok(())
}

pub(super) fn decode_sorted_row(body: &[u8]) -> Result<SortedOutputRow> {
    let mut pos = 0usize;
    let sequence = read_spill_u64(body, &mut pos)?;
    let key_count = read_spill_u32(body, &mut pos)? as usize;
    let mut keys = Vec::with_capacity(key_count);
    for _ in 0..key_count {
        keys.push(decode_value(body, &mut pos, None)?);
    }
    let value_count = read_spill_u32(body, &mut pos)? as usize;
    let mut values = Vec::with_capacity(value_count);
    for _ in 0..value_count {
        values.push(decode_value(body, &mut pos, None)?);
    }
    if pos != body.len() {
        return Err(Error::Corrupt(
            "query spill frame has trailing bytes".into(),
        ));
    }
    Ok(SortedOutputRow {
        keys,
        values,
        sequence,
    })
}

pub(super) fn read_spill_u32(buf: &[u8], pos: &mut usize) -> Result<u32> {
    let bytes: [u8; 4] = buf
        .get(*pos..pos.saturating_add(4))
        .ok_or_else(|| Error::Corrupt("truncated query spill frame".into()))?
        .try_into()
        .expect("four bytes");
    *pos += 4;
    Ok(u32::from_le_bytes(bytes))
}

pub(super) fn read_spill_u64(buf: &[u8], pos: &mut usize) -> Result<u64> {
    let bytes: [u8; 8] = buf
        .get(*pos..pos.saturating_add(8))
        .ok_or_else(|| Error::Corrupt("truncated query spill frame".into()))?
        .try_into()
        .expect("eight bytes");
    *pos += 8;
    Ok(u64::from_le_bytes(bytes))
}
