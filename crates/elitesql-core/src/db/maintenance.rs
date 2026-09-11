//! Maintenance internals; shared state and lock ownership remain in db.rs.
use super::*;

/// Drain committed in-memory data into a new segment, publish a new manifest
/// referencing it, and rotate the WAL. Runs under the commit mutex.
pub(super) fn checkpoint_measured(shared: &Arc<Shared>, cs: &mut CommitState) -> Result<()> {
    let started = Instant::now();
    // Derived overlays have their own frozen background pipeline. Canonical
    // checkpoint latency must not include serializing secondary/text/vector
    // indexes under the commit mutex.
    let result = checkpoint_locked(shared, cs);
    if result.is_ok() {
        let nanos = started.elapsed().as_nanos().min(u64::MAX as u128) as u64;
        shared
            .checkpoint_count
            .fetch_add(1, AtomicOrdering::Relaxed);
        shared
            .checkpoint_nanos
            .fetch_add(nanos, AtomicOrdering::Relaxed);
    }
    result
}

pub(super) fn wait_for_background_checkpoint(shared: &Arc<Shared>) -> Result<()> {
    let mut status = shared.background_checkpoint.lock().unwrap();
    while status.running {
        status = shared
            .background_checkpoint_done
            .wait(status)
            .unwrap_or_else(|poison| poison.into_inner());
    }
    if let Some(message) = status.last_error.take() {
        if std::mem::take(&mut status.commit_unknown) {
            return Err(Error::CommitUnknown(message));
        }
        return Err(Error::Io(std::io::Error::other(format!(
            "background checkpoint failed: {message}"
        ))));
    }
    Ok(())
}

pub(super) fn take_background_checkpoint_error(shared: &Shared) -> Result<()> {
    let mut status = shared.background_checkpoint.lock().unwrap();
    if !status.running {
        if let Some(message) = status.last_error.take() {
            if std::mem::take(&mut status.commit_unknown) {
                return Err(Error::CommitUnknown(message));
            }
            return Err(Error::Io(std::io::Error::other(format!(
                "background checkpoint failed: {message}"
            ))));
        }
    }
    Ok(())
}

pub(super) fn lock_commit_for_maintenance<'a>(shared: &'a Arc<Shared>) -> Result<CommitGuard<'a>> {
    ensure_canonical_writable(shared)?;
    loop {
        wait_for_background_checkpoint(shared)?;
        wait_for_background_derived(shared)?;
        let guard = lock_commit_after_group_sync(shared);
        let derived_running = shared.background_derived.lock().unwrap().running;
        if shared.state.read().unwrap().index.frozen.is_none() && !derived_running {
            return Ok(guard);
        }
        drop(guard);
    }
}

pub(super) fn wait_vector_indexing_shared(shared: &Shared) -> Result<()> {
    while shared.vector_backlog.load(AtomicOrdering::SeqCst) > 0 {
        if !shared.vector_worker_alive.load(AtomicOrdering::Acquire) {
            let message = shared
                .vector_worker_error
                .lock()
                .unwrap_or_else(|poison| poison.into_inner())
                .clone()
                .unwrap_or_else(|| "vector indexing worker stopped with pending work".into());
            return Err(Error::Io(std::io::Error::other(message)));
        }
        std::thread::sleep(Duration::from_millis(1));
    }
    Ok(())
}

/// WAL rotation must never overtake a group whose callers still depend on the
/// current generation's sync result. Ordinary run-manifest publication may
/// use the raw mutex because it does not replace the WAL.
pub(super) fn lock_commit_after_group_sync(shared: &Arc<Shared>) -> CommitGuard<'_> {
    loop {
        let guard = shared.commit.lock();
        let Some(group) = guard.wal_sync_group.clone() else {
            return guard;
        };
        drop(guard);
        let _ = group.wait();
    }
}

pub(super) fn ensure_canonical_writable(shared: &Shared) -> Result<()> {
    if let Some(message) = shared.canonical_error.lock().unwrap().as_ref() {
        return Err(Error::CommitUnknown(format!(
            "database writes are fenced until reopen after an uncertain canonical publication: {message}"
        )));
    }
    Ok(())
}

pub(super) fn publication_sync_error(
    shared: &Shared,
    operation: &str,
    outcome: PublishOutcome,
) -> Option<Error> {
    let PublishOutcome::SyncFailed(error) = outcome else {
        return None;
    };
    let message =
        format!("{operation} was renamed into place, but syncing its directory failed: {error}");
    *shared.canonical_error.lock().unwrap() = Some(message.clone());
    Some(Error::CommitUnknown(message))
}

/// Publish a catalog-only generation while the caller holds commit + state.
/// `catalog.json` is a compatibility mirror; the manifest-embedded catalog is
/// the canonical schema paired with its exact segment/WAL generation.
pub(super) fn publish_catalog_generation_locked(
    shared: &Arc<Shared>,
    _cs: &mut CommitState,
    state: &mut State,
    next: Catalog,
) -> Result<Option<Error>> {
    next.validate()?;
    // Writing the mirror first is safe: if canonical publication does not
    // happen, current engines ignore the ahead mirror in favor of the embedded
    // catalog in the old manifest.
    next.save(&shared.dir.join(CATALOG_FILE))?;
    let (current, _) = Manifest::load(&shared.dir)?;
    let mut identities = current.identity_high_water;
    identities.retain(|table, _| {
        next.table(table)
            .is_some_and(|schema| schema.columns.iter().any(|column| column.identity))
    });
    let outcome = Manifest {
        format_version: FORMAT_VERSION,
        committed_version: current.committed_version,
        segments: current.segments,
        wal_id: current.wal_id,
        required_wal_id: current.required_wal_id,
        identity_high_water: identities,
        catalog: Some(next.clone()),
    }
    .publish(&shared.dir)?;
    state.catalog = next;
    Ok(publication_sync_error(shared, "catalog manifest", outcome))
}

pub(super) fn fence_writes(shared: &Shared, message: impl Into<String>) {
    *shared.canonical_error.lock().unwrap() = Some(message.into());
}

pub(super) fn background_checkpoint_supported(shared: &Shared) -> bool {
    // Canonical primary data can be frozen independently of disposable
    // secondary/text/vector overlays. Async vector jobs are the only exception:
    // their charged payloads have not reached the overlay yet, so freezing until
    // they drain keeps memory accounting and index publication ordered.
    shared.vector_backlog.load(AtomicOrdering::SeqCst) == 0
}

/// Freeze the active primary delta in O(1), transfer its memory charge to the
/// maintenance pool, and enqueue its durable flush. The caller holds the
/// commit mutex, so the WAL boundary and MVCC version describe exactly the
/// frozen generation.
pub(super) fn schedule_frozen_checkpoint(
    shared: &Arc<Shared>,
    cs: &mut CommitState,
    memory: MaintenanceLease,
) -> Result<bool> {
    if !background_checkpoint_supported(shared) {
        return Ok(false);
    }
    let mut status = shared.background_checkpoint.lock().unwrap();
    if status.running {
        return Ok(false);
    }
    debug_assert!(status.last_error.is_none(), "checked before WAL commit");
    status.commit_unknown = false;
    let (job, retained_index_bytes) = {
        let mut state = shared.state.write().unwrap();
        if state.index.frozen.is_some() || state.index.delta.is_empty() {
            return Ok(false);
        }
        let frozen = Arc::new(std::mem::take(&mut state.index.delta));
        let job = FrozenCheckpointJob {
            frozen: frozen.clone(),
            version: state.committed_version,
            segments: state.segments.clone(),
            next_segment_id: state.next_segment_id,
            catalog: state.catalog.clone(),
            identity_high_water: identity_manifest(&state),
            first_primary_run: state.index.runs.is_empty(),
            wal_id: cs.wal().id,
            wal_cutoff: cs.wal().len,
            memory: Some(memory),
        };
        state.index.frozen = Some(FrozenPrimary {
            version: job.version,
            delta: frozen,
        });
        let retained = state.index_delta_memory_bytes();
        (job, retained)
    };
    cs.memtable_bytes = 0;
    // Only the primary delta moved to the maintenance-owned frozen generation;
    // derived overlays remain live and must stay charged to their pool.
    shared
        .memory_governor
        .set_index_delta_bytes(retained_index_bytes);
    status.running = true;
    let sent = shared
        .checkpoint_tx
        .lock()
        .unwrap()
        .as_ref()
        .is_some_and(|sender| sender.send(job).is_ok());
    if !sent {
        status.running = false;
        drop(status);
        thaw_frozen_checkpoint_locked(shared);
        return Ok(false);
    }
    shared.memory_governor.record_index_consolidation();
    Ok(true)
}

