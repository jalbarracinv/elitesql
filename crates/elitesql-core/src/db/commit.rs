//! Commit internals; shared state and lock ownership remain in db.rs.
use super::*;

/// The optimistic commit path. Serialized by the commit mutex; readers are
/// only blocked during the short in-memory apply at the end.
pub(super) fn finish_or_wait_wal_sync(
    shared: &Arc<Shared>,
    group: Arc<WalSyncGroup>,
    leader: bool,
) -> WalGroupResult {
    if !leader {
        return group.wait();
    }

    // Only a contended writer opens the coalescing window. Uncontended Safe
    // commits retain their original one-sync latency.
    if shared.commit_waiters.load(AtomicOrdering::Acquire) > 0
        && shared.opts.safe_group_commit_delay_us > 0
    {
        let coalesce_started = Instant::now();
        std::thread::sleep(Duration::from_micros(
            shared.opts.safe_group_commit_delay_us,
        ));
        shared.wal_group_coalesce_nanos.fetch_add(
            elapsed_nanos(coalesce_started.elapsed()),
            AtomicOrdering::Relaxed,
        );
    }

    let leader_lock_started = Instant::now();
    let mut cs = shared.commit.lock();
    shared.wal_group_leader_lock_wait_nanos.fetch_add(
        elapsed_nanos(leader_lock_started.elapsed()),
        AtomicOrdering::Relaxed,
    );
    debug_assert!(
        cs.wal_sync_group
            .as_ref()
            .is_some_and(|active| Arc::ptr_eq(active, &group)),
        "the elected WAL sync group stays active until its leader completes"
    );
    let sync_started = Instant::now();
    let outcome = cs.wal().sync_data();
    let sync_time = sync_started.elapsed();
    cs.wal_sync_group = None;
    let commits = group.commits.load(AtomicOrdering::Acquire);
    let bytes = group.bytes.load(AtomicOrdering::Acquire);
    record_wal_sync(shared, sync_time, commits, bytes);
    drop(cs);

    let result = match outcome {
        WalAppendOutcome::Complete => Ok(()),
        WalAppendOutcome::SyncFailed(error) => {
            fence_after_wal_sync_failure(shared, &error);
            Err(error.to_string())
        }
    };
    group.complete(result.clone());
    result
}

/// After a failed WAL barrier the kernel may already have discarded the dirty
/// pages of the framed records; a later successful sync would then acknowledge
/// a commit that depends on lost bytes. Fence every further write until the
/// database is reopened and the chain re-validated.
pub(super) fn fence_after_wal_sync_failure(shared: &Shared, error: &std::io::Error) {
    fence_writes(
        shared,
        format!("WAL sync failed; the durability of the last commit group is unknown: {error}"),
    );
}

pub(super) fn elapsed_nanos(duration: Duration) -> u64 {
    duration.as_nanos().min(u64::MAX as u128) as u64
}

pub(super) fn record_wal_sync(shared: &Shared, elapsed: Duration, commits: u64, bytes: u64) {
    shared.wal_sync_count.fetch_add(1, AtomicOrdering::Relaxed);
    shared
        .wal_sync_nanos
        .fetch_add(elapsed_nanos(elapsed), AtomicOrdering::Relaxed);
    shared
        .wal_synced_bytes
        .fetch_add(bytes, AtomicOrdering::Relaxed);
    shared
        .wal_sync_max_group_commits
        .fetch_max(commits, AtomicOrdering::Relaxed);
    shared
        .wal_sync_max_group_bytes
        .fetch_max(bytes, AtomicOrdering::Relaxed);
    if commits > 1 {
        shared
            .grouped_commit_count
            .fetch_add(commits, AtomicOrdering::Relaxed);
    }
}

/// The state write guard for publishing a commit, counted in
/// `state_write_waiting` while it is being waited for (the point-read
/// admission reads it).
#[track_caller]
pub(super) fn write_state_for_commit(
    shared: &Shared,
) -> crate::lock_probe::ProbedWriteGuard<'_, State, ReadView> {
    shared
        .state_write_waiting
        .fetch_add(1, AtomicOrdering::AcqRel);
    let guard = shared.state.write().unwrap();
    shared
        .state_write_waiting
        .fetch_sub(1, AtomicOrdering::AcqRel);
    guard
}

pub(super) fn lock_commit_for_transaction(shared: &Shared) -> TimedCommitGuard<'_> {
    TimedCommitGuard {
        guard: Some(shared.commit.lock()),
        started: Instant::now(),

        total_nanos: &shared.commit_lock_hold_nanos,
        waiters: &shared.commit_waiters,
        // Safe group commit benefits from allowing the current CPU to append
        // several queued records before the elected leader fsyncs them. Fast
        // and Balanced need fair handoff to bound mutex-tail latency instead.
        fair_handoff: shared.opts.durability != Durability::Safe,
    }
}

pub(super) fn commit_staged(
    shared: &Arc<Shared>,
    snap_version: u64,
    staged_tables: Vec<(String, StagedTable)>,
) -> Result<u64> {
    if shared.opts.read_only {
        return Err(Error::ReadOnly);
    }
    ensure_canonical_writable(shared)?;
    if staged_tables
        .iter()
        .all(|(_, staged_table)| staged_table.operations.is_empty())
    {
        return Ok(shared.state.read().unwrap().committed_version);
    }
    let _active_state_writer = ActiveStateWriter::enter(shared);
    let prepared = prepare_commit(shared, snap_version, staged_tables);
    let prepared = prepared?;
    // Only commits the batch path can actually take go through the
    // coordinator. Routing every commit through it was measured and is worse:
    // nothing extra batches, so the leader runs them in sequence exactly as
    // the mutex did, and the queueing hop costs five times the median latency
    // (7.6 ms against 1.4 at 500 in-flight requests) for throughput inside
    // run-to-run noise. The queue has to shorten, not move.
    if is_coordinated_insert_candidate(&prepared) {
        coordinate_commit(shared, prepared)
    } else {
        finish_prepared_commit(shared, prepared)
    }
}

/// Whether a commit may join a coordinated batch.
///
/// Secondary, text and vector indexes, identity columns and updates are all
/// allowed: the batch validates its members against the committed state *and*
/// against each other (`validate_unique` and `validate_foreign_keys` take the
/// whole batch), it refuses any batch whose members touch a common row, it
/// carries each member's prior record into the apply, and it hands the vector
/// indexing jobs the apply produces to the same worker the single-commit path
/// uses. A commit with no pre-encoded WAL frame cannot join, because the
/// batch appends the frames of its members as one vectored write.
///
/// A table carrying a text or vector index stays out, and the reason is
/// measured rather than structural: in the shop workload that is the product
/// catalogue, whose few hot rows every checkout and restock contends for.
/// Letting those commits queue for the coordinator, even with only the
/// colliding member set aside, took the batched share from 84 % to 28 % and
/// throughput at 500 in-flight requests from 9 814 to 6 401 operations a
/// second, because the contended writes gained a queueing hop and won nothing.
pub(super) fn is_coordinated_insert_candidate(prepared: &PreparedCommit) -> bool {
    prepared.preencoded_wal.is_some()
        && prepared.staged.iter().all(|table| {
            table.schema.text_indexes.is_empty() && table.schema.vector_indexes.is_empty()
        })
}

/// One staged table's superseded versions: for each change, the version it
/// replaces and the record that version held, when a derived index needs it.
type PreviousEntries = Vec<(Option<VersionEntry>, Option<Record>)>;