pub(super) fn thaw_frozen_checkpoint(shared: &Arc<Shared>) {
    let _commit = shared.commit.lock();
    thaw_frozen_checkpoint_locked(shared);
}

pub(super) fn thaw_frozen_checkpoint_locked(shared: &Arc<Shared>) {
    let mut state = shared.state.write().unwrap();
    let Some(frozen) = state.index.frozen.take() else {
        return;
    };
    for (table, ids) in frozen.delta.iter() {
        let active = state.index.delta.entry(table.clone()).or_default();
        for (id, versions) in ids.iter() {
            active.merge_versions(id.clone(), versions);
        }
    }
    let retained = state.index_delta_memory_bytes();
    drop(state);
    shared.memory_governor.set_index_delta_bytes(retained);
}

pub(super) fn flush_frozen_checkpoint(
    shared: &Arc<Shared>,
    mut job: FrozenCheckpointJob,
) -> Result<()> {
    let started = Instant::now();
    let result = flush_frozen_checkpoint_inner(shared, &job);
    if result.is_err() {
        // Publication did not complete. Return the frozen generation to the
        // active pool; the original WAL still contains every commit.
        drop(job.memory.take());
        thaw_frozen_checkpoint(shared);
    } else {
        shared.checkpoint_nanos.fetch_add(
            started.elapsed().as_nanos().min(u64::MAX as u128) as u64,
            AtomicOrdering::Relaxed,
        );
    }
    result
}