pub(super) fn prepare_commit(
    shared: &Arc<Shared>,
    snap_version: u64,
    staged_tables: Vec<(String, StagedTable)>,
) -> Result<PreparedCommit> {
    let commit_started = Instant::now();
    let prepare_started = Instant::now();
    let payload_capacity_started = Instant::now();
    let payload_capacity = staged_tables
        .iter()
        .flat_map(|(_, table)| {
            table.operations.values().filter_map(|operation| {
                operation
                    .operation
                    .as_ref()
                    .map(|record| encoded_record_len_hint(&table.schema, record))
            })
        })
        .fold(0usize, usize::saturating_add);
    let mut record_encode_time = payload_capacity_started.elapsed();
    let mut payload_arena = Vec::with_capacity(payload_capacity);
    let mut blob_sink = BlobSink::new(&shared.dir, shared.opts.external_blob_threshold);
    let mut staged = Vec::with_capacity(staged_tables.len());
    for (name, staged_table) in staged_tables {
        if staged_table.operations.is_empty() {
            continue;
        }
        let operation_count = staged_table.operations.len();
        let ordered = staged_table.operations.into_ordered();
        let mut changes = Vec::with_capacity(operation_count);
        let record_encode_started = Instant::now();
        for (id, operation, rebase) in ordered {
            let payload = match &operation {
                Some(record) => {
                    let start = u32::try_from(payload_arena.len()).map_err(|_| {
                        Error::InvalidArgument(
                            "transaction payload arena exceeds the 4-GiB WAL limit".into(),
                        )
                    })?;
                    encode_record_ordered_into(
                        &mut payload_arena,
                        &staged_table.schema,
                        record,
                        Some(&mut blob_sink),
                    )?;
                    let end = u32::try_from(payload_arena.len()).map_err(|_| {
                        Error::InvalidArgument(
                            "transaction payload arena exceeds the 4-GiB WAL limit".into(),
                        )
                    })?;
                    Some((start, end - start))
                }
                None => None,
            };
            changes.push(PreparedChange {
                id,
                operation,
                rebase,
                payload,
            });
        }
        record_encode_time = record_encode_time.saturating_add(record_encode_started.elapsed());
        staged.push(PreparedTable {
            name,
            schema: staged_table.schema,
            changes,
        });
    }
    staged.sort_unstable_by(|left, right| left.name.cmp(&right.name));
    let payload_arena = Arc::new(payload_arena);
    // Encode the potentially large WAL payload before taking the global commit
    // mutex, then patch the final version and CRC once conflict validation
    // assigns it. Identity high-water marks are atomic counters that only
    // grow and were advanced when this transaction reserved its values, so
    // reading them here records a mark at least as high as every identity in
    // the frame; recovery keeps the maximum across records anyway.
    let wal_encode_started = Instant::now();
    let identity_marks: Vec<(String, i64)> = {
        // The counters are shared with the read view, so no lock is taken.
        let view = shared.state.view();
        staged
            .iter()
            .filter(|table| table.schema.columns.iter().any(|column| column.identity))
            .filter_map(|table| {
                view.identities
                    .get(&table.name)
                    .map(|value| (table.name.clone(), value.load(AtomicOrdering::Relaxed)))
            })
            .collect()
    };
    let identity_wal: Vec<(&str, i64)> = identity_marks
        .iter()
        .map(|(table, value)| (table.as_str(), *value))
        .collect();
    let preencoded_wal = {
        let wal_changes: Vec<(&str, &str, Option<&[u8]>)> = staged
            .iter()
            .flat_map(|table| {
                table.changes.iter().map(|change| {
                    let payload = change.payload.map(|(start, len)| {
                        let start = start as usize;
                        &payload_arena[start..start + len as usize]
                    });
                    (table.name.as_str(), change.id.as_str(), payload)
                })
            })
            .collect();
        Some(encode_commit(0, &wal_changes, &identity_wal)?)
    };
    let wal_encode_time = wal_encode_started.elapsed();
    // Blob files are already individually synced. Publish their names and
    // sync the blob directory before contending for the commit mutex; the
    // pending set makes concurrent compaction GC treat them as referenced
    // until either this commit applies or aborts.
    let pending_blobs = PendingBlobPublications::new(shared.clone(), &blob_sink);
    blob_sink.publish()?;
    #[cfg(test)]
    {
        shared
            .blob_test_reached_publish
            .store(true, AtomicOrdering::Release);
        while shared
            .blob_test_pause_after_publish
            .load(AtomicOrdering::Acquire)
        {
            std::thread::yield_now();
        }
    }
    // Decode prior indexed records optimistically while writers still prepare
    // in parallel. Commit validation below re-reads each latest version under
    // the serialization mutex; a cached record is used only when its version
    // still matches, so concurrent updates remain conflicts and checkpoint
    // representation changes remain safe.
    let has_derived_indexes = staged.iter().any(|table| {
        !table.schema.indexes.is_empty()
            || !table.schema.text_indexes.is_empty()
            || !table.schema.vector_indexes.is_empty()
    });
    let (incoming_index_bytes, optimistic_prior_records) = if has_derived_indexes {
        // From the read view: these are estimates and a cache, both checked
        // again under the commit mutex, so a view a commit behind is fine and
        // the state lock is not taken.
        let state = shared.state.view();
        let incoming_index_bytes = estimate_staged_index_bytes(shared, &state, &staged)?;
        let records = staged
            .iter()
            .map(|table| {
                table
                    .changes
                    .iter()
                    .map(|change| {
                        if table.schema.indexes.is_empty() && table.schema.text_indexes.is_empty() {
                            return Ok(None);
                        }
                        let previous = if state.id_is_above_high_watermark(&table.name, &change.id)
                        {
                            None
                        } else {
                            state.latest_owned(&table.name, &change.id)?
                        };
                        // A record that already moved past this snapshot can
                        // only fail validation; report the conflict now,
                        // instead of after queueing for
                        // the commit mutex and spending its time to learn it.
                        // The locked validation below remains authoritative.
                        // A delta update is replayed over the newer version
                        // instead, so it goes on to the locked validation.
                        if change.rebase.is_none()
                            && previous
                                .as_ref()
                                .is_some_and(|entry| entry.version > snap_version)
                        {
                            return Err(Error::Conflict(format!(
                                "{}/{} changed after this transaction began",
                                table.name, change.id
                            )));
                        }
                        match previous {
                            Some(entry) if !entry.is_tombstone() => Ok(Some((
                                entry.version,
                                read_prior_for_indexes(
                                    &shared.blobs,
                                    &state.readers,
                                    &entry.kind,
                                    &table.schema,
                                )?,
                            ))),
                            _ => Ok(None),
                        }
                    })
                    .collect::<Result<Vec<_>>>()
            })
            .collect::<Result<Vec<_>>>()?;
        (incoming_index_bytes, records)
    } else {
        let bytes = staged
            .iter()
            .flat_map(|table| &table.changes)
            .map(|change| {
                change
                    .id
                    .len()
                    .saturating_add(change.payload.map_or(0, |(_, len)| len as usize))
                    .saturating_add(144)
            })
            .fold(0usize, usize::saturating_add);
        let records = staged
            .iter()
            .map(|table| vec![None; table.changes.len()])
            .collect();
        (bytes, records)
    };
    if incoming_index_bytes > shared.memory_governor.index_capacity() {
        return Err(Error::MemoryLimit(format!(
            "transaction needs an estimated {incoming_index_bytes} index-delta bytes, but the pool is {} bytes; split the transaction or raise memory.index_delta_pool_bytes",
            shared.memory_governor.index_capacity()
        )));
    }
    let outside_prepare_time = prepare_started.elapsed();
    let phase_prepare_time = outside_prepare_time
        .saturating_sub(record_encode_time)
        .saturating_sub(wal_encode_time);

    Ok(PreparedCommit {
        snap_version,
        staged,
        payload_arena,
        preencoded_wal,
        optimistic_prior_records,
        incoming_index_bytes,
        outside_prepare_time,
        phase_prepare_time,
        record_encode_time,
        wal_encode_time,
        commit_started,
        _pending_blobs: pending_blobs,
    })
}

pub(super) fn coordinate_commit(shared: &Arc<Shared>, prepared: PreparedCommit) -> Result<u64> {
    let request = Arc::new(CoordinatedCommit::new(prepared));
    let mut leader = {
        let mut coordinator = shared.commit_coordinator.lock().unwrap();
        coordinator.queue.push_back(request.clone());
        if coordinator.active {
            false
        } else {
            coordinator.active = true;
            true
        }
    };

    loop {
        if leader {
            if should_coalesce_safe_batch(shared) {
                let coalesce_started = Instant::now();
                std::thread::sleep(Duration::from_micros(
                    shared.opts.safe_group_commit_delay_us,
                ));
                shared.wal_group_coalesce_nanos.fetch_add(
                    elapsed_nanos(coalesce_started.elapsed()),
                    AtomicOrdering::Relaxed,
                );
            }
            let batch = {
                let mut coordinator = shared.commit_coordinator.lock().unwrap();
                if let Some(promoted) = coordinator.promoted_at.take() {
                    shared
                        .coordinator_handoffs
                        .fetch_add(1, AtomicOrdering::Relaxed);
                    shared
                        .coordinator_handoff_nanos
                        .fetch_add(elapsed_nanos(promoted.elapsed()), AtomicOrdering::Relaxed);
                }
                let take = coordinator.queue.len().min(COMMIT_COORDINATOR_MAX_BATCH);
                coordinator.queue.drain(..take).collect::<Vec<_>>()
            };
            process_coordinated_batch(shared, batch);
            let mut coordinator = shared.commit_coordinator.lock().unwrap();
            if let Some(next) = coordinator.queue.front().cloned() {
                coordinator.promoted_at = Some(Instant::now());
                next.promote_to_leader();
            } else {
                coordinator.active = false;
            }
        }
        if let Some(result) = request.take_result() {
            return result;
        }
        match request.wait_for_result_or_lead() {
            Some(result) => return result,
            None => leader = true,
        }
    }
}

pub(super) fn should_coalesce_safe_batch(shared: &Shared) -> bool {
    if shared.opts.durability != Durability::Safe || shared.opts.safe_group_commit_delay_us == 0 {
        return false;
    }
    if shared.active_state_writers.load(AtomicOrdering::Acquire) > 1 {
        // Preserve a short contention memory across the tiny gap between two
        // writers finishing one batch and staging the next. Without it, a
        // two-writer workload can alternate just cleanly enough for each new
        // leader to misclassify the stream as single-writer.
        shared
            .safe_coalesce_budget
            .store(8, AtomicOrdering::Release);
        return true;
    }
    shared
        .safe_coalesce_budget
        .fetch_update(AtomicOrdering::AcqRel, AtomicOrdering::Acquire, |budget| {
            budget.checked_sub(1)
        })
        .is_ok()
}

pub(super) fn process_coordinated_batch(shared: &Arc<Shared>, batch: Vec<Arc<CoordinatedCommit>>) {
    let prepared = batch
        .iter()
        .map(|request| {
            request
                .prepared
                .lock()
                .unwrap()
                .take()
                .expect("coordinated commit is processed once")
        })
        .collect::<Vec<_>>();
    let queue_wait = batch
        .iter()
        .map(|request| request.queued_at.elapsed())
        .fold(Duration::ZERO, Duration::saturating_add);

    match finish_coordinated_insert_batch(shared, prepared, queue_wait) {
        Ok(outcome) => {
            for (member, result) in outcome.results {
                batch[member].complete(result);
            }
            // Members the batch set aside commit one by one after it, in
            // queue order, exactly as a batch that could not form at all.
            for (member, prepared) in outcome.set_aside {
                batch[member].complete(finish_prepared_commit(shared, prepared));
            }
        }
        Err(prepared) => {
            for (request, prepared) in batch.into_iter().zip(prepared) {
                request.complete(finish_prepared_commit(shared, prepared));
            }
        }
    }
}

/// Records a batch member replays: `(table, change, record)` for each delta
/// update whose row changed after the member's snapshot.
type Replayed = Vec<(usize, usize, Record)>;

/// The superseded versions of every change of `commit`, with the delta
/// updates it has to replay over a newer version, or `None` when the batch
/// has to set it aside: it writes a row in `taken`, one of its rows changed
/// after its snapshot without a delta to replay, or a read failed. Every
/// fallible read happens here, before the WAL durability point, exactly as
/// the single path requires.
fn batch_member_previous(
    state: &State,
    commit: &PreparedCommit,
    taken: &HashSet<(&str, &str)>,
) -> Option<(Vec<PreviousEntries>, Replayed)> {
    let mut replayed = Vec::new();
    let mut commit_previous = Vec::with_capacity(commit.staged.len());
    for (table_index, table) in commit.staged.iter().enumerate() {
        let mut table_previous = Vec::with_capacity(table.changes.len());
        for (change_index, change) in table.changes.iter().enumerate() {
            if taken.contains(&(table.name.as_str(), change.id.as_str())) {
                return None;
            }
            let previous = if state.id_is_above_high_watermark(&table.name, &change.id) {
                None
            } else {
                state.latest_owned(&table.name, &change.id).ok()?
            };
            // Write-write conflict, or a delta update to replay, against this
            // member's own snapshot: the single path does both.
            if let Some(last) = previous
                .as_ref()
                .filter(|last| last.version > commit.snap_version)
            {
                let record = rebase_change(state, &table.schema, change, last).ok()??;
                replayed.push((table_index, change_index, record));
            }
            let prior_record = match &previous {
                Some(entry)
                    if !entry.is_tombstone()
                        && (!table.schema.indexes.is_empty()
                            || !table.schema.text_indexes.is_empty()) =>
                {
                    match &commit.optimistic_prior_records[table_index][change_index] {
                        Some((version, record)) if *version == entry.version => {
                            Some(record.clone())
                        }
                        _ => Some(
                            read_prior_for_indexes(
                                &state.blobs,
                                &state.readers,
                                &entry.kind,
                                &table.schema,
                            )
                            .ok()?,
                        ),
                    }
                }
                _ => None,
            };
            table_previous.push((previous, prior_record));
        }
        commit_previous.push(table_previous);
    }
    Some((commit_previous, replayed))
}

/// Put a batch member's replayed records in place of the staged ones: new
/// payloads in its arena and a new WAL frame, as the single path does at its
/// durability point. Identity marks are read again from the committed state;
/// they only grow, and recovery keeps the maximum.
fn replay_batch_member(
    state: &State,
    commit: &mut PreparedCommit,
    replayed: Replayed,
) -> Result<()> {
    let arena = Arc::make_mut(&mut commit.payload_arena);
    for (table_index, change_index, record) in replayed {
        let table = &mut commit.staged[table_index];
        let start = u32::try_from(arena.len()).map_err(|_| {
            Error::InvalidArgument("transaction payload arena exceeds the 4-GiB WAL limit".into())
        })?;
        encode_record_ordered_into(arena, &table.schema, &record, None)?;
        let end = u32::try_from(arena.len()).map_err(|_| {
            Error::InvalidArgument("transaction payload arena exceeds the 4-GiB WAL limit".into())
        })?;
        let change = &mut table.changes[change_index];
        change.payload = Some((start, end - start));
        change.operation = Some(record);
    }
    let identity_marks: Vec<(&str, i64)> = commit
        .staged
        .iter()
        .filter(|table| table.schema.columns.iter().any(|column| column.identity))
        .filter_map(|table| {
            state
                .identity_high_water
                .get(&table.name)
                .map(|value| (table.name.as_str(), value.load(AtomicOrdering::Relaxed)))
        })
        .collect();
    let arena = &commit.payload_arena;
    let wal_changes: Vec<(&str, &str, Option<&[u8]>)> = commit
        .staged
        .iter()
        .flat_map(|table| {
            table.changes.iter().map(|change| {
                let payload = change.payload.map(|(start, len)| {
                    let start = start as usize;
                    &arena[start..start + len as usize]
                });
                (table.name.as_str(), change.id.as_str(), payload)
            })
        })
        .collect();
    let frame = encode_commit(0, &wal_changes, &identity_marks)?;
    commit.preencoded_wal = Some(frame);
    Ok(())
}

/// Count what one applied change makes obsolete, for auto-compaction.
fn count_obsolete(
    table: &str,
    change: &PreparedChange,
    previous: Option<&VersionEntry>,
    operations: &mut u64,
    bytes: &mut u64,
) {
    if let Some(previous) = previous {
        // An update, delete, or reinsert supersedes one previously visible
        // record version. Count rows, not SQL statements.
        *operations += 1;
        *bytes = bytes.saturating_add(obsolete_entry_bytes_estimate(table, &change.id, previous));
    } else if change.operation.is_none() {
        // Insert-then-delete inside one transaction leaves only a tombstone,
        // which compaction can discard completely.
        *operations += 1;
    }
}

fn record_obsolete(shared: &Shared, operations: u64, bytes: u64) {
    if operations > 0 {
        let mut auto = shared.auto_compaction_state.lock().unwrap();
        auto.debt_operations = auto.debt_operations.saturating_add(operations);
        auto.estimated_reclaimable_bytes = auto.estimated_reclaimable_bytes.saturating_add(bytes);
    }
}

/// Hand the asynchronous vector indexing a commit produced to the worker.
fn send_vector_jobs(shared: &Shared, jobs: Vec<VecJob>) {
    if jobs.is_empty() {
        return;
    }
    let tx = shared.vector_tx.lock().unwrap();
    if let Some(tx) = tx.as_ref() {
        shared
            .vector_backlog
            .fetch_add(jobs.len() as u64, AtomicOrdering::SeqCst);
        for job in jobs {
            if tx.send(job).is_err() {
                *shared
                    .vector_worker_error
                    .lock()
                    .unwrap_or_else(|poison| poison.into_inner()) =
                    Some("vector indexing worker stopped before accepting committed work".into());
                // Keep the backlog charged: the vector is committed but not
                // searchable. A waiter must report the dead worker, not claim
                // that indexing completed successfully.
            }
        }
    }
}

/// Whether to publish derived deltas in the background: once the shared
/// delta pool is half full, but only when they are worth a run. The pool is
/// shared with the primary memtable, and publishing a few thousand vectors
/// every commit while the memtable is what fills it would create tiny HNSW
/// runs faster than background merges can fold them. One sixteenth of the
/// pool is about the largest run four of which a merge can still rebuild
/// within its half of the maintenance pool. Hard pool pressure still
/// publishes whatever exists.
fn derived_schedule_needed(shared: &Arc<Shared>, commit_version: u64) -> bool {
    let memory = shared.memory_governor.stats();
    memory.index_delta_bytes >= memory.index_delta_capacity_bytes / 2
        && sampled_derived_delta_bytes(shared, commit_version) as u64
            >= (memory.index_delta_capacity_bytes / DERIVED_PUBLICATION_MIN_DIVISOR).max(1)
}

/// Background work a commit may have to schedule once it released the
/// commit mutex: derived delta publication and the primary checkpoint.
fn post_commit_maintenance(
    shared: &Arc<Shared>,
    maintenance_needed: bool,
    derived_schedule_needed: bool,
    commit_version: u64,
) -> Result<()> {
    if !(maintenance_needed || derived_schedule_needed) {
        return Ok(());
    }
    let mut cs = lock_commit_after_group_sync(shared);
    if derived_schedule_needed && !shared.background_derived.lock().unwrap().running {
        let derived_bytes = shared.state.read().unwrap().derived_delta_memory_bytes();
        shared
            .derived_delta_size_walks
            .fetch_add(1, AtomicOrdering::Relaxed);
        if let Some(memory) = try_acquire_frozen_lease(shared, derived_bytes) {
            let _ = schedule_frozen_derived(shared, memory)?;
            // The overlays this sample described have just been frozen for
            // publication. Leaving the old figure standing would keep asking
            // for a job that has already been taken, and the extra runs that
            // produces are real work.
            reset_derived_delta_sample(shared, commit_version);
        }
    }
    if cs.memtable_bytes >= shared.opts.memtable_max_bytes {
        // The derived worker owns the sole maintenance reservation and needs
        // this mutex for its final publication. Defer the soft-threshold
        // primary checkpoint instead of waiting for that reservation while
        // holding the mutex. A later commit schedules it; hard index pressure
        // waits outside the mutex.
        if shared.background_derived.lock().unwrap().running {
            return Ok(());
        }
        let frozen_running = shared.state.read().unwrap().index.frozen.is_some();
        if !frozen_running {
            let memory = acquire_frozen_lease(shared, cs.memtable_bytes as usize);
            let _ = schedule_frozen_checkpoint(shared, &mut cs, memory)?;
        }
    }
    Ok(())
}