pub(super) fn flush_frozen_checkpoint_inner(
    shared: &Arc<Shared>,
    job: &FrozenCheckpointJob,
) -> Result<()> {
    struct ReservedWalFiles {
        bridge: PathBuf,
        active: PathBuf,
        armed: bool,
    }

    impl Drop for ReservedWalFiles {
        fn drop(&mut self) {
            if self.armed {
                let _ = fs::remove_file(&self.active);
                let _ = fs::remove_file(&self.bridge);
            }
        }
    }

    struct MemEntry {
        table_index: usize,
        id_start: usize,
        id_len: usize,
        version: u64,
        payload: Option<MemPayload>,
    }

    let mem_count = job
        .frozen
        .values()
        .flat_map(|ids| ids.values())
        .map(|versions| versions.len())
        .sum();
    let id_bytes = job
        .frozen
        .values()
        .flat_map(|ids| ids.keys())
        .map(String::len)
        .sum();
    let mut mem = Vec::with_capacity(mem_count);
    let mut mem_tables = Vec::with_capacity(job.frozen.len());
    let mut mem_ids = Vec::with_capacity(id_bytes);
    let mut new_segment_superseded = false;
    let mut delta_tables: Vec<_> = job.frozen.iter().collect();
    delta_tables.sort_unstable_by_key(|(table, _)| primary_table_prefix(table));
    for (table, ids) in delta_tables {
        let table_index = mem_tables.len();
        mem_tables.push(table.clone());
        for (id, versions) in ids.iter() {
            if versions.len() > 1 {
                new_segment_superseded = true;
            }
            let id_start = mem_ids.len();
            mem_ids.extend_from_slice(id.as_bytes());
            for version in versions {
                let payload = match &version.kind {
                    VKind::MemPut(payload) => Some(payload.clone()),
                    VKind::MemTombstone => None,
                    _ => continue,
                };
                mem.push(MemEntry {
                    table_index,
                    id_start,
                    id_len: id.len(),
                    version: version.version,
                    payload,
                });
            }
        }
    }
    if mem.is_empty() {
        return Ok(());
    }

    let seg_id = job.next_segment_id;
    let seg_path = shared
        .dir
        .join(SEGMENTS_DIR)
        .join(segment_file_name(seg_id));
    let mut locs = vec![(0, 0); mem.len()];
    let mut segment_order: Vec<usize> = (0..mem.len()).collect();
    segment_order.sort_unstable_by_key(|index| mem[*index].version);
    let raw = File::create(&seg_path)?;
    let writer_bytes = shared
        .opts
        .memory
        .maintenance_pool_bytes
        .clamp(1, 1024 * 1024);
    let mut writer = BufWriter::with_capacity(writer_bytes, raw);
    let mut position = 0u64;
    let mut encoded = Vec::new();
    for index in segment_order {
        let entry = &mem[index];
        let table = &mem_tables[entry.table_index];
        let id = std::str::from_utf8(&mem_ids[entry.id_start..entry.id_start + entry.id_len])
            .expect("copied from a String");
        let payload_rel = encode_entry_into(
            &mut encoded,
            entry.version,
            table,
            id,
            entry.payload.as_ref().map(MemPayload::as_slice),
        )?;
        locs[index] = (
            position + payload_rel,
            entry
                .payload
                .as_ref()
                .map_or(0, |payload| payload.len() as u32),
        );
        writer.write_all(&encoded)?;
        position = position.saturating_add(encoded.len() as u64);
    }
    writer.flush()?;
    let segment_file = writer
        .into_inner()
        .map_err(|error| Error::Io(error.into_error()))?;
    segment_file.sync_all()?;
    fsync_dir(&shared.dir.join(SEGMENTS_DIR))?;

    let mut new_segments = job.segments.clone();
    new_segments.push(SegmentMeta {
        id: seg_id,
        len: position,
    });
    let generation = primary_generation(job.version, &new_segments, &job.catalog);
    let indexes_dir = shared.dir.join(INDEXES_DIR);
    // Protect the prepared run from another publisher's orphan cleanup. This
    // mutex never serializes ordinary commits, and primary compaction releases
    // its maintenance memory before acquiring it, preserving lock order.
    let _publication = shared.primary_manifest_publication.lock().unwrap();
    let file = if job.first_primary_run {
        "primary.pidx".to_owned()
    } else {
        format!("primary-L0-{}.pidx.run", Ulid::new())
    };
    let level = if job.first_primary_run {
        PRIMARY_BASE_LEVEL
    } else {
        0
    };
    let primary_path = indexes_dir.join(&file);
    let primary_tmp = primary_path.with_extension("run.tmp");
    let mut primary_writer = PagedWriter::create(&primary_tmp, generation, None)?;
    let mut key = Vec::new();
    let mut value = Vec::with_capacity(25);
    let write_result = (|| -> Result<()> {
        for (entry, (payload_offset, payload_len)) in mem.iter().zip(&locs) {
            let table = &mem_tables[entry.table_index];
            let epoch = job.catalog.table(table).map_or(0, |schema| schema.epoch);
            if entry.version <= epoch {
                continue;
            }
            let id = std::str::from_utf8(&mem_ids[entry.id_start..entry.id_start + entry.id_len])
                .expect("copied from a String");
            encode_primary_key_into(table, id, &mut key);
            let kind = if entry.payload.is_some() {
                VKind::SegPut {
                    segment: seg_id,
                    payload_offset: *payload_offset,
                    payload_len: *payload_len,
                }
            } else {
                VKind::SegTombstone
            };
            encode_primary_entry_into(
                &VersionEntry {
                    version: entry.version,
                    kind,
                },
                &mut value,
            )?;
            primary_writer.add(&key, &value)?;
        }
        primary_writer.finish()
    })();
    if let Err(error) = write_result {
        let _ = fs::remove_file(&primary_tmp);
        return Err(error);
    }
    fs::rename(&primary_tmp, &primary_path)?;
    fsync_dir(&indexes_dir)?;
    let primary_bytes = fs::metadata(&primary_path)?.len();
    let primary_meta = PrimaryRunMeta {
        file,
        level,
        bytes: primary_bytes,
        generation,
    };
    let primary_index = Arc::new(PagedIndex::open(&primary_path)?);
    let segment_reader = File::open(&seg_path)?;

    // Two durable empty successor WALs form a recovery bridge before the
    // active writer is switched: a crash sees either the old manifest + old
    // WAL + successors, or the new manifest + copied tail + active successor.
    let bridge_wal_id = job
        .wal_id
        .checked_add(1)
        .ok_or_else(|| Error::Corrupt("WAL id exhausted during checkpoint".into()))?;
    let active_wal_id = job
        .wal_id
        .checked_add(2)
        .ok_or_else(|| Error::Corrupt("WAL id exhausted during checkpoint".into()))?;
    let wal_dir = shared.dir.join(WAL_DIR);
    let bridge_path = wal_path(&shared.dir, bridge_wal_id);
    let active_path = wal_path(&shared.dir, active_wal_id);
    OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&bridge_path)?
        .sync_all()?;
    if let Err(error) = OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&active_path)
        .and_then(|file| file.sync_all())
    {
        let _ = fs::remove_file(&bridge_path);
        return Err(error.into());
    }
    let mut reserved_wals = ReservedWalFiles {
        bridge: bridge_path.clone(),
        active: active_path.clone(),
        armed: true,
    };
    fsync_dir(&wal_dir)?;
    let active_wal = WalWriter::open(&shared.dir, active_wal_id)?;

    // Reserve the recovery extent durably before any commit can enter the
    // successor. Both manifest copies must know it before swapping writers.
    let (mut old_wal, tail_len, mut new_primary_runs) = {
        let mut cs = lock_commit_after_group_sync(shared);
        if cs.wal().id != job.wal_id || cs.wal().len < job.wal_cutoff {
            return Err(Error::Corrupt(
                "background checkpoint observed an unexpected WAL generation".into(),
            ));
        }
        let state = shared.state.read().unwrap();
        let still_frozen = state.index.frozen.as_ref().is_some_and(|frozen| {
            frozen.version == job.version && Arc::ptr_eq(&frozen.delta, &job.frozen)
        });
        if !still_frozen {
            return Err(Error::Corrupt(
                "background checkpoint lost its frozen generation".into(),
            ));
        }
        let runs = state.index.run_metas();
        drop(state);
        let (mut recovery_manifest, _) = Manifest::load(&shared.dir)?;
        recovery_manifest.required_wal_id = active_wal_id;
        let outcome = recovery_manifest.publish(&shared.dir)?;
        reserved_wals.armed = false;
        if let Some(error) = publication_sync_error(shared, "WAL chain reservation", outcome) {
            return Err(error);
        }
        let outcome = recovery_manifest.refresh_previous(&shared.dir)?;
        if let Some(error) =
            publication_sync_error(shared, "WAL chain fallback reservation", outcome)
        {
            return Err(error);
        }
        let tail_len = cs.wal().len - job.wal_cutoff;
        let old_wal = cs
            .wal
            .replace(active_wal)
            .expect("writable checkpoint has an active WAL");
        (old_wal, tail_len, runs)
    };
    // From this point forward the active writer may contain acknowledged
    // commits. Neither successor may be removed on an error; recovery follows
    // the consecutive WAL chain from the still-current manifest.
    reserved_wals.armed = false;

    if let WalAppendOutcome::SyncFailed(error) = old_wal.sync_data() {
        let _commit = shared.commit.lock();
        thaw_frozen_checkpoint_locked(shared);
        return Err(error.into());
    }

    // Build the bridge through a temporary inode. Recovery can safely cross
    // the durable empty placeholder while this copy is incomplete; the full
    // duplicate tail appears atomically only after it has been synced.
    let bridge_tmp = wal_dir.join(format!("{bridge_wal_id:06}.wal.bridge.tmp"));
    let mut source = File::open(wal_path(&shared.dir, job.wal_id))?;
    source.seek(SeekFrom::Start(job.wal_cutoff))?;
    let mut raw_tail = source.take(tail_len);
    let target = File::create(&bridge_tmp)?;
    let mut target = BufWriter::with_capacity(writer_bytes, target);
    let copied = std::io::copy(&mut raw_tail, &mut target)?;
    if copied != tail_len {
        return Err(Error::Corrupt(
            "background checkpoint could not copy the complete WAL tail".into(),
        ));
    }
    target.flush()?;
    let target = target
        .into_inner()
        .map_err(|error| Error::Io(error.into_error()))?;
    target.sync_all()?;
    fs::rename(&bridge_tmp, &bridge_path)?;
    fsync_dir(&wal_dir)?;

    #[cfg(test)]
    {
        shared
            .primary_test_reached_manifest
            .store(true, AtomicOrdering::Release);
        while shared
            .primary_test_pause_before_manifest
            .load(AtomicOrdering::Acquire)
        {
            std::thread::yield_now();
        }
    }

    new_primary_runs.push(primary_meta.clone());
    let primary_publish_error = PrimaryRunManifest::new(generation, new_primary_runs)
        .publish(&indexes_dir)
        .err();
    let published_manifest = Manifest {
        format_version: FORMAT_VERSION,
        committed_version: job.version,
        segments: new_segments.clone(),
        wal_id: bridge_wal_id,
        required_wal_id: active_wal_id,
        identity_high_water: job.identity_high_water.clone(),
        catalog: Some(job.catalog.clone()),
    };
    #[cfg(test)]
    if shared
        .primary_test_fail_before_manifest_rename
        .swap(false, AtomicOrdering::AcqRel)
    {
        return Err(Error::Io(std::io::Error::other(
            "injected primary manifest rename failure",
        )));
    }
    let manifest_outcome = match published_manifest.publish(&shared.dir) {
        Ok(outcome) => outcome,
        Err(error) => {
            let _commit = shared.commit.lock();
            thaw_frozen_checkpoint_locked(shared);
            return Err(error);
        }
    };
    #[cfg(test)]
    let manifest_outcome = if shared
        .primary_test_fail_after_manifest_rename
        .swap(false, AtomicOrdering::AcqRel)
    {
        PublishOutcome::SyncFailed(std::io::Error::other(
            "injected primary manifest directory sync failure",
        ))
    } else {
        manifest_outcome
    };

    let retained = {
        let _commit = shared.commit.lock();
        let mut state = shared.state.write().unwrap();
        let still_frozen = state.index.frozen.as_ref().is_some_and(|frozen| {
            frozen.version == job.version && Arc::ptr_eq(&frozen.delta, &job.frozen)
        });
        if !still_frozen {
            return Err(Error::Corrupt(
                "background checkpoint lost its frozen generation before adoption".into(),
            ));
        }
        if !new_segment_superseded {
            new_segment_superseded = state.index.delta.iter().any(|(table, ids)| {
                job.frozen
                    .get(table)
                    .is_some_and(|frozen_ids| ids.keys().any(|id| frozen_ids.contains_key(id)))
            });
        }
        state
            .readers
            .insert(seg_id, Arc::new(SegmentReader::new(segment_reader)));
        state.segments = new_segments;
        state.next_segment_id = seg_id + 1;
        if new_segment_superseded {
            state.superseded_segments.insert(seg_id);
        }
        state.index.frozen = None;
        state.index.generation = generation;
        state.index.runs.push(PrimaryRun {
            meta: primary_meta,
            index: primary_index,
        });
        state.index_delta_memory_bytes()
    };

    let publication_error =
        publication_sync_error(shared, "background checkpoint manifest", manifest_outcome);
    if publication_error.is_none() {
        cleanup_orphans(
            &shared.dir,
            &Manifest {
                format_version: FORMAT_VERSION,
                committed_version: job.version,
                segments: shared.state.read().unwrap().segments.clone(),
                wal_id: bridge_wal_id,
                required_wal_id: active_wal_id,
                identity_high_water: job.identity_high_water.clone(),
                catalog: Some(job.catalog.clone()),
            },
        )?;
    }
    shared
        .primary_checkpoint_bytes_written
        .fetch_add(primary_bytes, AtomicOrdering::Relaxed);
    shared.memory_governor.set_index_delta_bytes(retained);
    shared
        .checkpoint_count
        .fetch_add(1, AtomicOrdering::Relaxed);
    if let Some(error) = primary_publish_error {
        record_maintenance_error(
            shared,
            format!("primary run manifest will be rebuilt on reopen: {error}"),
        );
        shared
            .index_maintenance_failures
            .fetch_add(1, AtomicOrdering::Relaxed);
    }
    cleanup_primary_run_orphans(&shared.dir);
    maybe_schedule_primary_compaction(shared);
    refresh_compaction_debt_if_needed(shared);
    maybe_schedule_auto_compaction(shared);
    publication_error.map_or(Ok(()), Err)
}

pub(super) fn checkpoint_locked(shared: &Arc<Shared>, cs: &mut CommitState) -> Result<()> {
    if shared.opts.read_only {
        return Err(Error::ReadOnly);
    }
    struct MemEntry {
        table_index: usize,
        id_start: usize,
        id_len: usize,
        version: u64,
        payload: Option<MemPayload>,
    }
    let (
        mem,
        mem_tables,
        mem_ids,
        segments,
        committed_version,
        next_segment_id,
        new_segment_superseded,
        catalog,
        identity_high_water,
        old_primary_runs,
        first_primary_run,
    ) = {
        let st = shared.state.read().unwrap();
        let mem_count = st
            .index
            .delta
            .values()
            .flat_map(|ids| ids.values())
            .flat_map(|versions| versions.iter())
            .filter(|version| matches!(&version.kind, VKind::MemPut(_) | VKind::MemTombstone))
            .count();
        let id_bytes = st
            .index
            .delta
            .values()
            .flat_map(|ids| ids.iter())
            .filter(|(_, versions)| {
                versions
                    .iter()
                    .any(|version| matches!(&version.kind, VKind::MemPut(_) | VKind::MemTombstone))
            })
            .map(|(id, _)| id.len())
            .sum();
        let mut mem: Vec<MemEntry> = Vec::with_capacity(mem_count);
        let mut mem_tables = Vec::with_capacity(st.index.delta.len());
        let mut mem_ids = Vec::with_capacity(id_bytes);
        let mut new_segment_superseded = false;
        let mut delta_tables: Vec<_> = st.index.delta.iter().collect();
        delta_tables.sort_unstable_by_key(|(table, _)| primary_table_prefix(table));
        for (table, ids) in delta_tables {
            let table_index = mem_tables.len();
            mem_tables.push(table.clone());
            for (id, versions) in ids.iter() {
                let resident_versions = versions.iter().filter(|version| {
                    matches!(&version.kind, VKind::MemPut(_) | VKind::MemTombstone)
                });
                let resident_count = resident_versions.clone().count();
                if resident_count > 1 {
                    new_segment_superseded = true;
                }
                if resident_count == 0 {
                    continue;
                }
                let id_start = mem_ids.len();
                mem_ids.extend_from_slice(id.as_bytes());
                for v in resident_versions {
                    match &v.kind {
                        VKind::MemPut(p) => mem.push(MemEntry {
                            table_index,
                            id_start,
                            id_len: id.len(),
                            version: v.version,
                            payload: Some(p.clone()),
                        }),
                        VKind::MemTombstone => mem.push(MemEntry {
                            table_index,
                            id_start,
                            id_len: id.len(),
                            version: v.version,
                            payload: None,
                        }),
                        _ => {}
                    }
                }
            }
        }
        (
            mem,
            mem_tables,
            mem_ids,
            st.segments.clone(),
            st.committed_version,
            st.next_segment_id,
            new_segment_superseded,
            st.catalog.clone(),
            identity_manifest(&st),
            st.index.run_metas(),
            st.index.runs.is_empty(),
        )
    };
    if mem.is_empty() && cs.wal().len == 0 {
        return Ok(());
    }
    let mut new_segments = segments;
    let mut written: Option<(u32, Vec<(u64, u32)>)> = None;
    if !mem.is_empty() {
        let seg_id = next_segment_id;
        let mut locs = vec![(0, 0); mem.len()];
        let mut segment_order: Vec<usize> = (0..mem.len()).collect();
        segment_order.sort_unstable_by_key(|index| mem[*index].version);
        let seg_path = shared
            .dir
            .join(SEGMENTS_DIR)
            .join(segment_file_name(seg_id));
        let raw = File::create(&seg_path)?;
        let writer_bytes = shared
            .opts
            .memory
            .maintenance_pool_bytes
            .clamp(1, 1024 * 1024);
        let mut writer = BufWriter::with_capacity(writer_bytes, raw);
        let mut position = 0u64;
        let mut entry = Vec::new();
        for index in segment_order {
            let m = &mem[index];
            let table = &mem_tables[m.table_index];
            let id = std::str::from_utf8(&mem_ids[m.id_start..m.id_start + m.id_len])
                .expect("copied from a String");
            let payload_rel = encode_entry_into(
                &mut entry,
                m.version,
                table,
                id,
                m.payload.as_ref().map(|p| p.as_slice()),
            )?;
            locs[index] = (
                position + payload_rel,
                m.payload.as_ref().map_or(0, |p| p.len() as u32),
            );
            writer.write_all(&entry)?;
            position = position.saturating_add(entry.len() as u64);
        }
        writer.flush()?;
        let f = writer
            .into_inner()
            .map_err(|error| Error::Io(error.into_error()))?;
        f.sync_all()?;
        fsync_dir(&shared.dir.join(SEGMENTS_DIR))?;
        new_segments.push(SegmentMeta {
            id: seg_id,
            len: position,
        });
        written = Some((seg_id, locs));
    }

    // The compact snapshot is already in primary-key order. Build the
    // disposable primary run directly from it and the segment offsets instead
    // of writing those offsets back into every B-tree entry only to scan and
    // clear the same delta immediately afterward.
    let generation = primary_generation(committed_version, &new_segments, &catalog);
    let prepared_primary = if let Some((seg_id, locs)) = &written {
        let indexes_dir = shared.dir.join(INDEXES_DIR);
        let file = if first_primary_run {
            "primary.pidx".to_owned()
        } else {
            format!("primary-L0-{}.pidx.run", Ulid::new())
        };
        let level = if first_primary_run {
            PRIMARY_BASE_LEVEL
        } else {
            0
        };
        let path = indexes_dir.join(&file);
        let tmp = path.with_extension("run.tmp");
        let mut writer = PagedWriter::create(&tmp, generation, None)?;
        let mut key = Vec::new();
        let mut value = Vec::with_capacity(25);
        let mut entries = 0usize;
        let write_result = (|| -> Result<()> {
            for (m, (payload_offset, payload_len)) in mem.iter().zip(locs) {
                let table = &mem_tables[m.table_index];
                let epoch = catalog.table(table).map_or(0, |schema| schema.epoch);
                if m.version <= epoch {
                    continue;
                }
                let id = std::str::from_utf8(&mem_ids[m.id_start..m.id_start + m.id_len])
                    .expect("copied from a String");
                encode_primary_key_into(table, id, &mut key);
                let kind = match m.payload {
                    Some(_) => VKind::SegPut {
                        segment: *seg_id,
                        payload_offset: *payload_offset,
                        payload_len: *payload_len,
                    },
                    None => VKind::SegTombstone,
                };
                encode_primary_entry_into(
                    &VersionEntry {
                        version: m.version,
                        kind,
                    },
                    &mut value,
                )?;
                writer.add(&key, &value)?;
                entries += 1;
            }
            writer.finish()
        })();
        if let Err(error) = write_result {
            let _ = fs::remove_file(&tmp);
            return Err(error);
        }
        if entries == 0 {
            let _ = fs::remove_file(&tmp);
            None
        } else {
            fs::rename(&tmp, &path)?;
            fsync_dir(&indexes_dir)?;
            let bytes = fs::metadata(&path)?.len();
            let meta = PrimaryRunMeta {
                file,
                level,
                bytes,
                generation,
            };
            let index = Arc::new(PagedIndex::open(&path)?);
            Some((meta, index))
        }
    } else {
        None
    };
    let mut prepared_segment_reader = match &written {
        Some((seg_id, _)) => Some(File::open(
            shared
                .dir
                .join(SEGMENTS_DIR)
                .join(segment_file_name(*seg_id)),
        )?),
        None => None,
    };

    // Create the new WAL before the manifest that references it, so the
    // manifest never points at a missing file.
    let new_wal_id = cs.wal().id + 1;
    File::create(wal_path(&shared.dir, new_wal_id))?.sync_all()?;
    fsync_dir(&shared.dir.join(WAL_DIR))?;
    let next_wal = WalWriter::open(&shared.dir, new_wal_id)?;
    let manifest_outcome = Manifest {
        format_version: FORMAT_VERSION,
        committed_version,
        segments: new_segments.clone(),
        wal_id: new_wal_id,
        required_wal_id: new_wal_id,
        identity_high_water,
        catalog: Some(catalog.clone()),
    }
    .publish(&shared.dir)?;

    // Canonical publication has succeeded. Switch to the new WAL before any
    // disposable index work so a failed run-manifest publication cannot leave
    // future commits appending to an unlinked old WAL inode.
    cs.wal = Some(next_wal);

    {
        let mut st = shared.state.write().unwrap();
        if let Some((seg_id, _)) = &written {
            st.readers.insert(
                *seg_id,
                Arc::new(SegmentReader::new(
                    prepared_segment_reader
                        .take()
                        .expect("reader prepared before canonical publication"),
                )),
            );
            st.next_segment_id = seg_id + 1;
            if new_segment_superseded {
                st.superseded_segments.insert(*seg_id);
            }
        }
        st.segments = new_segments;
    }

    let mut new_primary_runs = old_primary_runs;
    if let Some((meta, _)) = &prepared_primary {
        new_primary_runs.push(meta.clone());
    }
    let primary_publish_error = PrimaryRunManifest::new(generation, new_primary_runs)
        .publish(&shared.dir.join(INDEXES_DIR))
        .err();

    {
        let mut st = shared.state.write().unwrap();
        st.index.delta.clear();
        st.index.generation = generation;
        if let Some((meta, index)) = prepared_primary {
            shared
                .primary_checkpoint_bytes_written
                .fetch_add(meta.bytes, AtomicOrdering::Relaxed);
            st.index.runs.push(PrimaryRun { meta, index });
        }
    }
    cs.memtable_bytes = 0;
    let publication_error = publication_sync_error(shared, "checkpoint manifest", manifest_outcome);
    if let Some(error) = primary_publish_error {
        if publication_error.is_none() {
            fence_writes(
                shared,
                format!("checkpoint published but primary index publication failed: {error}"),
            );
        }
        return Err(publication_error.unwrap_or(error));
    }
    cleanup_primary_run_orphans(&shared.dir);
    maybe_schedule_primary_compaction(shared);
    let retained = shared.state.read().unwrap().index_delta_memory_bytes();
    shared.memory_governor.set_index_delta_bytes(retained);
    if publication_error.is_none() {
        cleanup_orphans(
            &shared.dir,
            &Manifest {
                format_version: FORMAT_VERSION,
                committed_version,
                segments: shared.state.read().unwrap().segments.clone(),
                wal_id: new_wal_id,
                required_wal_id: new_wal_id,
                identity_high_water: identity_manifest(&shared.state.read().unwrap()),
                catalog: Some(shared.state.read().unwrap().catalog.clone()),
            },
        )?;
    }
    publication_error.map_or(Ok(()), Err)
}