/// What a coordinated batch did with its members: the result of each one it
/// committed, and the ones it set aside for the single-commit path, each with
/// its position in the batch.
pub(super) struct BatchOutcome {
    results: Vec<(usize, Result<u64>)>,
    set_aside: Vec<(usize, PreparedCommit)>,
}

/// Coordinated path: many commits under one hold of the serialization mutex.
/// Validation still happens under that mutex, every transaction receives its
/// own version and WAL frame, and readers see the complete batch only after
/// the vectored append and one state publication finish. `Safe` batches
/// issue one strict durability barrier before any member returns.
///
/// A member the batch cannot take as it is (it touches a row an earlier
/// member already writes, its row changed after its snapshot, or reading its
/// prior version failed) is set aside and returned to the caller, which
/// commits it on the single-commit path after the batch: that path replays
/// delta updates and raises conflicts with the message callers expect. The
/// rest of the batch goes ahead without it. `Err` hands every member back
/// when no batch can form at all.
pub(super) fn finish_coordinated_insert_batch(
    shared: &Arc<Shared>,
    mut prepared: Vec<PreparedCommit>,
    queue_wait: Duration,
) -> std::result::Result<BatchOutcome, Vec<PreparedCommit>> {
    if prepared.len() < 2 {
        return Err(prepared);
    }
    debug_assert!(prepared.iter().all(is_coordinated_insert_candidate));
    if shared
        .background_checkpoint
        .lock()
        .unwrap()
        .last_error
        .is_some()
    {
        return Err(prepared);
    }
    let Some(incoming_bytes) = prepared.iter().try_fold(0usize, |total, commit| {
        total.checked_add(commit.incoming_index_bytes)
    }) else {
        return Err(prepared);
    };
    if shared.memory_governor.index_would_exceed(incoming_bytes) {
        return Err(prepared);
    }

    let lock_started = Instant::now();
    shared.commit_waiters.fetch_add(1, AtomicOrdering::AcqRel);
    let mut cs = lock_commit_for_transaction(shared);
    shared.commit_waiters.fetch_sub(1, AtomicOrdering::AcqRel);

    let lock_wait = lock_started.elapsed().saturating_add(queue_wait);
    let locked_prepare_started = Instant::now();
    // Members of the batch touch disjoint rows. That is what lets each of
    // them be validated exactly as the single-commit path validates it: with
    // no row in common, no member can be another's prior version or
    // invalidate its conflict check. A member that overlaps one already taken
    // is set aside, not the batch.
    let mut ids: HashSet<(&str, &str)> = HashSet::new();
    let mut rebases: Vec<Vec<(usize, usize, Record)>> = Vec::with_capacity(prepared.len());
    let mut rebased_rows = 0u64;
    // Per member: `None` when it is set aside, otherwise per table, per
    // change, the version this change supersedes and, when a derived index
    // has to drop its old keys, that record.
    let mut previous_entries: Vec<Option<Vec<PreviousEntries>>> =
        Vec::with_capacity(prepared.len());
    let start_version = {
        let state = shared.state.read().unwrap();
        for commit in prepared.iter() {
            if commit
                .staged
                .iter()
                .any(|table| state.catalog.table(&table.name) != Some(&table.schema))
            {
                drop(state);
                drop(cs);
                return Err(prepared);
            }
            match batch_member_previous(&state, commit, &ids) {
                Some((previous, replayed)) => {
                    for table in &commit.staged {
                        for change in &table.changes {
                            ids.insert((table.name.as_str(), change.id.as_str()));
                        }
                    }
                    previous_entries.push(Some(previous));
                    rebases.push(replayed);
                }
                None => {
                    previous_entries.push(None);
                    rebases.push(Vec::new());
                }
            }
        }
        drop(ids);
        // Delta updates replayed over a newer version replace their staged
        // record, payload and WAL frame before anything validates them.
        for ((commit, replayed), previous) in prepared
            .iter_mut()
            .zip(rebases)
            .zip(previous_entries.iter_mut())
        {
            if replayed.is_empty() {
                continue;
            }
            let rows = replayed.len() as u64;
            match replay_batch_member(&state, commit, replayed) {
                Ok(()) => rebased_rows += rows,
                Err(_) => *previous = None,
            }
        }
        // Uniqueness and foreign keys over the members taken, at once: these
        // already check a slice of staged tables against the committed state
        // and against itself, which is exactly what members needing to not
        // collide with each other means.
        let borrowed: Vec<&PreparedTable> = prepared
            .iter()
            .zip(&previous_entries)
            .filter(|(_, previous)| previous.is_some())
            .flat_map(|(commit, _)| commit.staged.iter())
            .collect();
        if validate_unique(&state, &borrowed).is_err()
            || validate_foreign_keys(&state, &borrowed).is_err()
        {
            drop(borrowed);
            drop(state);
            drop(cs);
            return Err(prepared);
        }
        state.committed_version
    };
    if rebased_rows > 0 {
        shared
            .delta_rebased_rows
            .fetch_add(rebased_rows, AtomicOrdering::Relaxed);
    }
    let mut members = Vec::with_capacity(prepared.len());
    let mut set_aside = Vec::new();
    let mut taken = Vec::with_capacity(prepared.len());
    let mut taken_previous = Vec::with_capacity(prepared.len());
    for (member, (commit, previous)) in prepared.into_iter().zip(previous_entries).enumerate() {
        match previous {
            Some(previous) => {
                members.push(member);
                taken.push(commit);
                taken_previous.push(previous);
            }
            None => set_aside.push((member, commit)),
        }
    }
    let mut prepared = taken;
    let previous_entries = taken_previous;
    if prepared.is_empty() {
        drop(cs);
        return Ok(BatchOutcome {
            results: Vec::new(),
            set_aside,
        });
    }
    // Members set aside charge the pool themselves when they commit.
    let incoming_bytes = prepared.iter().fold(0usize, |total, commit| {
        total.saturating_add(commit.incoming_index_bytes)
    });

    let batch_len = u64::try_from(prepared.len()).expect("coordinator batch length fits u64");
    let Some(end_version) = start_version.checked_add(batch_len) else {
        drop(cs);
        return Ok(BatchOutcome {
            results: members
                .into_iter()
                .map(|member| {
                    (
                        member,
                        Err(Error::InvalidArgument(
                            "commit version space is exhausted".into(),
                        )),
                    )
                })
                .collect(),
            set_aside,
        });
    };

    let versions = ((start_version + 1)..=end_version).collect::<Vec<_>>();
    let locked_prepare_time = locked_prepare_started.elapsed();

    let wal_started = Instant::now();
    let wal_encode_started = Instant::now();
    for (commit, version) in prepared.iter_mut().zip(&versions) {
        set_encoded_commit_version(
            commit
                .preencoded_wal
                .as_mut()
                .expect("insert batch has preencoded WAL"),
            *version,
        );
    }
    let version_encode_time = wal_encode_started.elapsed();
    let wal_append_started = Instant::now();
    let records = prepared
        .iter()
        .map(|commit| {
            commit
                .preencoded_wal
                .as_deref()
                .expect("insert batch has preencoded WAL")
        })
        .collect::<Vec<_>>();
    let synced_bytes = records.iter().fold(0u64, |total, record| {
        total.saturating_add(record.len() as u64)
    });
    if let Err(error) = cs.wal().append_commits_unflushed(&records) {
        drop(cs);
        let message = error.to_string();
        return Ok(BatchOutcome {
            results: members
                .into_iter()
                .map(|member| {
                    (
                        member,
                        Err(Error::Io(std::io::Error::other(format!(
                            "coordinated WAL append failed: {message}"
                        )))),
                    )
                })
                .collect(),
            set_aside,
        });
    }

    shared
        .wal_appended_bytes
        .fetch_add(synced_bytes, AtomicOrdering::Relaxed);
    let wal_append_time = wal_append_started.elapsed();
    // See `finish_prepared_commit`: `Balanced` syncs on the timer thread.
    let sync_due = shared.opts.durability == Durability::Safe
        && cs.wal().sync_due(
            shared.opts.durability,
            shared.opts.balanced_sync_interval_ms,
        );
    let sync_outcome = sync_due.then(|| {
        let sync_started = Instant::now();
        let outcome = cs.wal().sync_data();
        let elapsed = sync_started.elapsed();
        record_wal_sync(shared, elapsed, batch_len, synced_bytes);
        (outcome, elapsed)
    });

    let apply_started = Instant::now();
    let mut added = 0u64;
    let mut obsolete_operations = 0u64;
    let mut obsolete_bytes = 0u64;
    let mut jobs: Vec<VecJob> = Vec::new();
    {
        let write_wait_started = Instant::now();
        let mut state = write_state_for_commit(shared);
        shared.commit_state_write_wait_nanos.fetch_add(
            elapsed_nanos(write_wait_started.elapsed()),
            AtomicOrdering::Relaxed,
        );
        for ((commit, version), commit_previous) in
            prepared.iter_mut().zip(&versions).zip(previous_entries)
        {
            let payload_arena = commit.payload_arena.clone();
            for (table, table_previous) in std::mem::take(&mut commit.staged)
                .into_iter()
                .zip(commit_previous)
            {
                let high_id = table
                    .changes
                    .last()
                    .map(|change| change.id.clone())
                    .expect("prepared table is non-empty");
                let keys = DerivedIndexKeys::new(&table.name, &table.schema);
                let changed_ids: Vec<String> = table
                    .changes
                    .iter()
                    .map(|change| change.id.clone())
                    .collect();
                for (change, (previous, prior_record)) in
                    table.changes.into_iter().zip(table_previous)
                {
                    count_obsolete(
                        &table.name,
                        &change,
                        previous.as_ref(),
                        &mut obsolete_operations,
                        &mut obsolete_bytes,
                    );
                    if let Some(VersionEntry {
                        kind: VKind::SegPut { segment, .. },
                        ..
                    }) = &previous
                    {
                        state.superseded_segments.insert(*segment);
                    }
                    let payload = change
                        .payload
                        .map(|range| MemPayload::from_range(payload_arena.clone(), range));
                    let put = change
                        .operation
                        .as_ref()
                        .zip(payload.as_ref())
                        .map(|(record, payload)| (payload, record));
                    apply_one_owned(
                        &mut state,
                        *version,
                        &table.schema,
                        &table.name,
                        change.id,
                        ApplyRecordState {
                            put,
                            prior: prior_record,
                        },
                        &keys,
                        &mut jobs,
                    );
                    added =
                        added.saturating_add(change.payload.map_or(0, |(_, len)| len as u64) + 32);
                }
                state.record_high_id(&table.name, &high_id);
                state.change_log.push(*version, &table.name, changed_ids);
            }
            state.committed_version = *version;
        }
    }
    // Only now, with the read view that covers the batch published by the
    // guard's release, do new snapshots move to it. Storing the version
    // under the guard handed readers a snapshot no view covered yet, and
    // they waited for the state lock behind the rest of the batch. The
    // commit mutex is still held, so compaction sees the two agree.
    shared
        .published_version
        .store(end_version, AtomicOrdering::Release);
    let apply_time = apply_started.elapsed();
    record_obsolete(shared, obsolete_operations, obsolete_bytes);
    send_vector_jobs(shared, jobs);
    cs.memtable_bytes = cs.memtable_bytes.saturating_add(added);
    shared.memory_governor.add_index_delta_bytes(incoming_bytes);
    let maintenance_needed = cs.memtable_bytes >= shared.opts.memtable_max_bytes;
    let derived_schedule_needed = derived_schedule_needed(shared, end_version);
    drop(cs);
    let wal_time = wal_started.elapsed();

    let maintenance_started = Instant::now();
    let result = post_commit_maintenance(
        shared,
        maintenance_needed,
        derived_schedule_needed,
        end_version,
    );
    if let Err(error) = result {
        let mut status = shared.background_checkpoint.lock().unwrap();
        status.commit_unknown = matches!(&error, Error::CommitUnknown(_));
        status.last_error = Some(format!("post-commit maintenance failed: {error}"));
    }
    let maintenance_wait_time = maintenance_started.elapsed();

    let elapsed_nanos = |duration: Duration| duration.as_nanos().min(u64::MAX as u128) as u64;
    let count = prepared.len() as u64;
    shared
        .commit_count
        .fetch_add(count, AtomicOrdering::Relaxed);
    let total_commit_nanos = prepared.iter().fold(0u64, |total, commit| {
        total.saturating_add(elapsed_nanos(commit.commit_started.elapsed()))
    });
    shared
        .commit_nanos
        .fetch_add(total_commit_nanos, AtomicOrdering::Relaxed);
    shared
        .commit_lock_wait_nanos
        .fetch_add(elapsed_nanos(lock_wait), AtomicOrdering::Relaxed);
    let outside_prepare = prepared
        .iter()
        .map(|commit| commit.outside_prepare_time)
        .fold(Duration::ZERO, Duration::saturating_add);
    let phase_prepare = prepared
        .iter()
        .map(|commit| commit.phase_prepare_time)
        .fold(Duration::ZERO, Duration::saturating_add);
    let record_encode = prepared
        .iter()
        .map(|commit| commit.record_encode_time)
        .fold(Duration::ZERO, Duration::saturating_add);
    let wal_encode = prepared
        .iter()
        .map(|commit| commit.wal_encode_time)
        .fold(version_encode_time, Duration::saturating_add);
    shared.commit_prepare_nanos.fetch_add(
        elapsed_nanos(outside_prepare.saturating_add(locked_prepare_time)),
        AtomicOrdering::Relaxed,
    );
    shared
        .commit_locked_prepare_nanos
        .fetch_add(elapsed_nanos(locked_prepare_time), AtomicOrdering::Relaxed);
    shared
        .commit_phase_prepare_nanos
        .fetch_add(elapsed_nanos(phase_prepare), AtomicOrdering::Relaxed);
    shared
        .commit_phase_record_encode_nanos
        .fetch_add(elapsed_nanos(record_encode), AtomicOrdering::Relaxed);
    shared
        .commit_phase_wal_encode_nanos
        .fetch_add(elapsed_nanos(wal_encode), AtomicOrdering::Relaxed);
    shared
        .commit_phase_validation_nanos
        .fetch_add(elapsed_nanos(locked_prepare_time), AtomicOrdering::Relaxed);
    shared.commit_wal_nanos.fetch_add(
        elapsed_nanos(wal_time).saturating_mul(count),
        AtomicOrdering::Relaxed,
    );
    shared
        .commit_wal_append_nanos
        .fetch_add(elapsed_nanos(wal_append_time), AtomicOrdering::Relaxed);
    shared.commit_phase_sync_wait_nanos.fetch_add(
        elapsed_nanos(
            sync_outcome
                .as_ref()
                .map_or(Duration::ZERO, |(_, elapsed)| *elapsed),
        ),
        AtomicOrdering::Relaxed,
    );
    shared
        .commit_apply_nanos
        .fetch_add(elapsed_nanos(apply_time), AtomicOrdering::Relaxed);
    shared.commit_phase_maintenance_wait_nanos.fetch_add(
        elapsed_nanos(maintenance_wait_time),
        AtomicOrdering::Relaxed,
    );
    shared
        .coordinated_batch_count
        .fetch_add(1, AtomicOrdering::Relaxed);
    shared
        .coordinated_commit_count
        .fetch_add(count, AtomicOrdering::Relaxed);

    let sync_error = match sync_outcome {
        Some((WalAppendOutcome::SyncFailed(error), _)) => {
            fence_after_wal_sync_failure(shared, &error);

            Some(error.to_string())
        }
        _ => None,
    };
    Ok(BatchOutcome {
        results: members
            .into_iter()
            .zip(versions)
            .map(|(member, version)| {
                (
                    member,
                    match &sync_error {
                        Some(error) => Err(Error::CommitUnknown(format!(
                            "version {version} was published, but syncing its coordinated WAL batch failed: {error}"
                        ))),
                        None => Ok(version),
                    },
                )
            })
            .collect(),
        set_aside,
    })
}

pub(super) fn finish_prepared_commit(
    shared: &Arc<Shared>,
    prepared: PreparedCommit,
) -> Result<u64> {
    let PreparedCommit {
        snap_version,
        mut staged,
        mut payload_arena,
        mut preencoded_wal,
        optimistic_prior_records,
        incoming_index_bytes,
        outside_prepare_time,
        phase_prepare_time,
        record_encode_time,
        mut wal_encode_time,
        commit_started,
        _pending_blobs,
    } = prepared;

    let mut lock_wait = Duration::ZERO;
    let mut locked_prepare_time = Duration::ZERO;
    let mut validation_time = Duration::ZERO;
    let mut maintenance_wait_time = Duration::ZERO;
    let mut rebased_rows;
    let (mut cs, previous_entries, commit_version, incoming_index_bytes, identity_high_water) = loop {
        let lock_started = Instant::now();
        shared.commit_waiters.fetch_add(1, AtomicOrdering::AcqRel);
        let mut cs = lock_commit_for_transaction(shared);
        shared.commit_waiters.fetch_sub(1, AtomicOrdering::AcqRel);
        lock_wait = lock_wait.saturating_add(lock_started.elapsed());
        // Surface asynchronous I/O failure before this transaction reaches
        // its WAL durability point; reporting it after apply would make the
        // commit outcome ambiguous to the caller.
        take_background_checkpoint_error(shared)?;
        let locked_prepare_started = Instant::now();
        let validation_started = Instant::now();
        let mut previous_entries: Vec<Vec<(Option<VersionEntry>, Option<Record>)>> =
            Vec::with_capacity(staged.len());
        // `(table, change, record)` for each delta update replayed over a
        // version committed after the snapshot.
        let mut rebased: Vec<(usize, usize, Record)> = Vec::new();
        let (commit_version, identity_high_water) = {
            let st = shared.state.read().unwrap();
            for (table_index, table) in staged.iter().enumerate() {
                if st.catalog.table(&table.name) != Some(&table.schema) {
                    return Err(Error::Conflict(format!(
                        "schema for {} changed while the transaction was preparing",
                        table.name
                    )));
                }
                let mut table_previous = Vec::with_capacity(table.changes.len());
                for (change_index, change) in table.changes.iter().enumerate() {
                    // Write-write conflict: someone committed a change to this
                    // record after our snapshot.
                    let previous = if st.id_is_above_high_watermark(&table.name, &change.id) {
                        None
                    } else {
                        st.latest_owned(&table.name, &change.id)?
                    };
                    if let Some(last) = &previous {
                        if last.version > snap_version {
                            match rebase_change(&st, &table.schema, change, last)? {
                                Some(record) => rebased.push((table_index, change_index, record)),
                                None => {
                                    return Err(Error::Conflict(format!(
                                        "{}/{} changed after this transaction began",
                                        table.name, change.id
                                    )))
                                }
                            }
                        }
                    }
                    // Complete every fallible read before the WAL durability
                    // point. Applying the already-committed change below must
                    // only mutate memory and cannot return a late error after
                    // partially publishing a transaction.
                    let prior_record = match &previous {
                        Some(entry)
                            if !entry.is_tombstone()
                                && (!table.schema.indexes.is_empty()
                                    || !table.schema.text_indexes.is_empty()) =>
                        {
                            match &optimistic_prior_records[table_index][change_index] {
                                Some((version, record)) if *version == entry.version => {
                                    Some(record.clone())
                                }
                                _ => Some(read_prior_for_indexes(
                                    &st.blobs,
                                    &st.readers,
                                    &entry.kind,
                                    &table.schema,
                                )?),
                            }
                        }
                        _ => None,
                    };
                    table_previous.push((previous, prior_record));
                }
                previous_entries.push(table_previous);
            }
            // A replayed row replaces the staged record and its payload, so
            // the frame encoded before the mutex no longer matches: it is
            // encoded again at the durability point. Uniqueness and foreign
            // keys below see the replayed records.
            rebased_rows = rebased.len() as u64;
            if !rebased.is_empty() {
                let arena = Arc::make_mut(&mut payload_arena);
                for (table_index, change_index, record) in rebased {
                    let table = &mut staged[table_index];
                    let start = u32::try_from(arena.len()).map_err(|_| {
                        Error::InvalidArgument(
                            "transaction payload arena exceeds the 4-GiB WAL limit".into(),
                        )
                    })?;
                    encode_record_ordered_into(arena, &table.schema, &record, None)?;
                    let end = u32::try_from(arena.len()).map_err(|_| {
                        Error::InvalidArgument(
                            "transaction payload arena exceeds the 4-GiB WAL limit".into(),
                        )
                    })?;
                    let change = &mut table.changes[change_index];
                    change.payload = Some((start, end - start));
                    change.operation = Some(record);
                }
                preencoded_wal = None;
            }
            let borrowed: Vec<&PreparedTable> = staged.iter().collect();
            validate_unique(&st, &borrowed)?;
            validate_foreign_keys(&st, &borrowed)?;
            let identity_high_water: Vec<(String, i64)> = staged
                .iter()
                .filter(|table| table.schema.columns.iter().any(|column| column.identity))
                .filter_map(|table| {
                    st.identity_high_water
                        .get(&table.name)
                        .map(|value| (table.name.clone(), value.load(AtomicOrdering::Relaxed)))
                })
                .collect();
            (st.committed_version + 1, identity_high_water)
        };
        validation_time = validation_time.saturating_add(validation_started.elapsed());
        locked_prepare_time = locked_prepare_time.saturating_add(locked_prepare_started.elapsed());
        if shared
            .memory_governor
            .index_would_exceed(incoming_index_bytes)
        {
            let maintenance_wait_started = Instant::now();
            if let Some(group) = cs.wal_sync_group.clone() {
                drop(cs);
                let _ = group.wait();
                maintenance_wait_time =
                    maintenance_wait_time.saturating_add(maintenance_wait_started.elapsed());
                continue;
            }
            let (derived_bytes, has_frozen_derived) = {
                let state = shared.state.read().unwrap();
                (
                    state.derived_delta_memory_bytes(),
                    state.has_frozen_derived(),
                )
            };
            let derived_running = shared.background_derived.lock().unwrap().running;
            if derived_running {
                drop(cs);
                wait_for_background_derived(shared)?;
                maintenance_wait_time =
                    maintenance_wait_time.saturating_add(maintenance_wait_started.elapsed());
                continue;
            }
            if derived_bytes > 0 || has_frozen_derived {
                wait_vector_indexing_shared(shared)?;
                let memory = acquire_frozen_lease(shared, derived_bytes);
                if schedule_frozen_derived(shared, memory)? {
                    drop(cs);
                    wait_for_background_derived(shared)?;
                    maintenance_wait_time =
                        maintenance_wait_time.saturating_add(maintenance_wait_started.elapsed());
                    continue;
                }
            }
            if shared.state.read().unwrap().index.frozen.is_some() {
                drop(cs);
                wait_for_background_checkpoint(shared)?;
                maintenance_wait_time =
                    maintenance_wait_time.saturating_add(maintenance_wait_started.elapsed());
                continue;
            }
            let memory = acquire_frozen_lease(shared, cs.memtable_bytes as usize);
            if schedule_frozen_checkpoint(shared, &mut cs, memory)? {
                drop(cs);
                wait_for_background_checkpoint(shared)?;
                maintenance_wait_time =
                    maintenance_wait_time.saturating_add(maintenance_wait_started.elapsed());
                continue;
            }
            if shared
                .memory_governor
                .index_would_exceed(incoming_index_bytes)
            {
                return Err(Error::MemoryLimit(
                    "index tombstones still fill the delta pool after consolidation; compact the database or raise memory.index_delta_pool_bytes"
                        .into(),
                ));
            }
            maintenance_wait_time =
                maintenance_wait_time.saturating_add(maintenance_wait_started.elapsed());
        }
        break (
            cs,
            previous_entries,
            commit_version,
            incoming_index_bytes,
            identity_high_water,
        );
    };
    let prepare_time = outside_prepare_time.saturating_add(locked_prepare_time);
    // Durability point: the WAL record is the commit.
    let wal_started = Instant::now();
    let wal_encode_started = Instant::now();
    let mut bytes = match preencoded_wal {
        Some(bytes) => bytes,
        None => {
            let wal_changes: Vec<(&str, &str, Option<&[u8]>)> = staged
                .iter()
                .flat_map(|table| {
                    table.changes.iter().map(|change| {
                        let payload = change.payload.map(|(start, len)| {
                            let start = start as usize;
                            &payload_arena[start..start + len as usize]
                        });
                        (table.name.as_str(), change.id.as_str(), payload)
                    })
                })
                .collect();
            let identity_wal: Vec<(&str, i64)> = identity_high_water
                .iter()
                .map(|(table, value)| (table.as_str(), *value))
                .collect();
            encode_commit(commit_version, &wal_changes, &identity_wal)?
        }
    };
    set_encoded_commit_version(&mut bytes, commit_version);
    wal_encode_time = wal_encode_time.saturating_add(wal_encode_started.elapsed());
    let wal_append_started = Instant::now();
    cs.wal().append_commit_unflushed(&bytes)?;
    if rebased_rows > 0 {
        shared
            .delta_rebased_rows
            .fetch_add(rebased_rows, AtomicOrdering::Relaxed);
    }
    shared
        .wal_appended_bytes
        .fetch_add(bytes.len() as u64, AtomicOrdering::Relaxed);
    let wal_append_time = wal_append_started.elapsed();
    // `Balanced` acknowledges before the disk: its barrier runs on the sync
    // timer thread, outside this mutex, so no commit ever queues behind an
    // fsync. Only `Safe` forms a sync group here.
    let sync_due = shared.opts.durability == Durability::Safe
        && cs.wal().sync_due(
            shared.opts.durability,
            shared.opts.balanced_sync_interval_ms,
        );
    let sync_group = if sync_due {
        match cs.wal_sync_group.as_ref() {
            Some(group) => {
                group.join(bytes.len() as u64);
                Some((group.clone(), false))
            }
            None => {
                let group = Arc::new(WalSyncGroup::new(bytes.len() as u64));
                cs.wal_sync_group = Some(group.clone());
                Some((group, true))
            }
        }
    } else {
        None
    };
    // Publish atomically to readers.
    let apply_started = Instant::now();
    let mut added = 0u64;
    let mut obsolete_operations = 0u64;
    let mut obsolete_bytes = 0u64;
    let mut jobs: Vec<VecJob> = Vec::new();
    {
        let write_wait_started = Instant::now();
        let mut st = write_state_for_commit(shared);
        shared.commit_state_write_wait_nanos.fetch_add(
            elapsed_nanos(write_wait_started.elapsed()),
            AtomicOrdering::Relaxed,
        );
        for (table, table_previous) in staged.into_iter().zip(previous_entries) {
            let high_id = table
                .changes
                .last()
                .map(|change| change.id.clone())
                .expect("prepared tables are non-empty");
            let keys = DerivedIndexKeys::new(&table.name, &table.schema);
            let changed_ids: Vec<String> = table
                .changes
                .iter()
                .map(|change| change.id.clone())
                .collect();
            for (change, (previous, prior_record)) in table.changes.into_iter().zip(table_previous)
            {
                count_obsolete(
                    &table.name,
                    &change,
                    previous.as_ref(),
                    &mut obsolete_operations,
                    &mut obsolete_bytes,
                );
                if let Some(VersionEntry {
                    kind: VKind::SegPut { segment, .. },
                    ..
                }) = &previous
                {
                    st.superseded_segments.insert(*segment);
                }
                let payload = change
                    .payload
                    .map(|range| MemPayload::from_range(payload_arena.clone(), range));
                let put = match (&change.operation, &payload) {
                    (Some(record), Some(payload)) => Some((payload, record)),
                    _ => None,
                };
                apply_one_owned(
                    &mut st,
                    commit_version,
                    &table.schema,
                    &table.name,
                    change.id,
                    ApplyRecordState {
                        put,
                        prior: prior_record,
                    },
                    &keys,
                    &mut jobs,
                );
                added += change.payload.map_or(0, |(_, len)| len as u64) + 32;
            }
            st.record_high_id(&table.name, &high_id);
            st.change_log.push(commit_version, &table.name, changed_ids);
        }
        st.committed_version = commit_version;
    }
    // After the read view is published; see the coordinated batch.
    shared
        .published_version
        .store(commit_version, AtomicOrdering::Release);

    let apply_time = apply_started.elapsed();
    record_obsolete(shared, obsolete_operations, obsolete_bytes);
    send_vector_jobs(shared, jobs);
    cs.memtable_bytes += added;
    shared
        .memory_governor
        .add_index_delta_bytes(incoming_index_bytes);
    let maintenance_needed = cs.memtable_bytes >= shared.opts.memtable_max_bytes;
    let derived_schedule_needed = derived_schedule_needed(shared, commit_version);
    drop(cs);

    let waited_for_sync = sync_group.is_some();
    let sync_wait_started = Instant::now();
    let wal_result = sync_group.map_or(Ok(()), |(group, leader)| {
        finish_or_wait_wal_sync(shared, group, leader)
    });
    let sync_wait_time = if waited_for_sync {
        sync_wait_started.elapsed()
    } else {
        Duration::ZERO
    };
    let wal_time = wal_started.elapsed();

    let maintenance_started = Instant::now();
    let maintenance_result = post_commit_maintenance(
        shared,
        maintenance_needed,
        derived_schedule_needed,
        commit_version,
    );
    maintenance_wait_time = maintenance_wait_time.saturating_add(maintenance_started.elapsed());
    if let Err(error) = maintenance_result {
        // The transaction is already in the WAL and visible. Never turn a
        // post-commit checkpoint failure into an apparent transaction
        // failure that callers might retry. Defer it to the next write before
        // that write reaches its own durability point.
        let mut status = shared.background_checkpoint.lock().unwrap();
        status.commit_unknown = matches!(&error, Error::CommitUnknown(_));
        status.last_error = Some(format!("post-commit maintenance failed: {error}"));
    }
    let elapsed_nanos = |duration: Duration| duration.as_nanos().min(u64::MAX as u128) as u64;
    shared.commit_count.fetch_add(1, AtomicOrdering::Relaxed);
    shared.commit_nanos.fetch_add(
        elapsed_nanos(commit_started.elapsed()),
        AtomicOrdering::Relaxed,
    );
    shared
        .commit_lock_wait_nanos
        .fetch_add(elapsed_nanos(lock_wait), AtomicOrdering::Relaxed);
    shared
        .commit_prepare_nanos
        .fetch_add(elapsed_nanos(prepare_time), AtomicOrdering::Relaxed);
    shared
        .commit_locked_prepare_nanos
        .fetch_add(elapsed_nanos(locked_prepare_time), AtomicOrdering::Relaxed);
    shared
        .commit_phase_prepare_nanos
        .fetch_add(elapsed_nanos(phase_prepare_time), AtomicOrdering::Relaxed);
    shared
        .commit_phase_record_encode_nanos
        .fetch_add(elapsed_nanos(record_encode_time), AtomicOrdering::Relaxed);
    shared
        .commit_phase_wal_encode_nanos
        .fetch_add(elapsed_nanos(wal_encode_time), AtomicOrdering::Relaxed);
    shared
        .commit_phase_validation_nanos
        .fetch_add(elapsed_nanos(validation_time), AtomicOrdering::Relaxed);
    shared
        .commit_wal_nanos
        .fetch_add(elapsed_nanos(wal_time), AtomicOrdering::Relaxed);
    shared
        .commit_wal_append_nanos
        .fetch_add(elapsed_nanos(wal_append_time), AtomicOrdering::Relaxed);
    shared
        .commit_phase_sync_wait_nanos
        .fetch_add(elapsed_nanos(sync_wait_time), AtomicOrdering::Relaxed);
    shared
        .commit_apply_nanos
        .fetch_add(elapsed_nanos(apply_time), AtomicOrdering::Relaxed);
    shared.commit_phase_maintenance_wait_nanos.fetch_add(
        elapsed_nanos(maintenance_wait_time),
        AtomicOrdering::Relaxed,
    );
    match wal_result {
        Ok(()) => Ok(commit_version),
        Err(error) => Err(Error::CommitUnknown(format!(
            "version {commit_version} was published, but syncing its WAL group failed: {error}"
        ))),
    }
}