/// Move every mutable derived overlay into an immutable in-memory generation
/// in O(number of indexes), then let the maintenance worker perform graph/run
/// serialization without the commit mutex or state write lock.
pub(super) fn schedule_frozen_derived(
    shared: &Arc<Shared>,
    memory: MaintenanceLease,
) -> Result<bool> {
    let vector_ready = shared.vector_backlog.load(AtomicOrdering::SeqCst) == 0;
    let mut status = shared.background_derived.lock().unwrap();
    if status.running {
        return Ok(false);
    }
    status.last_error = None;

    let (secondary, text, vector, retained) = {
        let mut state = shared.state.write().unwrap();
        let version = state.committed_version;
        let mut secondary = Vec::new();
        for (key, index) in &mut state.secondary {
            let frozen = index.frozen.clone().or_else(|| index.freeze_delta(version));
            if let Some(frozen) = frozen {
                secondary.push(FrozenSecondaryJob {
                    key: key.clone(),
                    frozen,
                });
            }
        }
        let mut text = Vec::new();
        for (key, index) in &mut state.text {
            let frozen = index
                .frozen_delta()
                .or_else(|| index.freeze_delta_background(version));
            if let Some(frozen) = frozen {
                text.push(FrozenTextJob {
                    key: key.clone(),
                    frozen,
                });
            }
        }
        let definitions: HashMap<(String, String), VectorIndexDef> = state
            .catalog
            .tables
            .iter()
            .flat_map(|table| {
                table
                    .vector_indexes
                    .iter()
                    .cloned()
                    .map(|def| ((table.name.clone(), def.column.clone()), def))
            })
            .collect();
        let mut vector = Vec::new();
        for (key, index) in &mut state.vector {
            let frozen = index.frozen_delta().or_else(|| {
                vector_ready
                    .then(|| index.freeze_delta_background(version))
                    .flatten()
            });
            if let (Some(frozen), Some(generation), Some(def)) =
                (frozen, index.frozen_generation(), definitions.get(key))
            {
                vector.push(FrozenVectorJob {
                    table: key.0.clone(),
                    def: def.clone(),
                    frozen,
                    generation,
                });
            }
        }
        let retained = state.index_delta_memory_bytes();
        (secondary, text, vector, retained)
    };

    if secondary.is_empty() && text.is_empty() && vector.is_empty() {
        return Ok(false);
    }
    shared.memory_governor.set_index_delta_bytes(retained);
    let job = DerivedCheckpointJob {
        secondary,
        text,
        vector,
        memory: Some(memory),
    };
    status.running = true;
    let sent = shared
        .derived_tx
        .lock()
        .unwrap()
        .as_ref()
        .is_some_and(|sender| sender.send(job).is_ok());
    if !sent {
        status.running = false;
        return Err(Error::Io(std::io::Error::new(
            std::io::ErrorKind::BrokenPipe,
            "derived-index maintenance worker is unavailable",
        )));
    }
    shared.memory_governor.record_index_consolidation();
    Ok(true)
}

pub(super) fn wait_for_background_derived(shared: &Arc<Shared>) -> Result<()> {
    let mut status = shared.background_derived.lock().unwrap();
    while status.running {
        status = shared.background_derived_done.wait(status).unwrap();
    }
    if let Some(message) = status.last_error.take() {
        return Err(Error::Io(std::io::Error::other(format!(
            "background derived-index publication failed: {message}"
        ))));
    }
    Ok(())
}

pub(super) fn flush_frozen_derived(shared: &Arc<Shared>, job: DerivedCheckpointJob) -> Result<()> {
    let mut stage = "initializing derived-index publication".to_owned();
    flush_frozen_derived_inner(shared, job, &mut stage)
        .map_err(|error| Error::Io(std::io::Error::other(format!("{stage}: {error}"))))
}

#[cfg(test)]
pub(super) fn pause_derived_before_publish_for_test(shared: &Shared) {
    shared
        .derived_test_reached_publish
        .store(true, AtomicOrdering::Release);
    while shared
        .derived_test_pause_before_publish
        .load(AtomicOrdering::Acquire)
    {
        std::thread::sleep(Duration::from_millis(1));
    }
}

#[cfg(not(test))]
pub(super) fn pause_derived_before_publish_for_test(_shared: &Shared) {}