/// Commits between two walks of the derived overlays.
///
/// Their total size is a walk of every posting they hold, and the commit path
/// consulted it on every commit to decide whether a background publication
/// was due. Past half the delta pool that walk grows with the overlays
/// themselves, so each commit paid for everything written since the last
/// publication: on the shop workload the write operations doubled in latency
/// between checkpoints and snapped back after each one. The decision is a
/// schedule, not an invariant, so it samples. Hard pool pressure still
/// measures exactly, and so does the code that actually schedules the job.
const DERIVED_BYTES_SAMPLE_COMMITS: u64 = 256;

fn sampled_derived_delta_bytes(shared: &Arc<Shared>, version: u64) -> usize {
    let sampled_at = shared
        .derived_delta_bytes_sampled_at
        .load(AtomicOrdering::Relaxed);
    if version < sampled_at.saturating_add(DERIVED_BYTES_SAMPLE_COMMITS) {
        return shared
            .derived_delta_bytes_sample
            .load(AtomicOrdering::Relaxed);
    }
    let bytes = shared.state.read().unwrap().derived_delta_memory_bytes();
    shared
        .derived_delta_size_walks
        .fetch_add(1, AtomicOrdering::Relaxed);
    shared
        .derived_delta_bytes_sample
        .store(bytes, AtomicOrdering::Relaxed);
    shared
        .derived_delta_bytes_sampled_at
        .store(version, AtomicOrdering::Relaxed);
    bytes
}

/// Forget the sampled overlay size: it describes overlays that are no longer
/// there.
fn reset_derived_delta_sample(shared: &Arc<Shared>, version: u64) {
    shared
        .derived_delta_bytes_sample
        .store(0, AtomicOrdering::Relaxed);
    shared
        .derived_delta_bytes_sampled_at
        .store(version, AtomicOrdering::Relaxed);
}

/// A change's delta updates replayed over `latest`, the version committed
/// after the transaction's snapshot; `None` when the conflict stands.
///
/// The conflict stands when the change has no delta updates, the row was
/// deleted, a statement's WHERE fails on the new version (the stock ran out)
/// or the arithmetic fails there (overflow): the retry then meets the new
/// version at statement time and reports the real outcome. Tables with blob
/// columns are left out because re-encoding under the commit mutex must not
/// write blob files.
fn rebase_change(
    st: &State,
    schema: &TableSchema,
    change: &PreparedChange,
    latest: &VersionEntry,
) -> Result<Option<Record>> {
    let Some(steps) = &change.rebase else {
        return Ok(None);
    };
    if latest.is_tombstone()
        || schema
            .columns
            .iter()
            .any(|column| column.ty == ColumnType::Blob)
    {
        return Ok(None);
    }
    let mut record = read_record_kind(&st.blobs, &st.readers, &latest.kind, schema)?;
    for step in steps {
        match step(&change.id, &record) {
            Ok(Some(next)) => record = next,
            Ok(None) | Err(_) => return Ok(None),
        }
    }
    Ok(Some(record))
}

pub(super) fn estimate_staged_index_bytes(
    shared: &Shared,
    st: &ReadView,
    staged: &[PreparedTable],
) -> Result<usize> {
    let mut bytes = 0usize;
    for table in staged {
        for change in &table.changes {
            bytes = bytes
                .saturating_add(change.id.len())
                .saturating_add(change.payload.map_or(0, |(_, len)| len as usize))
                // Matches PrimaryIdx::delta_memory_bytes (96 bytes for the
                // B-tree key/value slot plus 40 for VersionEntry/Arc), with a
                // small conservative margin. The outer table key is amortized
                // across all rows in the transaction.
                .saturating_add(144);
            // Updates can create both a tombstone for the old derived entry
            // and a new entry. Charge both sides conservatively before WAL.
            bytes = bytes.saturating_add(
                table
                    .schema
                    .indexes
                    .len()
                    .saturating_mul(change.id.len().saturating_mul(2).saturating_add(320)),
            );
            if table.schema.text_indexes.is_empty() && table.schema.vector_indexes.is_empty() {
                continue;
            }
            let mut charge_record = |record: &Record| {
                for def in &table.schema.text_indexes {
                    if let Some(Value::Text(text)) = record.get(&def.column) {
                        for token in crate::text::tokenize(text) {
                            bytes = bytes
                                .saturating_add(token.len())
                                .saturating_add(change.id.len())
                                .saturating_add(112);
                        }
                    }
                }
                for def in &table.schema.vector_indexes {
                    if let Some(Value::Vector(vector)) = record.get(&def.column) {
                        let scalar = if def.quantized { 1 } else { 4 };
                        bytes = bytes
                            .saturating_add(vector.len().saturating_mul(scalar))
                            .saturating_add(def.m.saturating_mul(16))
                            .saturating_add(change.id.len())
                            .saturating_add(192);
                    }
                }
            };
            if let Some(record) = &change.operation {
                charge_record(record);
            }
            if let Some(previous) = st.latest_owned(&table.name, &change.id)? {
                if !previous.is_tombstone() {
                    let record = read_record_kind(
                        &shared.blobs,
                        &st.readers,
                        &previous.kind,
                        &table.schema,
                    )?;
                    charge_record(&record);
                }
            }
        }
    }
    Ok(bytes)
}