pub(super) fn publish_background_derived_manifest(
    _shared: &Shared,
    manifest: PreparedDerivedRunManifest,
) -> Result<PublishOutcome> {
    #[cfg(test)]
    if _shared
        .derived_test_fail_before_manifest_rename
        .swap(false, AtomicOrdering::AcqRel)
    {
        return Err(Error::Io(std::io::Error::other(
            "injected derived manifest rename failure",
        )));
    }
    let outcome = manifest.publish()?;
    #[cfg(test)]
    let outcome = if _shared
        .derived_test_fail_after_manifest_rename
        .swap(false, AtomicOrdering::AcqRel)
    {
        PublishOutcome::SyncFailed(std::io::Error::other(
            "injected derived manifest directory sync failure",
        ))
    } else {
        outcome
    };
    Ok(outcome)
}

pub(super) fn flush_frozen_derived_inner(
    shared: &Arc<Shared>,
    mut job: DerivedCheckpointJob,
    stage: &mut String,
) -> Result<()> {
    let _publication = shared.derived_manifest_publication.lock().unwrap();
    let indexes_dir = shared.dir.join(INDEXES_DIR);
    let vectors_dir = shared.dir.join(VECTORS_DIR);
    fs::create_dir_all(&indexes_dir)?;
    fs::create_dir_all(&vectors_dir)?;
    let temp_dir = shared
        .opts
        .memory
        .spill_directory
        .clone()
        .unwrap_or_else(|| indexes_dir.join("tmp"));
    fs::create_dir_all(&temp_dir)?;
    let budget = shared.opts.memory.maintenance_pool_bytes;

    for frozen_job in &job.secondary {
        *stage = format!(
            "writing secondary index {}.{}",
            frozen_job.key.0, frozen_job.key.1
        );
        let file = sidx_run_filename(&shared.dir, &frozen_job.key.0, &frozen_job.key.1, 0);
        let path = indexes_dir.join(&file);
        let prepared = path.with_file_name(format!("{file}.derived-pending"));
        let tmp = path.with_file_name(format!("{file}.derived-pending.tmp"));
        let generation = frozen_job.frozen.generation;
        let mut writer = ExternalPagedWriter::new(&tmp, &temp_dir, generation, budget)?;
        writer.add(SECONDARY_FORMAT_KEY, SECONDARY_FORMAT_VALUE)?;
        for (value, ids) in &frozen_job.frozen.delta {
            for id in ids {
                writer.add(
                    &secondary_pair_key(value, id),
                    &secondary_operation(generation, SECONDARY_ADD),
                )?;
            }
        }
        for (value, ids) in &frozen_job.frozen.removed {
            for id in ids {
                writer.add(
                    &secondary_pair_key(value, id),
                    &secondary_operation(generation, SECONDARY_DELETE),
                )?;
            }
        }
        if let Err(error) = writer.finish() {
            let _ = fs::remove_file(&tmp);
            return Err(error);
        }
        fs::rename(&tmp, &prepared).map_err(|error| {
            Error::Io(std::io::Error::new(
                error.kind(),
                format!(
                    "publishing frozen secondary run {} -> {} failed: {error}",
                    tmp.display(),
                    prepared.display()
                ),
            ))
        })?;
        fsync_dir(&indexes_dir)?;
        let mapped = Arc::new(PagedIndex::open(&prepared)?);
        validate_secondary_run(&mapped)?;
        *stage = format!(
            "publishing secondary index {}.{}",
            frozen_job.key.0, frozen_job.key.1
        );
        let bytes = fs::metadata(&prepared)?.len();
        let (meta, manifest) = {
            let _commit = shared.commit.lock();
            let state = shared.state.read().unwrap();
            let Some(index) = state.secondary.get(&frozen_job.key) else {
                drop(state);
                let _ = fs::remove_file(&prepared);
                continue;
            };
            if !index.frozen_matches(&frozen_job.frozen) {
                drop(state);
                let _ = fs::remove_file(&prepared);
                continue;
            }
            let meta = DerivedRunMeta {
                file,
                level: if index.runs.is_empty() {
                    DERIVED_BASE_LEVEL
                } else {
                    0
                },
                bytes,
                generation,
            };
            let mut metas = index.run_metas();
            metas.push(meta.clone());
            let manifest = DerivedRunManifest::new(
                DerivedRunKind::Secondary,
                &frozen_job.key.0,
                &frozen_job.key.1,
                generation,
                metas,
                [0, 0],
            );
            (meta, manifest)
        };
        *stage = format!(
            "preparing secondary run manifest {}.{}",
            frozen_job.key.0, frozen_job.key.1
        );
        let manifest = manifest.prepare(&sidx_manifest_path(
            &shared.dir,
            &frozen_job.key.0,
            &frozen_job.key.1,
        ))?;
        pause_derived_before_publish_for_test(shared);
        fs::rename(&prepared, &path)?;
        fsync_dir(&indexes_dir)?;
        let manifest_outcome = match publish_background_derived_manifest(shared, manifest) {
            Ok(outcome) => outcome,
            Err(error) => {
                let _ = fs::remove_file(&path);
                return Err(error);
            }
        };
        {
            let _commit = shared.commit.lock();
            let mut state = shared.state.write().unwrap();
            let index = state.secondary.get_mut(&frozen_job.key).ok_or_else(|| {
                Error::Corrupt("secondary index disappeared during background publication".into())
            })?;
            if !index.frozen_matches(&frozen_job.frozen) {
                return Err(Error::Corrupt(
                    "secondary frozen generation changed during background publication".into(),
                ));
            }
            index.runs.push(SecRun {
                meta: meta.clone(),
                index: mapped,
            });
            index.generation = generation;
            index.clear_frozen(&frozen_job.frozen);
        }
        shared
            .secondary_checkpoint_bytes_written
            .fetch_add(meta.bytes, AtomicOrdering::Relaxed);
        if let PublishOutcome::SyncFailed(error) = manifest_outcome {
            return Err(error.into());
        }
    }

    for frozen_job in &job.text {
        *stage = format!(
            "writing text index {}.{}",
            frozen_job.key.0, frozen_job.key.1
        );
        let file = tidx_run_filename(&shared.dir, &frozen_job.key.0, &frozen_job.key.1, 0);
        let path = indexes_dir.join(&file);
        let prepared = path.with_file_name(format!("{file}.derived-pending"));
        let tmp = path.with_file_name(format!("{file}.derived-pending.tmp"));
        if let Err(error) =
            TextIdx::write_frozen_delta_paged(&frozen_job.frozen, &tmp, &temp_dir, budget)
        {
            let _ = fs::remove_file(&tmp);
            return Err(error);
        }
        fs::rename(&tmp, &prepared).map_err(|error| {
            Error::Io(std::io::Error::new(
                error.kind(),
                format!(
                    "publishing frozen text run {} -> {} failed: {error}",
                    tmp.display(),
                    prepared.display()
                ),
            ))
        })?;
        fsync_dir(&indexes_dir)?;
        let mapped = Arc::new(PagedIndex::open(&prepared)?);
        validate_text_run(&mapped)?;
        *stage = format!(
            "publishing text index {}.{}",
            frozen_job.key.0, frozen_job.key.1
        );
        let generation = frozen_job.frozen.generation;
        let bytes = fs::metadata(&prepared)?.len();
        let (meta, manifest) = {
            let _commit = shared.commit.lock();
            let state = shared.state.read().unwrap();
            let Some(index) = state.text.get(&frozen_job.key) else {
                drop(state);
                let _ = fs::remove_file(&prepared);
                continue;
            };
            if !index.frozen_matches(&frozen_job.frozen) {
                drop(state);
                let _ = fs::remove_file(&prepared);
                continue;
            }
            let meta = DerivedRunMeta {
                file,
                level: if index.runs.is_empty() {
                    DERIVED_BASE_LEVEL
                } else {
                    0
                },
                bytes,
                generation,
            };
            let mut metas = index.run_metas();
            metas.push(meta.clone());
            let (doc_count, total_len) = index.frozen_doc_stats();
            let manifest = DerivedRunManifest::new(
                DerivedRunKind::Text,
                &frozen_job.key.0,
                &frozen_job.key.1,
                generation,
                metas,
                [doc_count, total_len],
            );
            (meta, manifest)
        };
        *stage = format!(
            "preparing text run manifest {}.{}",
            frozen_job.key.0, frozen_job.key.1
        );
        let manifest = manifest.prepare(&tidx_manifest_path(
            &shared.dir,
            &frozen_job.key.0,
            &frozen_job.key.1,
        ))?;
        pause_derived_before_publish_for_test(shared);
        fs::rename(&prepared, &path)?;
        fsync_dir(&indexes_dir)?;
        let manifest_outcome = match publish_background_derived_manifest(shared, manifest) {
            Ok(outcome) => outcome,
            Err(error) => {
                let _ = fs::remove_file(&path);
                return Err(error);
            }
        };
        {
            let _commit = shared.commit.lock();
            let mut state = shared.state.write().unwrap();
            let index = state.text.get_mut(&frozen_job.key).ok_or_else(|| {
                Error::Corrupt("text index disappeared during background publication".into())
            })?;
            if !index.frozen_matches(&frozen_job.frozen) {
                return Err(Error::Corrupt(
                    "text frozen generation changed during background publication".into(),
                ));
            }
            index.runs.push(TextRun {
                meta: meta.clone(),
                index: mapped,
            });
            index.generation = generation;
            index.clear_frozen(&frozen_job.frozen);
        }
        shared
            .text_checkpoint_bytes_written
            .fetch_add(meta.bytes, AtomicOrdering::Relaxed);
        if let PublishOutcome::SyncFailed(error) = manifest_outcome {
            return Err(error.into());
        }
    }

    for frozen_job in &job.vector {
        *stage = format!(
            "writing vector index {}.{}",
            frozen_job.table, frozen_job.def.column
        );
        // The run is a durable part of the index from here on: the manifest
        // published below lets the next open map it instead of re-inserting
        // every vector of this generation.
        let file = vidx_run_filename(&shared.dir, &frozen_job.table, &frozen_job.def.column);
        let run = vectors_dir.join(&file);
        let pending = vectors_dir.join(format!("{file}.derived-pending"));
        if let Err(error) = frozen_job.frozen.dump_file(
            &pending,
            &frozen_job.table,
            &frozen_job.def.column,
            &frozen_job.def,
            frozen_job.generation,
        ) {
            let _ = fs::remove_file(&pending);
            return Err(error);
        }
        fs::rename(&pending, &run)?;
        fsync_dir(&vectors_dir)?;
        let (loaded, version) = match VecIdx::load_mmap_file(
            &run,
            &frozen_job.table,
            &frozen_job.def.column,
            &frozen_job.def,
        ) {
            Ok(loaded) => loaded,
            Err(error) => {
                let _ = fs::remove_file(&run);
                return Err(error);
            }
        };
        if version != frozen_job.generation {
            let _ = fs::remove_file(&run);
            return Err(Error::Corrupt(
                "background vector run has the wrong generation".into(),
            ));
        }
        *stage = format!(
            "publishing vector index {}.{}",
            frozen_job.table, frozen_job.def.column
        );
        pause_derived_before_publish_for_test(shared);
        // The manifest is written under the commit mutex so it can never
        // overtake a compaction rebuild that replaced the whole run set.
        let _cs = shared.commit.lock();
        let key = (frozen_job.table.clone(), frozen_job.def.column.clone());
        let manifest = {
            let mut state = shared.state.write().unwrap();
            let published = state
                .vector
                .get_mut(&key)
                .is_some_and(|index| index.publish_frozen_loaded(&frozen_job.frozen, loaded));
            if !published {
                None
            } else {
                state
                    .vector
                    .get_mut(&key)
                    .filter(|index| index.frozen_delta().is_none())
                    .map(|index| {
                        let manifest =
                            vector_manifest_for(&key.0, &key.1, frozen_job.generation, index);
                        if manifest.is_some() {
                            index.durable_generation = Some(frozen_job.generation);
                        }
                        manifest
                    })
            }
        };
        match manifest {
            None => {
                let _ = fs::remove_file(&run);
                continue;
            }
            Some(Some(manifest)) => {
                manifest.publish(&vidx_manifest_path(&shared.dir, &key.0, &key.1))?;
            }
            Some(None) => {}
        }
    }

    drop(job.memory.take());
    let retained = shared.state.read().unwrap().index_delta_memory_bytes();
    shared.memory_governor.set_index_delta_bytes(retained);
    maybe_schedule_secondary_compaction(shared);
    maybe_schedule_text_compaction(shared);
    maybe_schedule_vector_merge(shared);
    Ok(())
}