pub(super) fn validate_unique(st: &State, staged: &[&PreparedTable]) -> Result<()> {
    if !st
        .catalog
        .tables
        .iter()
        .any(|schema| schema.indexes.iter().any(|index| index.unique))
    {
        return Ok(());
    }
    let mut staged_new: HashMap<(String, String, Vec<u8>), String> = HashMap::new();
    let staged_keys: HashSet<_> = staged
        .iter()
        .flat_map(|table| {
            table
                .changes
                .iter()
                .map(|change| (table.name.as_str(), change.id.as_str()))
        })
        .collect();
    for table in staged {
        for change in &table.changes {
            let Some(record) = &change.operation else {
                continue;
            };
            for def in &table.schema.indexes {
                if !def.unique {
                    continue;
                }
                if secondary_key_has_null(record, def.columns()) {
                    continue;
                }
                let key = secondary_tuple_key(record, def.columns());
                if let Some(previous) = staged_new.insert(
                    (
                        table.name.clone(),
                        secondary_index_id(def.columns()),
                        key.clone(),
                    ),
                    change.id.clone(),
                ) {
                    if previous != change.id {
                        return Err(Error::UniqueViolation {
                            table: table.name.clone(),
                            column: def.column.clone(),
                        });
                    }
                }
                // A declared unique index that the state does not hold can
                // never authorize a write: absence is corruption, not "no
                // holders".
                let index = st
                    .secondary
                    .get(&secondary_index_key(&table.name, def))
                    .ok_or_else(|| missing_unique_index(&table.name, &def.column))?;
                for holder in index.ids(&key)? {
                    // A holder also written by this transaction is judged
                    // by its staged value (covered by staged_new above).
                    if holder != change.id
                        && !staged_keys.contains(&(table.name.as_str(), holder.as_str()))
                    {
                        return Err(Error::UniqueViolation {
                            table: table.name.clone(),
                            column: def.column.clone(),
                        });
                    }
                }
            }
        }
    }
    Ok(())
}

pub(super) fn missing_unique_index(table: &str, column: &str) -> Error {
    Error::Corrupt(format!(
        "unique index {table}.{column} is declared by the catalog but not loaded; reopen the database to rebuild derived indexes"
    ))
}

pub(super) fn prepared_change<'a>(
    staged: &'a [&'a PreparedTable],
    table: &str,
    id: &str,
) -> Option<&'a PreparedChange> {
    let table = staged.iter().find(|candidate| candidate.name == table)?;
    table
        .changes
        .binary_search_by(|change| change.id.as_str().cmp(id))
        .ok()
        .map(|index| &table.changes[index])
}

/// Read one row from the state that would exist after `staged` is applied.
/// The commit mutex is held by the caller, so this view cannot move beneath
/// validation.
pub(super) fn final_record(
    st: &State,
    staged: &[&PreparedTable],
    table: &str,
    id: &str,
) -> Result<Option<Record>> {
    let schema = st
        .catalog
        .table(table)
        .ok_or_else(|| Error::TableNotFound(table.into()))?;
    if let Some(change) = prepared_change(staged, table, id) {
        return Ok(change.operation.as_ref().map(|record| {
            let mut record = record.clone();
            if schema.has_implicit_id() {
                record.insert(ID_COLUMN, Value::Text(id.to_owned()));
            }
            record
        }));
    }
    match st.latest_owned(table, id)? {
        Some(entry) if entry.version > schema.epoch && !entry.is_tombstone() => {
            let mut record = read_record_kind(&st.blobs, &st.readers, &entry.kind, schema)?;
            if schema.has_implicit_id() {
                record.insert(ID_COLUMN, Value::Text(id.to_owned()));
            }
            Ok(Some(record))
        }
        _ => Ok(None),
    }
}

pub(super) fn final_ids_matching(
    st: &State,
    staged: &[&PreparedTable],
    table: &str,
    column: &str,
    value: &Value,
) -> Result<BTreeSet<String>> {
    let mut candidates = BTreeSet::new();
    let schema = st
        .catalog
        .table(table)
        .ok_or_else(|| Error::TableNotFound(table.into()))?;
    if column == ID_COLUMN && schema.has_implicit_id() {
        if let Value::Text(id) = value {
            candidates.insert(id.clone());
        }
    } else if let Some(index) = st.secondary.get(&single_secondary_index_key(table, column)) {
        candidates.extend(index.ids(&index_key(value))?);
    } else {
        // Catalog validation normally guarantees an index for every FK side.
        // Keep the fallback for old catalogs and recovery tooling.
        st.index.visit_table(table, None, |id, _versions| {
            candidates.insert(id.to_owned());
            Ok(true)
        })?;
    }
    if let Some(prepared) = staged.iter().find(|candidate| candidate.name == table) {
        candidates.extend(prepared.changes.iter().map(|change| change.id.clone()));
    }
    let mut matching = BTreeSet::new();
    for id in candidates {
        if let Some(record) = final_record(st, staged, table, &id)? {
            if record.get(column) == Some(value) {
                matching.insert(id);
            }
        }
    }
    Ok(matching)
}

pub(super) fn validate_foreign_keys(st: &State, staged: &[&PreparedTable]) -> Result<()> {
    if !st
        .catalog
        .tables
        .iter()
        .any(|schema| !schema.foreign_keys.is_empty())
    {
        return Ok(());
    }

    // Every new final child value must resolve to a parent in the same final
    // view. This permits parent+child insertion in one transaction.
    for table in staged {
        for change in &table.changes {
            let Some(record) = &change.operation else {
                continue;
            };
            for foreign_key in &table.schema.foreign_keys {
                let Some(value) = record.get(&foreign_key.column) else {
                    continue;
                };
                if value.is_null() {
                    continue;
                }
                if final_ids_matching(
                    st,
                    staged,
                    &foreign_key.referenced_table,
                    &foreign_key.referenced_column,
                    value,
                )?
                .is_empty()
                {
                    return Err(Error::SchemaViolation(format!(
                        "foreign key violation: {}.{} has no matching {}.{}",
                        table.name,
                        foreign_key.column,
                        foreign_key.referenced_table,
                        foreign_key.referenced_column
                    )));
                }
            }
        }
    }

    // Removing or replacing a referenced key is legal only if the final view
    // still has a parent for it or has no children. Check PUTs too: a staged
    // DELETE+INSERT collapses to a PUT, and new FK dependencies may have been
    // added after an UPDATE was prepared. A CASCADE miss means a child committed
    // after the deleting transaction's snapshot; return Conflict so the retry can
    // rescan and delete it atomically.
    for parent in staged {
        if !st.catalog.tables.iter().any(|child| {
            child
                .foreign_keys
                .iter()
                .any(|foreign_key| foreign_key.referenced_table == parent.name)
        }) {
            continue;
        }
        for change in &parent.changes {
            let Some(previous) = st.latest_owned(&parent.name, &change.id)? else {
                continue;
            };
            if previous.is_tombstone() || previous.version <= parent.schema.epoch {
                continue;
            }
            let mut old_record =
                read_record_kind(&st.blobs, &st.readers, &previous.kind, &parent.schema)?;
            if parent.schema.has_implicit_id() {
                old_record.insert(ID_COLUMN, Value::Text(change.id.clone()));
            }
            for child_schema in &st.catalog.tables {
                for foreign_key in child_schema
                    .foreign_keys
                    .iter()
                    .filter(|foreign_key| foreign_key.referenced_table == parent.name)
                {
                    let Some(old_value) = old_record.get(&foreign_key.referenced_column) else {
                        continue;
                    };
                    if change.operation.as_ref().is_some_and(|record| {
                        (parent.schema.has_implicit_id()
                            && foreign_key.referenced_column == ID_COLUMN)
                            || record.get(&foreign_key.referenced_column) == Some(old_value)
                    }) {
                        continue;
                    }
                    if old_value.is_null()
                        || !final_ids_matching(
                            st,
                            staged,
                            &parent.name,
                            &foreign_key.referenced_column,
                            old_value,
                        )?
                        .is_empty()
                    {
                        continue;
                    }
                    let children = final_ids_matching(
                        st,
                        staged,
                        &child_schema.name,
                        &foreign_key.column,
                        old_value,
                    )?;
                    if children.is_empty() {
                        continue;
                    }
                    if change.operation.is_some() {
                        return Err(Error::SchemaViolation(format!(
                            "cannot replace referenced key {}.{}: referenced by {}.{}; ON UPDATE is not supported",
                            parent.name, foreign_key.referenced_column,
                            child_schema.name, foreign_key.column,
                        )));
                    }
                    match foreign_key.on_delete {
                        ReferentialAction::Restrict => {
                            return Err(Error::SchemaViolation(format!(
                                "cannot delete {}.{}: referenced by {}.{}",
                                parent.name, change.id, child_schema.name, foreign_key.column
                            )))
                        }
                        ReferentialAction::Cascade => {
                            return Err(Error::Conflict(format!(
                                "cascade for {}/{} must be retried after concurrent child change",
                                parent.name, change.id
                            )))
                        }
                    }
                }
            }
        }
    }
    Ok(())
}