pub(super) fn consolidate_derived_indexes(shared: &Arc<Shared>) -> Result<()> {
    if shared.opts.read_only {
        return Ok(());
    }
    let idir = shared.dir.join(INDEXES_DIR);
    let vdir = shared.dir.join(VECTORS_DIR);
    fs::create_dir_all(&idir)?;
    fs::create_dir_all(&vdir)?;
    let temp_dir = shared
        .opts
        .memory
        .spill_directory
        .clone()
        .unwrap_or_else(|| idir.join("tmp"));
    let budget = shared.opts.memory.maintenance_pool_bytes;
    let mut st = shared.state.write().unwrap();
    let version = st.committed_version;
    let has_sorted_indexes = !st.secondary.is_empty() || !st.text.is_empty();
    let has_vector_indexes = !st.vector.is_empty();
    let mut secondary_written = 0u64;
    let mut text_written = 0u64;

    let secondary_keys: Vec<_> = st.secondary.keys().cloned().collect();
    for key in secondary_keys {
        let index = st
            .secondary
            .get_mut(&key)
            .expect("secondary key collected above");
        if index.delta.is_empty() && index.removed.is_empty() {
            index.generation = version;
            publish_secondary_manifest(&shared.dir, &key.0, &key.1, version, index)?;
            continue;
        }
        let first = index.runs.is_empty();
        let file = if first {
            sidx_path(&shared.dir, &key.0, &key.1)
                .file_name()
                .and_then(|name| name.to_str())
                .expect("secondary path has utf8 filename")
                .to_owned()
        } else {
            sidx_run_filename(&shared.dir, &key.0, &key.1, 0)
        };
        let path = idir.join(&file);
        let tmp = path.with_file_name(format!("{file}.tmp"));
        let mut writer = ExternalPagedWriter::new(&tmp, &temp_dir, version, budget)?;
        writer.add(SECONDARY_FORMAT_KEY, SECONDARY_FORMAT_VALUE)?;
        for (value, ids) in &index.delta {
            for id in ids {
                writer.add(
                    &secondary_pair_key(value, id),
                    &secondary_operation(version, SECONDARY_ADD),
                )?;
            }
        }
        for (value, ids) in &index.removed {
            for id in ids {
                writer.add(
                    &secondary_pair_key(value, id),
                    &secondary_operation(version, SECONDARY_DELETE),
                )?;
            }
        }
        if let Err(error) = writer.finish() {
            let _ = fs::remove_file(&tmp);
            return Err(error);
        }
        fs::rename(&tmp, &path)?;
        let meta = DerivedRunMeta {
            file,
            level: if first { DERIVED_BASE_LEVEL } else { 0 },
            bytes: fs::metadata(&path)?.len(),
            generation: version,
        };
        secondary_written = secondary_written.saturating_add(meta.bytes);
        let mapped = Arc::new(PagedIndex::open(&path)?);
        validate_secondary_run(&mapped)?;
        index.runs.push(SecRun {
            meta,
            index: mapped,
        });
        index.generation = version;
        index.delta.clear();
        index.removed.clear();
        publish_secondary_manifest(&shared.dir, &key.0, &key.1, version, index)?;
    }

    let text_keys: Vec<_> = st.text.keys().cloned().collect();
    for key in text_keys {
        let index = st.text.get_mut(&key).expect("text key collected above");
        if index.delta_memory_bytes() == 0 {
            index.generation = version;
            publish_text_manifest(&shared.dir, &key.0, &key.1, version, index)?;
            continue;
        }
        let first = index.runs.is_empty();
        let file = if first {
            tidx_path(&shared.dir, &key.0, &key.1)
                .file_name()
                .and_then(|name| name.to_str())
                .expect("text path has utf8 filename")
                .to_owned()
        } else {
            tidx_run_filename(&shared.dir, &key.0, &key.1, 0)
        };
        let path = idir.join(&file);
        let tmp = path.with_file_name(format!("{file}.tmp"));
        if let Err(error) = index.write_delta_paged(&tmp, &temp_dir, version, budget) {
            let _ = fs::remove_file(&tmp);
            return Err(error);
        }
        fs::rename(&tmp, &path)?;
        let meta = DerivedRunMeta {
            file,
            level: if first { DERIVED_BASE_LEVEL } else { 0 },
            bytes: fs::metadata(&path)?.len(),
            generation: version,
        };
        text_written = text_written.saturating_add(meta.bytes);
        let mapped = Arc::new(PagedIndex::open(&path)?);
        validate_text_run(&mapped)?;
        index.runs.push(TextRun {
            meta,
            index: mapped,
        });
        index.freeze_delta(version);
        publish_text_manifest(&shared.dir, &key.0, &key.1, version, index)?;
    }

    // Freeze each mutable HNSW overlay independently. Existing mmap graphs are
    // not rebuilt: searches merge the immutable runs, while canonical
    // segments remain the restart source of truth for ephemeral overlays.
    let vector_defs: Vec<(String, VectorIndexDef)> = st
        .catalog
        .tables
        .iter()
        .flat_map(|table| {
            table
                .vector_indexes
                .iter()
                .cloned()
                .map(|def| (table.name.clone(), def))
                .collect::<Vec<_>>()
        })
        .collect();
    for (table, def) in vector_defs {
        let key = (table.clone(), def.column.clone());
        let Some(index) = st.vector.get_mut(&key) else {
            continue;
        };
        if index.delta_memory_bytes() > 0 {
            // The first durable file of an index is its base; later flushes
            // add runs. Both stay on disk and are listed in the manifest.
            let run = if index.has_mapped_base() {
                vdir.join(vidx_run_filename(&shared.dir, &table, &def.column))
            } else {
                vidx_path(&shared.dir, &table, &def.column)
            };
            index.flush_delta_mmap(&run, &table, &def.column, &def, version)?;
        }
        // A frozen generation still awaiting publication means the runs do
        // not yet cover `version`; the previous manifest stays valid as a
        // prefix and the next open replays the difference.
        if index.frozen_delta().is_none() {
            publish_vector_manifest(&shared.dir, &table, &def.column, version, index)?;
        }
    }
    if has_sorted_indexes {
        fsync_dir(&idir)?;
    }
    if has_vector_indexes {
        fsync_dir(&vdir)?;
    }
    drop(st);
    shared
        .secondary_checkpoint_bytes_written
        .fetch_add(secondary_written, AtomicOrdering::Relaxed);
    shared
        .text_checkpoint_bytes_written
        .fetch_add(text_written, AtomicOrdering::Relaxed);
    maybe_schedule_secondary_compaction(shared);
    maybe_schedule_text_compaction(shared);
    Ok(())
}

/// Rebuild derived indexes after canonical segment rewriting without ever
/// retaining all rebuilt structures at once. Sorted indexes stream directly
/// to paged files; each HNSW graph must fit the maintenance pool, is persisted,
/// and is remapped before the next graph begins.
pub(super) fn rebuild_derived_indexes_after_rewrite(
    shared: &Arc<Shared>,
    rebuild_secondary: bool,
) -> Result<()> {
    let idir = shared.dir.join(INDEXES_DIR);
    let vdir = shared.dir.join(VECTORS_DIR);
    fs::create_dir_all(&idir)?;
    fs::create_dir_all(&vdir)?;
    let temp_dir = shared
        .opts
        .memory
        .spill_directory
        .clone()
        .unwrap_or_else(|| idir.join("tmp"));
    let budget = shared.opts.memory.maintenance_pool_bytes;
    let mut st = shared.state.write().unwrap();
    let version = st.committed_version;

    if rebuild_secondary {
        let definitions: Vec<_> = st
            .catalog
            .tables
            .iter()
            .flat_map(|table| {
                table
                    .indexes
                    .iter()
                    .map(|def| (table.name.clone(), def.column.clone()))
                    .collect::<Vec<_>>()
            })
            .collect();
        let mut rebuilt = HashMap::new();
        for (table, column) in definitions {
            let path = sidx_path(&shared.dir, &table, &column);
            let tmp = path.with_extension("sidx.tmp");
            write_secondary_from_canonical(
                &tmp,
                &temp_dir,
                version,
                budget,
                &st.blobs,
                &table,
                &column,
                &st.index,
                &st.readers,
            )?;
            fs::rename(&tmp, &path)?;
            let meta = DerivedRunMeta {
                file: path
                    .file_name()
                    .and_then(|name| name.to_str())
                    .expect("secondary path has utf8 filename")
                    .to_owned(),
                level: DERIVED_BASE_LEVEL,
                bytes: fs::metadata(&path)?.len(),
                generation: version,
            };
            let index = SecIdx::paged_runs(
                version,
                vec![SecRun {
                    meta,
                    index: Arc::new(PagedIndex::open(&path)?),
                }],
            )?;
            publish_secondary_manifest(&shared.dir, &table, &column, version, &index)?;
            rebuilt.insert((table, column), index);
        }
        st.secondary = rebuilt;
    }

    let text_definitions: Vec<_> = st
        .catalog
        .tables
        .iter()
        .flat_map(|table| {
            table
                .text_indexes
                .iter()
                .map(|def| (table.name.clone(), def.column.clone()))
                .collect::<Vec<_>>()
        })
        .collect();
    let mut rebuilt_text = HashMap::new();
    for (table, column) in text_definitions {
        let path = tidx_path(&shared.dir, &table, &column);
        let tmp = path.with_extension("tidx.tmp");
        let (doc_count, total_len) = write_text_from_canonical(
            &tmp,
            &temp_dir,
            version,
            budget,
            &st.blobs,
            &table,
            &column,
            &st.index,
            &st.readers,
        )?;
        fs::rename(&tmp, &path)?;
        let meta = DerivedRunMeta {
            file: path
                .file_name()
                .and_then(|name| name.to_str())
                .expect("text path has utf8 filename")
                .to_owned(),
            level: DERIVED_BASE_LEVEL,
            bytes: fs::metadata(&path)?.len(),
            generation: version,
        };
        let index = TextIdx::paged_runs(
            version,
            vec![TextRun {
                meta,
                index: Arc::new(PagedIndex::open(&path)?),
            }],
            doc_count,
            total_len,
        )?;
        publish_text_manifest(&shared.dir, &table, &column, version, &index)?;
        rebuilt_text.insert((table, column), index);
    }
    st.text = rebuilt_text;

    let vector_definitions: Vec<_> = st
        .catalog
        .tables
        .iter()
        .flat_map(|table| {
            table
                .vector_indexes
                .iter()
                .cloned()
                .map(|def| (table.name.clone(), def))
                .collect::<Vec<_>>()
        })
        .collect();
    let mut rebuilt_vector = HashMap::new();
    for (table, def) in vector_definitions {
        let resident =
            build_one_vector_index(&st.blobs, &table, &def, &st.index, &st.readers, budget)?;
        let path = vidx_path(&shared.dir, &table, &def.column);
        let tmp = path.with_extension("vidx.tmp");
        resident.dump_file(&tmp, &table, &def.column, &def, version)?;
        fs::rename(&tmp, &path)?;
        let (mut mapped, dump_version) = VecIdx::load_mmap_file(&path, &table, &def.column, &def)?;
        if dump_version != version {
            return Err(Error::Corrupt(
                "rewritten vector index has the wrong generation".into(),
            ));
        }
        publish_vector_manifest(&shared.dir, &table, &def.column, version, &mut mapped)?;
        cleanup_vector_run_orphans(&shared.dir, &table, &def.column, &mapped);
        rebuilt_vector.insert((table, def.column.clone()), mapped);
    }
    st.vector = rebuilt_vector;
    fsync_dir(&idir)?;
    fsync_dir(&vdir)?;
    Ok(())
}
