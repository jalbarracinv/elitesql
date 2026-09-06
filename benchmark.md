# EliteSQL current benchmarks

This document publishes the complete current acceptance run collected on
2026-09-05, plus clearly labeled historical and focused diagnostic results.
The current run uses the 384 MiB default and covers scale, bulk load, all three
durability modes, concurrent reads/writes, mutation/index contention, SQL and
synthetic ANN. Older artifacts are retained for comparison rather than
overwritten.

The results are candid. The primary directory publishes immutable deltas and
promotes groups of sixteen same-level runs in the background; equality and
BM25 retain fanout eight. Non-overlapping V2 primary runs are promoted by
copying already checksummed pages without decoding and rebuilding every entry.
The implementation also has a direct sorted bulk loader, streaming
unindexed equality scans, compact mmap page directories, transaction-local
table interning, allocation-light checkpoint snapshots, mapped segment payload
reads, vectorized HNSW distance kernels with neighbour prefetching, durable
HNSW runs merged in the background, and a budgeted hash GROUP BY. In the
current 10M transactional workload EliteSQL's ingest wall is 1.231x SQLite's,
while its total load (7.363 s) beat SQLite's (7.693 s) because SQLite's
deferred checkpoint took 1.838 s; the direct sorted bulk path was 1.554x
faster. Current Fast/Balanced throughput beat SQLite at every measured writer
count. Safe is compared separately using the same macOS `F_FULLFSYNC`
primitive on both engines.

Post-reference architectural change (updated 2026-08-23): automatic and
explicit checkpoints freeze one bounded memtable generation and flush it on a
dedicated worker while later commits fill a fresh active generation. The
frozen heap is charged to the maintenance pool. Two durable successor WALs
bridge the unlocked publication window: recovery can follow the old manifest,
the atomic copied tail and the active WAL, or the new manifest and its
successors. Segment, WAL-copy and manifest I/O therefore happen without the
global commit mutex; `checkpoint()` remains an end-to-end barrier for the
generation it freezes.

## Reference environment and source state

- MacBook Air, Apple M5, 10 cores (4 performance + 6 efficiency)
- 16 GiB RAM
- macOS 26.6.2 (25G83), arm64
- Rust/Cargo 1.93.1, release benchmark profile (fat LTO, one codegen unit)
- SQLite 3.45.0 through `rusqlite`'s bundled build
- EliteSQL 0.0.1 dirty worktree on top of commit `849190f`
- The 2026-09-05 run was on battery power with low power mode off and no
  thermal CPU limit recorded; the 2026-09-04 run was on AC power

The worktree contains the changes being measured; the commit hash alone is not
sufficient to reproduce these numbers until those changes are committed.
Benchmarks ran sequentially on the same machine. No benchmark ran in parallel
with another measurement.

## Hot-path optimizations — 2026-09-05

A second targeted pass, measured as A/B runs against the committed tree at
`849190f` on the same Apple M5 laptop (10 cores, 16 GiB, macOS 26.6.2, Rust
1.93.1). Every pair used the same harness in the same session and never
overlapped another measurement; where a first sample was taken while a build
was running it was discarded and repeated on an idle machine. Durability
semantics, the memory budget contracts and every existing on-disk format are
unchanged. One new disposable file kind is added under `vectors/` (see
[`docs/disk-format.md`](docs/disk-format.md)).

What changed:

1. **The persisted HNSW graph is now actually persisted.** The 2026-09-04
   "open with persisted graph" number was a full rebuild: the background
   publisher unlinked every immutable HNSW run after mapping it, so a
   database whose vector index had been flushed even once while running kept
   nothing on disk at close, and the next open re-inserted all 100K vectors
   (10.8 s). Runs now stay on disk and a small run manifest
   (`<stem>.vidx.runs`, published under the commit mutex) records which runs
   cover which commit generation. Open maps the run set, supersedes ids that
   newer runs re-indexed, replays only the commits after the manifest
   generation, and removes records that lost their vector between runs. A
   stale manifest (crash before the clean-close publication) is a valid
   prefix and is caught up; a missing or corrupt file falls back to the
   previous behavior. Four new persistence tests cover background runs,
   deletes/updates between runs, stale and broken manifests and index drops.
2. HNSW beam search prefetches the vectors of every unvisited neighbour in a
   list before computing any distance (`PRFM` on arm64, `PREFETCHT0` on
   x86_64), for the resident and the mapped graph. Candidates are still
   evaluated in the original order, so results are bit-identical; only the
   cache misses overlap.
3. SQL predicate evaluation borrows operands instead of cloning every column
   value and literal per row; the indexed nested-loop join lends the outer
   record to the joined row instead of deep-cloning both records per output
   row; single-table scans reuse one row buffer; and a bounded query whose
   access path already enforces its predicates (`LIMIT n` with no residual
   filter, or an equality driver with no other conjunct) trims its last scan
   batch to the rows still needed instead of decoding a full 512-row batch.
4. Point reads release the shared state lock before decoding the record
   (the batched scan path already did), so page faults and record
   materialization no longer hold up committers waiting for the write lock.
5. The per-record commit apply loop no longer allocates two strings per
   derived index per record to probe the index maps, and synchronous vector
   indexing borrows the staged vector instead of cloning it.
6. The release profile enables fat LTO with one codegen unit. A clean
   benchmark build takes about 75 s instead of 30 s; measured alone it was
   worth 2-7% on the SQL and point-read paths and neutral for ANN.
7. **Background merges bound the number of HNSW runs.** Every publication
   used to add one graph that each search had to visit. The maintenance
   worker now size-tiers the durable runs by live vectors and, whenever four
   or more comparable runs exist and the rebuilt graph fits the maintenance
   pool, re-inserts their live vectors into one new run: planned under the
   state lock, built from the run files without any lock, published under
   the commit mutex with ids removed meanwhile filtered out, old runs
   unlinked, manifest republished. Along the way the run metadata (node
   offsets, level tables, id maps, about 120 bytes per vector) stopped being
   charged to the index-delta pool: the base graph's never was, and charging
   the other runs meant a small pool was declared exhausted after four or
   five publications ("index tombstones still fill the delta pool") even
   though nothing could be consolidated. It is reported instead
   (`MaintenanceStats::vector_run_metadata_bytes`, plus `vector_runs`,
   `vector_run_merges` and `Db::wait_for_vector_run_merge`).
   The first version reserved the whole maintenance pool while it rebuilt,
   which stalled commits (see "Deciding how a merge shares the maintenance
   pool" below); the shipped version reserves only its estimated footprint,
   capped at half the pool, and frozen heaps are accounted without waiting.
8. **GROUP BY hashes before it sorts.** The single-table aggregate path
   externally sorted every input row by its encoded group key, whatever the
   number of groups. It now aggregates into a hash table while the estimated
   group table (keys, group values, aggregate states including DISTINCT sets)
   fits the per-query budget, and only when the budget is exceeded re-scans
   the same snapshot with the bounded sort-merge. Groups keep first-seen order
   and their first row's sequence, so both strategies produce identical
   output; a test runs the same queries under a 16 MiB and an 8 KiB budget
   and compares them row for row.

SQL over 1M rows (`sql.rs`, Criterion central estimates):

| Query | Before | After | Change |
|---|---:|---:|---:|
| Unique-index point lookup, literal | 4.476 µs | 4.132 µs | -7.7% |
| Unique-index point lookup, bound values | 3.654 µs | 3.431 µs | -6.1% |
| Indexed join, ~100 matching orders, top 10 | 188.06 µs | 154.09 µs | -18.1% |
| Indexed join, bound values | 186.07 µs | 154.90 µs | -16.8% |
| Unindexed 1M-row filter, `LIMIT 5` | 33.418 ms | 0.308 ms | -99.1% |

The last row is the batch trimming in change 3: the equality driver used to
collect 512 matching rows (about half the table for a 1-in-997 predicate)
before the executor kept five. The result set is the same five rows in the
same order.

GROUP BY over the same 1M orders (change 8; "before" is the committed tree
with the two new benchmark functions copied in):

| Query | Before | After | Change |
|---|---:|---:|---:|
| `GROUP BY amount`, 997 groups, `count(*)` | 845.1 ms | 313.7 ms | -62.9% |
| `GROUP BY user_id`, 10K groups, `count/sum/max` | 940.5 ms | 342.5 ms | -63.6% |

Point reads (`vs_sqlite.rs`, 10K resident rows) improved from 355.1 ns to
324.5 ns; SQLite measured 1.38-1.44 µs in the same runs. The 1M-row scale
harness (`scale_vs_sqlite.rs`, EliteSQL only, `fast`) moved from 0.748 s to
0.711 s ingest wall, 0.900 s to 0.812 s total load, 1.400 µs to 1.243 µs per
warm point read and 24 ms to 18 ms for the unindexed scan.

Synthetic ANN (`vector.rs`, 100K vectors, dimension 64, default construction):

| Metric | Before | After | Change |
|---|---:|---:|---:|
| Indexed ingest | 9.066 s | 8.856 s | -2.3% |
| Open with persisted graph | 10.787 s | 0.061 s | -99.4% |
| Mean search, `ef_search` 64 | 243.7 µs | 172.1 µs | -29.4% |
| Mean search, `ef_search` 128 | 419.7 µs | 298.2 µs | -28.9% |
| Mean search, `ef_search` 256 | 780.4 µs | 546.2 µs | -30.0% |
| Mean search, `ef_search` 512 | 1375.5 µs | 1007.0 µs | -26.8% |

Recall@10 was identical before and after at every `ef_search`
(0.9520/0.9940/0.9980/1.0000). The open time is a real load now: two
immutable runs (39.5 MB) are mapped and checksummed, no vector is
re-inserted, and the `vectors/` directory keeps them across restarts.

Run merging (change 7) was measured with a throwaway probe that ingests 40K
clustered 64-dimensional vectors in 1,000-row transactions under a reduced
`index_delta_pool_bytes`, so the overlay is published many times, waits three
seconds and then times 2,000 searches at `ef_search` 128:

| Pool | Before: publications, runs, mean search | After: publications, runs, mean search |
|---|---|---|
| 64 MiB | 1, 2 runs, 214.6 µs | 1, 2 runs, 212.9 µs |
| 16 MiB | 13, 13 runs, 1035.9 µs | 12, 3 runs, 373.5 µs |
| 4 MiB | fails: "index tombstones still fill the delta pool" | 25, 7 runs, 536.1 µs |

With one publication nothing changes. With thirteen, the unmerged tree
searches every run and takes 2.8x longer per query than the merged tree; with
a 4 MiB pool the committed tree cannot finish the ingest at all because the
run metadata alone exhausts its delta pool. Merging is not free: at 16 MiB
the ingest wall rose from 2.54 s to 5.12 s because a merge holds the
maintenance pool while it rebuilds, and publications (which commits wait for
when the delta pool is full) queue behind it. With the 128 MiB default the
100K-vector workload publishes once and never merges.

Concurrent readers with writers (`concurrent_rw.rs`, 16 readers, 4 writers,
three repetitions, compared with the published 2026-09-04 run of the same
configuration): 2.57-2.63M reads/s versus 2.53M, 103-105K inserted rows/s
versus 101K, and a writer p99 of 354-366 µs versus 487 µs.

### Deciding how a merge shares the maintenance pool

The first merge implementation reserved the whole maintenance pool for the
duration of a rebuild, like every other maintenance task. Two experiments were
run before deciding whether that was acceptable: a sustained ingest of 1M
clustered 64-dimensional vectors in 1,000-row transactions with per-commit
latency, run counts and RSS sampled every 250 ms, at the default profile and
with the index-delta pool reduced to 32 MiB; and a measurement of the resident
bytes of a rebuilt graph against the on-disk bytes per node the planner
estimates from.

What the sustained ingest showed with whole-pool merges:

| Profile | Publications | Merges (CPU) | Runs at the end | Commit p50 / p99 outside merges | Commits over 500 ms | Worst commit |
|---|---:|---:|---:|---:|---:|---:|
| 128/128 MiB (default) | 52 | 13 (90.5 s) | 13 | 93 / 145 ms | 7 | 19.5 s |
| 32/128 MiB | 195 | 60 (177.9 s) | 15 | 88 / 111 ms | 35 | 21.9 s |

Publications are far more frequent than the vector delta alone would cause:
the index-delta pool is shared with the primary memtable, so at the default
profile the HNSW overlay is published every ~19K vectors and merging is what
keeps a 1M index at 13 runs instead of 52. The stalls had one cause. The
commit path schedules a checkpoint or a publication by reserving the whole
maintenance pool, in two places while holding the commit mutex, and a merge
held that pool for ten to twenty seconds. Reserving less memory for the merge
alone would not have helped, because the schedulers asked for all of it.

The footprint measurement (debug build, so only the ratios matter): a
rebuilt 64-dimensional f32 graph occupies 442 resident bytes per vector
against 396 on disk, int8 254 against 208, and 384-dimensional f32 1,721
against 1,676. The planner's 3/2 factor overestimated by 23-46%; 5/4 stays
above the true footprint in every case and is what ships.

Decision, implemented the same day:

- A frozen heap scheduled from the commit path (a primary generation for a
  checkpoint, derived deltas for a publication) is memory that already exists
  and is only being reclassified from the index-delta pool. It is now
  accounted to the maintenance pool at its own size without waiting for
  capacity; blocking could only delay the consolidation that frees it.
- The mutual exclusion between checkpoints, publications and explicit
  maintenance, which the whole-pool reservation provided implicitly (and
  which existing tests assert), is now an explicit serial lease taken at the
  same points and held for the same duration. Nothing that was serialized
  before overlaps now.
- A merge reserves only its estimated footprint, blocking, capped at half the
  pool, and never takes the serial lease. Commits therefore never wait for a
  merge; explicit maintenance waits at most one merge. A merge whose graph
  outgrows 3/2 of its estimate is abandoned and its runs are not selected
  again in that process, so a mis-estimate costs one rebuild.
- The rebuild ceiling itself stays: a merge can only produce a graph that
  fits half the maintenance pool (about 135K 64-dimensional vectors at the
  default), so a very large index keeps several runs per size tier and
  `maintenance_pool_bytes` is the lever. Reading vectors from the source
  mappings instead of copying them would roughly double that reach and was
  deferred; `compact()`, which rebuilds the whole graph resident, cannot
  rebuild a 1M-vector index inside the default pool either.
- One consequence needed a second decision. Once publications stopped
  waiting for the pool, the soft trigger (publish derived deltas whenever the
  shared index-delta pool is half full) fired on nearly every commit while the
  primary memtable was what filled the pool: 259 publications of about 4K
  vectors for 1M rows, 97 runs at the end of the ingest, and the merger
  needing eleven more seconds to fold them to 19. A background publication
  now also requires derived deltas of at least one sixteenth of the pool
  (about 19K 64-dimensional vectors at the default), the largest run size four
  of which one merge can still rebuild. Hard pool pressure still publishes
  whatever exists.

The same sustained ingest after both changes:

| Profile | Publications | Merges (CPU) | Runs at the end | Commit p50 / p99 | Commits over 500 ms | Worst commit | Ingest wall | Search, mean at `ef_search` 128 |
|---|---:|---:|---:|---:|---:|---:|---:|---:|
| 128/128 MiB (default) | 34 | 6 (68.0 s) | 16 | 103 / 154 ms | 0 | 166 ms | 107.9 s (was 147.1 s) | 4.26 ms with 16 runs (was 3.01 ms with 14) |
| 32/128 MiB | 195 | 33 (136.3 s) | 16 | 95 / 140 ms | 0 | 147 ms | 92.4 s (was 216.8 s) | 4.30 ms with 16 runs (was 3.46 ms with 15) |

The stalls are gone in both profiles and the ingest wall fell by 27% and
57%. The run count settled at 16 rather than 13: with 34 publications of
37K-98K vectors, four comparable runs no longer fit the 64 MiB merge budget
(the planner trimmed each group below the fanout), so only the six merges of
smaller runs ran, and searches over 16 runs cost about 40% more than over 13.
That is the rebuild ceiling in practice; the choice is explicit: a bounded,
never-blocking background merge over a lower run count. Raising
`maintenance_pool_bytes`, or the deferred cheaper merge, moves the ceiling.

Later the same day, on the same machine, the complete workspace suite grew to
375 tests with the merge and GROUP BY work (a size-tiered selection unit
test, a unit test that deletes and updates records between a merge's plan
and its publication and checks the published run, an integration test that
drives background merges through a small pool and reopens the merged set, and
three GROUP BY tests including the hash/sort equivalence).

The 10M transactional load was repeated three times per tree on an idle
machine. Ingest wall measured 7.10/7.03/10.07 s before and 6.85/7.45/9.56 s
after; warm point reads 5.34/3.10/2.36 µs before and 1.66/3.58/2.42 µs after.
Back-to-back 10M runs write 2.2 GB each and the point-read phase overlaps the
background level promotion in some runs and not in others, so the spread
between repetitions exceeds any difference between the trees; none of the
changes above target that path, and this pass claims no 10M improvement.

## Hot-path optimizations — 2026-09-04

A targeted pass over the read, ANN and ingest hot paths, measured as A/B runs
against the committed tree at `fcdcde7` on an Apple M5 laptop (10 cores,
16 GiB, macOS 26.6.2, Rust 1.93.1). Each pair of runs used the same harness in
the same session and never overlapped another measurement. This is not a full
acceptance rerun: durability semantics, on-disk formats and the memory budget
contracts are unchanged, and the tables below replace nothing in the sections
that follow.

What changed:

1. Segment payload reads go through one lazily created read-only `mmap` per
   immutable segment instead of one `pread` system call plus a heap copy per
   record. The streaming equality scan keeps its own private sequential
   mapping per pass: an earlier attempt to share the point-read mapping doubled
   the 10M full scan and would have kept multi-gigabyte scans resident.
2. Unindexed equality scans that walk the primary directory (`find_eq_batch`,
   the SQL `WHERE col = v` path without an index) evaluate the predicate on the
   encoded payload and decode only matching records.
3. The primary-directory merge (`visit_table`) reuses its id and version
   buffers, so a steady scan performs no heap allocation per visited record.
4. HNSW distance kernels use lane-parallel accumulators that the compiler can
   vectorize (`distance.rs`), the per-search visited set is a reusable bitmap
   instead of a hash set, and vector-index memory accounting is incremental.
   The previous accounting walked every adjacency list after each insert,
   which made index builds and catch-up on open quadratic in the node count.
5. Record encoding reserves the exact payload size up front instead of
   growing the buffer through repeated reallocation.

SQL over 1M rows (`sql.rs`, Criterion central estimates; "before" is the
committed tree measured in the same session):

| Query | Before | After | Change |
|---|---:|---:|---:|
| Unique-index point lookup, bound values | 4.003 µs | 3.701 µs | -7.5% |
| Indexed join, ~100 matching orders, top 10 | 243.87 µs | 187.80 µs | -23.0% |
| Indexed join, bound values | 240.05 µs | 187.38 µs | -21.9% |
| Unindexed 1M-row filter, `LIMIT 5` | 294.77 ms | 33.82 ms | -88.5% |

Synthetic ANN (`vector.rs`, 100K vectors, dimension 64, default construction):

| Metric | Before | After |
|---|---:|---:|
| Indexed ingest | 15.17 s | 9.38 s |
| Open with persisted graph | 22.97 s | 10.49 s |
| Mean search, `ef_search` 64 | 311.3 µs | 234.4 µs |
| Mean search, `ef_search` 128 | 586.8 µs | 402.3 µs |
| Mean search, `ef_search` 256 | 1037.4 µs | 701.5 µs |
| Mean search, `ef_search` 512 | 1809.4 µs | 1327.8 µs |

Recall@10 was identical before and after at every `ef_search`
(0.9520/0.9940/0.9980/1.0000). The lane-parallel kernels change the floating
point summation order by a few ulps; one persistence test that compared two
vectors exactly parallel to its query now asserts membership instead of a
rounding-dependent tie order.

Transactional ingest (`scale_vs_sqlite.rs`, 4M rows, `fast`, 10K-row
transactions, EliteSQL only, median of three runs). This pair isolates change
5 above: "before" already includes changes 1-4.

| Phase | Before | After | Change |
|---|---:|---:|---:|
| Commit prepare | 0.690 s | 0.486 s | -29.6% |
| Commit total | 1.647 s | 1.452 s | -11.8% |
| Ingest wall | 3.053 s | 2.799 s | -8.3% |

The complete `elitesql-core` suite (327 tests) passes after these changes. One
timing fixture (`a_large_scan_yields_the_state_lock_to_a_concurrent_writer`)
scans 50K rows instead of 20K and bounds its sleep by the measured baseline,
because the mapped segment reads made the original 20K-row scan finish before
the concurrent commit it was meant to overlap.

## Full acceptance rerun — 2026-09-05

A complete local rerun was performed after the 2026-09-05 passes described
above, using the same harnesses, parameters and sequential procedure as the two
previous runs; it took twelve minutes and no step recorded a thermal CPU
limit. The immutable environment, result summary and artifact inventory are in
[`current-acceptance-2026-09-05.md`](benchmark-results/current-acceptance-2026-09-05.md);
all raw files use the `current-2026-09-05-*` prefix. Earlier artifacts remain
in place for comparison.

Headline results from the current run:

- At 10M rows, the transactional path completed in 7.363 s versus SQLite's
  7.693 s. EliteSQL's ingest wall (7.207 s) is 1.231x SQLite's (5.855 s); the
  total flips because SQLite's deferred checkpoint took 1.838 s while
  EliteSQL's final checkpoint took 0.155 s. Direct sorted bulk completed in
  4.388 s versus SQLite's 6.821 s (1.554x faster). Warm 10M point reads
  measured 2.5 us versus 34.3 us; the EliteSQL figure varies with the timing
  of the background level promotion (31.0 us on 2026-09-04).
- Fast and Balanced EliteSQL throughput beat SQLite at every writer count, by
  1.77-6.06x and 1.68-4.90x. Safe strict reached 1.00x/1.16x/3.79x/7.20x/13.47x
  at 1/2/4/8/16 writers with 1.00/1.09/4.00/7.98/15.87 commits per sync; the
  two-writer grouping (1.64 on 2026-09-04) remains the noisiest point.
- The 16-reader/four-writer run measured 2,620,233 reads/s, 104,809 inserted
  rows/s and 0.342 ms writer p99 (2,526,321 / 101,053 / 0.487 ms before).
  Read-only throughput peaked at 3,548,239 reads/s with 16 readers.
- Warm writer p99 across insert/update/delete/identity/FK/derived profiles was
  0.390/0.961/1.461/0.806/0.684/1.490 ms, with read throughput up in every
  profile but foreign key; the delete value sits inside that profile's own
  warm/reopened spread (0.937 ms reopened).
- ANN recall@10 was 0.952/0.994/0.998/1.000 at requested `ef_search`
  64/128/256/512, identical to both previous runs. Mean search was
  0.210/0.315/0.591/1.084 ms (11-24% faster). Indexed ingestion took 8.830 s
  and opening the persisted graph took 74.5 ms instead of 10.937 s, because the
  published runs now survive the close.
- SQL over 1M rows: unique-index point lookup 4.06/3.45 us (literal/bound),
  indexed join 155.6/153.0 us, unindexed filter with `LIMIT 5` 0.309 ms
  (34.23 ms before), and the new `GROUP BY` benchmarks 310.3 ms (997 groups)
  and 340.5 ms (10K groups).
- The fixed 1M-row small-transaction workload measured 0.855 s total load
  against SQLite's 0.745 s (1.148x SQLite time; 1.170x on 2026-09-04).

## Previous acceptance rerun — 2026-09-04

A complete local rerun was performed on AC power after the hot-path
optimizations of that date, using the same harnesses, parameters and
sequential procedure as the 2026-08-23 run. The immutable environment, result
summary and artifact inventory are in
[`current-acceptance-2026-09-04.md`](benchmark-results/current-acceptance-2026-09-04.md);
all raw files use the `current-2026-09-04-*` prefix. The 2026-08-23 artifacts
remain in place for comparison.

Headline results from that run (superseded by the 2026-09-05 section above):

- At 10M rows, the transactional path completed in 7.330 s versus SQLite's
  8.631 s. EliteSQL's ingest wall (7.220 s) is still 1.202x SQLite's (6.005 s);
  the total flips because SQLite's deferred checkpoint took 2.626 s in this
  run (1.611 s on 2026-08-23) while EliteSQL's final checkpoint took 0.111 s.
  Direct sorted bulk completed in 4.559 s versus SQLite's 7.289 s (1.599x
  faster). Warm 10M point reads measured 31.0 us versus 52.3 us.
- Fast and Balanced EliteSQL throughput beat SQLite at every writer count, by
  1.82-5.06x and 1.45-4.48x, within the spread of the previous matrix. Safe
  strict reached 1.03x/1.52x/3.61x/7.21x/12.75x at 1/2/4/8/16 writers with
  1.00/1.64/4.00/8.00/15.75 commits per sync.
- The 16-reader/four-writer run measured 2,526,321 reads/s, 101,053 inserted
  rows/s and 0.487 ms writer p99. Read-only throughput peaked at 3,583,376
  reads/s with 16 readers (1,216,481 with four readers was the previous peak).
- Warm writer p99 across insert/update/delete/identity/FK/derived profiles was
  0.420/1.085/0.954/0.963/0.812/1.514 ms, with read throughput roughly doubled
  in every profile. Every run validated final data and, where applicable,
  derived-index queries.
- ANN recall@10 was 0.952/0.994/0.998/1.000 at requested `ef_search`
  64/128/256/512, identical to the previous run. Mean search was
  0.237/0.416/0.773/1.353 ms. Indexed ingestion took 9.198 s and persisted
  open took 10.937 s.
- SQL over 1M rows: unique-index point lookup 4.36/3.66 us (literal/bound),
  indexed join 187.1/184.7 us, unindexed filter 34.23 ms.

## Previous acceptance rerun — 2026-08-23

A complete local rerun was performed on AC power after the writer and ANN work
of that date. It covers every Cargo benchmark in `elitesql-core`: 1M/10M scale
versus SQLite with the 384 MiB default, direct bulk load, the SQLite
microbenchmark, SQL over 1M rows, writers 1/2/4/8/16 under all three durability
modes, the complete 5x3 reader/writer matrix, all six mutation/index profiles
in warm/reopened modes, and synthetic ANN over 100K vectors. Benchmarks ran
sequentially; none overlapped another measurement.

The immutable environment, result summary, limitations and artifact inventory
are in
[`current-acceptance-2026-08-23.md`](benchmark-results/current-acceptance-2026-08-23.md).
All raw current-run files use the `current-2026-08-23-*` prefix. Older CSVs and
historical tables below remain available for longitudinal comparison.

Headline results from that run (superseded by the sections above):

- At 10M rows, the transactional path completed in 8.623 s versus SQLite's
  7.615 s (1.132x SQLite time). Direct sorted bulk completed in 5.190 s versus
  SQLite's 8.513 s (1.640x faster).
- Fast and Balanced EliteSQL throughput beat SQLite at every writer count, by
  1.85-5.08x and 1.49-4.29x. Safe uses a like-for-like strict comparison:
  EliteSQL and SQLite both use `F_FULLFSYNC`; the ordinary SQLite `fsync`
  profile remains in the raw CSV but is not presented as equivalent.
- The 16-reader/four-writer run measured 1,031,466 reads/s, 41,259 inserted
  rows/s and 0.425 ms writer p99. Read-only throughput peaked at 1,216,481
  reads/s with four readers.
- Warm writer p99 across insert/update/delete/identity/FK/derived profiles was
  0.425/1.363/2.418/1.258/0.748/1.612 ms. Every run validated final data and,
  where applicable, derived-index queries.
- ANN recall@10 was 0.952/0.994/0.998/1.000 at requested `ef_search`
  64/128/256/512, so every quality gate passed. Mean search was
  0.326/0.587/1.066/1.828 ms. Indexed ingestion took 15.347 s and persisted
  open took 22.606 s. The current values are stored separately from the prior
  diagnostic history so even small recall variation remains visible.

## How to read memory numbers

The published measurements use the current 384 MiB default: 64 MiB for
concurrent queries, 16 MiB admitted per query, 128 MiB for mutable index
deltas, 128 MiB for maintenance and an 8 MiB reserve. Clean file-backed `mmap`
pages and values already returned to the caller are deliberately outside that
accounting. So are the navigation caches of mapped HNSW runs (about 120 bytes
per indexed vector for node offsets, level tables and the id map): the base
graph's cache never counted against the delta pool, and since 2026-09-05 every
run is durable and background merges bound their number, so all runs are
treated like the base. `MaintenanceStats::vector_run_metadata_bytes` reports
the estimate.

Consequently, the configured envelope is not an RSS ceiling. macOS
`/usr/bin/time -l` reports both maximum resident set size and peak physical
footprint for the complete benchmark process. Mapped clean pages are
reclaimable, but they can still become resident and appear in RSS after scans
or full-base merges. Tables below keep logical admission and observed process
memory conceptually separate.

## Scalable load and reads: EliteSQL versus SQLite

The harness is
[`scale_vs_sqlite.rs`](crates/elitesql-core/benches/scale_vs_sqlite.rs). Both
engines receive deterministic rows with an explicit text primary key, two text
columns, and one signed 64-bit score. Rows are committed in 10K-row
transactions. EliteSQL automatic compaction and SQLite automatic WAL
checkpointing are disabled so hidden maintenance cannot move into the measured
write window.

Durability mappings are:

| Option | EliteSQL | SQLite |
|---|---|---|
| `fast` | `Durability::Fast` | WAL + `synchronous=OFF` |
| `balanced` | `Durability::Balanced` | WAL + `synchronous=NORMAL` |
| `safe` | `Durability::Safe` | WAL + `synchronous=FULL` |

The published scale runs use `fast`; smoke runs also passed under `balanced`
and `safe`. Schema creation is outside timing. `ingest wall` includes staging,
commit and automatic checkpoints. `final checkpoint` and the wait for queued
run promotions (`maintenance drain`) are reported separately; `total load`
includes all three. Point reads follow 1,000 warmups. The full scan is the
average of three unindexed equality lookups that each return one row.

### Current 384 MiB default

These are the current transactional results with 10K rows per transaction.
EliteSQL automatic compaction and SQLite automatic WAL checkpointing are
disabled; `total load` includes the explicit final checkpoint.

| Rows | Engine | Ingest wall | Final checkpoint | Total load | Rows/s | Point read | Full scan |
|---:|---|---:|---:|---:|---:|---:|---:|
| 1M | EliteSQL | 0.686 s | 0.102 s | 0.789 s | 1,267,688 | 1.818 µs | 0.017 s |
| 1M | SQLite | 0.577 s | 0.124 s | 0.700 s | 1,428,095 | 2.406 µs | 0.027 s |
| 10M | EliteSQL | 7.207 s | 0.155 s | 7.363 s | 1,358,180 | 2.524 µs | 0.167 s |
| 10M | SQLite | 5.855 s | 1.838 s | 7.693 s | 1,299,811 | 34.347 µs | 0.568 s |

At 10M rows EliteSQL's ingest wall is 1.231x SQLite's. EliteSQL performs 3.943 s
of checkpoint work and 0.458 s of promotion work during ingest, while SQLite
defers its checkpoint until after ingest; that checkpoint took 1.838 s here
(2.626 s on 2026-09-04 and 1.611 s on 2026-08-23), so the end-to-end result
(EliteSQL 0.957x SQLite's total load time) depends on that flush. At 1M rows
EliteSQL takes 1.127x SQLite's total load time. The 10M point-read and scan
figures depend on whether the background level promotion overlaps the read
phase (31.0 us and 0.690 s on 2026-09-04) and are reported as measured. Raw
data:
[`current-2026-09-05-scale-default-1m.csv`](benchmark-results/current-2026-09-05-scale-default-1m.csv)
and
[`current-2026-09-05-scale-default-10m.csv`](benchmark-results/current-2026-09-05-scale-default-10m.csv);
the previous run's files keep the `current-2026-09-04-` prefix.

### Direct sorted bulk path

`Db::bulk_insert_sorted` accepts strictly increasing explicit IDs and requires
derived indexes to be created after loading. It streams one canonical segment
and one primary run with bounded memory.

| Rows | Engine | Total load | Rows/s | Point read | Full scan |
|---:|---|---:|---:|---:|---:|
| 1M | EliteSQL bulk | 0.504 s | 1,983,112 | 1.781 µs | 0.018 s |
| 1M | SQLite | 0.707 s | 1,414,920 | 2.361 µs | 0.027 s |
| 10M | EliteSQL bulk | 4.388 s | 2,278,856 | 2.338 µs | 0.178 s |
| 10M | SQLite | 6.821 s | 1,466,081 | 11.580 µs | 0.347 s |

At 10M rows EliteSQL bulk is 1.554x faster end-to-end (1.403x at 1M). Raw
data:
[`current-2026-09-05-scale-bulk-1m.csv`](benchmark-results/current-2026-09-05-scale-bulk-1m.csv)
and
[`current-2026-09-05-scale-bulk-10m.csv`](benchmark-results/current-2026-09-05-scale-bulk-10m.csv).

### Small transaction and primary-key microbenchmark

The independent Criterion comparison in
[`vs_sqlite.rs`](crates/elitesql-core/benches/vs_sqlite.rs) uses 1,000-row
transactions and prepared primary-key reads. The published transaction result
is the sustained comparison: the fixed harness loaded 1M identical explicit-ID
rows in 1,000-row transactions, included automatic primary-index flush work in
ingest wall time, then measured the final checkpoint separately. These are
medians of five fresh runs under Fast/OFF durability with the 384 MiB default.
The A/B baseline is commit `5fb56aa`; the optimized rows identify the measured
dirty worktree:

| Sustained 1M-row workload | EliteSQL | SQLite | Result |
|---|---:|---:|---:|
| Ingest wall | 0.833 s | 0.638 s | EliteSQL 1.306x SQLite time |
| Final checkpoint | 0.097 s | 0.125 s | EliteSQL 1.29x faster |
| Total load | 0.930 s | 0.764 s | EliteSQL 1.218x SQLite time |
| Throughput | 1,075,081 rows/s | 1,308,961 rows/s | EliteSQL 17.9% lower |

Profiling found that EliteSQL cloned the cached table schema for every staged
row. Reusing the transaction's cached schema removes that allocation and copy
without changing validation, the memory budget, WAL format, commit semantics or
recovery. In matched five-run A/B measurements, EliteSQL median ingest improved
from 0.943 to 0.833 s (-11.7%) and total load from 1.040 to 0.930 s (-10.6%);
the total-time ratio moved from 1.36x to 1.22x SQLite. Staging improved by about
25%, while commit time was effectively unchanged, confirming where the saving
came from.

The independent warmed Criterion diagnostic corroborates the sustained result:
generated-ID EliteSQL averaged 0.911 ms per transaction, explicit-ID EliteSQL
0.904 ms, and SQLite 0.722 ms (1.26x and 1.25x SQLite time respectively). Its
EliteSQL confidence intervals are relatively broad, so the fixed five-run
workload above is the primary result. Autocommit semantics and durability costs
differ between engines and remain supporting evidence. Prepared primary-key
reads averaged 0.360 us in EliteSQL versus 1.418 us in SQLite, making EliteSQL
3.94x faster in that microbenchmark.

The 2026-09-04 acceptance run repeated the same five-run fixed workload after
the hot-path pass: EliteSQL median ingest 0.802 s, final checkpoint 0.097 s and
total load 0.900 s (1,111,522 rows/s) against SQLite's 0.641/0.128/0.769 s
(1,299,940 rows/s), so the total-time ratio is now 1.170x SQLite. The warmed
Criterion steady diagnostic in that run measured 0.991 ms (generated ids) and
1.033 ms (explicit ids) against SQLite's 0.719 ms with broad EliteSQL
intervals (0.87-1.05 and 0.93-1.10 ms); the fixed workload remains the primary
result. Raw data:
[`current-2026-09-04-small-transactions-fixed.csv`](benchmark-results/current-2026-09-04-small-transactions-fixed.csv)
and
[`current-2026-09-04-criterion.csv`](benchmark-results/current-2026-09-04-criterion.csv).

The 2026-09-05 acceptance run repeated it once more: EliteSQL median ingest
0.760 s, final checkpoint 0.096 s and total load 0.855 s (1,168,947 rows/s)
against SQLite's 0.621/0.123/0.745 s (1,342,804 rows/s), a total-time ratio of
1.148x SQLite. The warmed Criterion steady diagnostic measured 0.796 ms
(generated ids) and 0.884 ms (explicit ids) against SQLite's 0.698 ms, again
with broad EliteSQL intervals (0.67-0.93 and 0.75-1.01 ms). Prepared
primary-key reads averaged 0.318 us against SQLite's 1.385 us (4.35x). Raw
data:
[`current-2026-09-05-small-transactions-fixed.csv`](benchmark-results/current-2026-09-05-small-transactions-fixed.csv)
and
[`current-2026-09-05-criterion.csv`](benchmark-results/current-2026-09-05-criterion.csv).

Raw historical and optimized fixed-workload runs from 2026-08-23 are retained
together in
[`current-2026-08-23-small-transactions-fixed.csv`](benchmark-results/current-2026-08-23-small-transactions-fixed.csv).
The complete new Criterion central estimates and confidence intervals are in
[`current-2026-08-23-small-transactions-criterion.csv`](benchmark-results/current-2026-08-23-small-transactions-criterion.csv);
the earlier full-run values remain in
[`current-2026-08-23-criterion.csv`](benchmark-results/current-2026-08-23-criterion.csv)
for historical comparison.

## SQL query and bound-parameter overhead

The Criterion harness
[`sql.rs`](crates/elitesql-core/benches/sql.rs) builds 10K users and 1M orders.
It now compares interpolated benchmark literals with the equivalent safe
`query_params` calls over the same indexed plans.

| Query | Literal SQL | Bound values | Difference |
|---|---:|---:|---:|
| Unique-index point lookup | 4.061 µs | 3.447 µs | -15.1% |
| Indexed join, ~100 matching orders, top 10 | 155.58 µs | 153.05 µs | -1.6% |
| Unindexed 1M-row filter, `LIMIT 5` | 0.309 ms | — | — |
| `GROUP BY amount`, 997 groups, `count(*)` | 310.34 ms | — | — |
| `GROUP BY user_id`, 10K groups, `count/sum/max` | 340.47 ms | — | — |

The bound point-lookup confidence interval does not overlap the literal path in
this run; the indexed join difference is inside noise. Neither result indicates
a binding penalty. The bound path should still be chosen for type preservation
and injection safety. The table reports Criterion's measured query intervals,
not fixture-build time. The unindexed filter fell from 291.25 ms on
2026-08-23 to 34.23 ms on 2026-09-04 (predicate tested on the encoded payload,
mapped segment bytes, allocation-free run merge) and to 0.309 ms on
2026-09-05, when a bounded query stopped decoding a full 512-row batch of
matches to keep five. The indexed join lost its per-row record clones and the
two `GROUP BY` rows are new; both hash their groups within the query budget.
Raw estimates:
[`current-2026-09-05-criterion.csv`](benchmark-results/current-2026-09-05-criterion.csv)
(previous run:
[`current-2026-09-04-criterion.csv`](benchmark-results/current-2026-09-04-criterion.csv)).

## Concurrent writers

The harness is
[`concurrent_writers.rs`](crates/elitesql-core/benches/concurrent_writers.rs).
Each point uses 200K total rows, 10 rows/transaction, disjoint IDs, three fresh
runs, and the median. Checkpoints are outside the measured write window. The
EliteSQL harness gives the bounded delta 384 bytes per fixture row and asserts
that no consolidation occurred before timing ended; SQLite likewise disables
automatic WAL checkpoints. This isolates commit concurrency rather than
silently charging maintenance to only one engine.
For EliteSQL rows, current CSV/output also reports physical `wal_syncs` and the
number of commits served by multi-commit sync groups. This makes `Safe` and
`Balanced` group-commit efficiency observable instead of inferring it from
throughput alone.

### Current Fast and Balanced matrix — 2026-09-05

Values are median rows/s from three fresh 200K-row repetitions. SQLite uses the
matching `synchronous=OFF`/`NORMAL` profile for Fast/Balanced.

| Writers | EliteSQL Fast | SQLite Fast | Ratio | EliteSQL Balanced | SQLite Balanced | Ratio |
|---:|---:|---:|---:|---:|---:|---:|
| 1 | 938,739 | 440,937 | 2.13x | 787,212 | 440,132 | 1.79x |
| 2 | 708,305 | 400,221 | 1.77x | 593,407 | 353,052 | 1.68x |
| 4 | 676,083 | 326,806 | 2.07x | 566,279 | 321,089 | 1.76x |
| 8 | 830,916 | 255,612 | 3.25x | 717,022 | 307,631 | 2.33x |
| 16 | 974,657 | 160,888 | 6.06x | 854,570 | 174,278 | 4.90x |

These points are within the run-to-run spread of the two previous matrices;
the 2026-09-05 changes to the commit path (allocation-free derived-index keys
in the apply loop, the maintenance lease) do not show above that spread on a
table without derived indexes. Safe strict, in the same run, reached
2,673/3,082/9,847/19,119/34,733 rows/s at 1/2/4/8/16 writers against
2,661/2,662/2,596/2,654/2,579 for SQLite with `F_FULLFSYNC`, grouping
1.00/1.09/4.00/7.98/15.87 commits per sync with a p99 of 4.28/8.20/5.08/5.15/
5.77 ms. Raw repetitions:
[`current-2026-09-05-concurrent-writers-fast.csv`](benchmark-results/current-2026-09-05-concurrent-writers-fast.csv),
[`current-2026-09-05-concurrent-writers-balanced.csv`](benchmark-results/current-2026-09-05-concurrent-writers-balanced.csv)
and
[`current-2026-09-05-concurrent-writers-safe-strict.csv`](benchmark-results/current-2026-09-05-concurrent-writers-safe-strict.csv).

### 2026-09-04 Fast and Balanced matrix

Values are median rows/s from three fresh 200K-row repetitions. SQLite uses the
matching `synchronous=OFF`/`NORMAL` profile for Fast/Balanced.

| Writers | EliteSQL Fast | SQLite Fast | Ratio | EliteSQL Balanced | SQLite Balanced | Ratio |
|---:|---:|---:|---:|---:|---:|---:|
| 1 | 908,883 | 433,806 | 2.10x | 738,731 | 434,920 | 1.70x |
| 2 | 725,236 | 398,894 | 1.82x | 583,086 | 401,658 | 1.45x |
| 4 | 681,175 | 325,371 | 2.09x | 574,563 | 283,223 | 2.03x |
| 8 | 759,007 | 264,338 | 2.87x | 677,290 | 266,580 | 2.54x |
| 16 | 889,489 | 175,909 | 5.06x | 789,294 | 176,202 | 4.48x |

These points are within the run-to-run spread of the 2026-08-23 matrix
(822,577/703,584/695,985/755,724/889,721 Fast and
711,126/604,841/612,479/643,297/749,281 Balanced); the 2026-09-04 changes
target reads and index maintenance rather than the commit path. Raw
repetitions:
[`current-2026-09-04-concurrent-writers-fast.csv`](benchmark-results/current-2026-09-04-concurrent-writers-fast.csv)
and
[`current-2026-09-04-concurrent-writers-balanced.csv`](benchmark-results/current-2026-09-04-concurrent-writers-balanced.csv);
the previous run's files keep the `current-2026-08-23-` prefix.

### 2026-08-23 commit-mutex and tail-latency repeat

A focused before/after run used 40K rows, 10 rows/transaction, Fast durability,
three fresh repetitions and writer counts 1/2/4/8/16. The only scheduling
change between these two CSVs is the commit mutex: Safe retains the standard
mutex behavior that favors fsync coalescing, while Fast/Balanced use adaptive
spinning and hand the lock fairly to an already queued writer. Values below
are medians of the three EliteSQL repetitions.

| Writers | Baseline rows/s | Fair rows/s | Baseline p95 | Fair p95 | Baseline p99 | Fair p99 |
|---:|---:|---:|---:|---:|---:|---:|
| 1 | 429,863 | 438,859 | 28.5 µs | 27.7 µs | 34.4 µs | 31.7 µs |
| 2 | 378,170 | 376,130 | 66.1 µs | 68.2 µs | 71.4 µs | 75.9 µs |
| 4 | 351,370 | 382,868 | 179.3 µs | 110.5 µs | 235.1 µs | 127.0 µs |
| 8 | 364,122 | 379,243 | 542.5 µs | 221.0 µs | 833.1 µs | 240.6 µs |
| 16 | 365,455 | 373,521 | 1186.7 µs | 446.0 µs | 1957.5 µs | 486.8 µs |

At 4/8/16 writers, throughput improved 9.0%/4.2%/2.2%, p95 fell
38.4%/59.3%/62.4%, and p99 fell 46.0%/71.1%/75.1%. The two-writer difference
is within 1% throughput, with a small latency tradeoff. Instrumentation also
shows why throughput still plateaus: at 16 writers the average critical hold
is 18.5 µs, of which about 7.6 µs is WAL append and 6.6 µs is in-memory apply;
parallel preparation is no longer the dominant serialized cost.

The exact raw repetitions are
[`concurrent-writers-mutex-baseline-2026-08-23.csv`](benchmark-results/concurrent-writers-mutex-baseline-2026-08-23.csv)
and
[`concurrent-writers-fair-2026-08-23.csv`](benchmark-results/concurrent-writers-fair-2026-08-23.csv).
Balanced and Safe validation runs are retained in
[`concurrent-writers-balanced-2026-08-23.csv`](benchmark-results/concurrent-writers-balanced-2026-08-23.csv)
and
[`concurrent-writers-safe-smoke-2026-08-23.csv`](benchmark-results/concurrent-writers-safe-smoke-2026-08-23.csv).
Reproduce the focused Fast run with:

```sh
cargo bench -p elitesql-core --bench concurrent_writers -- \
  --rows 40000 --batch-size 10 --repetitions 3 \
  --writers 1,2,4,8,16 --durability fast \
  --csv benchmark-results/concurrent-writers-fair-2026-08-23.csv
```

This focused matrix is an architectural acceptance run, not a replacement for
the larger 200K-row EliteSQL/SQLite comparison below.

### 2026-08-23 coordinated commits and concurrent reads

Fast/Balanced/Safe now queue eligible disjoint inserts in FIFO order. A bounded
leader batch validates every transaction under the normal serialization lock,
keeps an independent version/CRC recovery frame for each commit, emits those
frames with vectored WAL writes and publishes the batch under one state write
lock. Under Safe, the complete batch shares one strict durability barrier and
no member returns before it completes. Updates, deletes, identity tables,
foreign keys and derived indexes keep the established general group-sync path.

The same focused Fast workload was repeated after that change. Values are
medians of three 40K-row runs; the raw CSV also records the number of commits
and batches that actually used coordinated publication.

| Writers | Fair rows/s | Coordinated rows/s | Change | Fair p99 | Coordinated p99 |
|---:|---:|---:|---:|---:|---:|
| 1 | 438,859 | 433,633 | -1.2% | 31.7 µs | 31.6 µs |
| 4 | 382,868 | 401,175 | +4.8% | 127.0 µs | 145.3 µs |
| 16 | 373,521 | 572,261 | +53.2% | 486.8 µs | 437.0 µs |

At 16 writers, 3,993–3,998 of 4,000 transactions entered coordinated batches,
with median critical-lock hold falling from 18.5 to 11.3 µs per commit. The
four-writer gain is smaller and p99 is 18.3 µs higher, so this is not presented
as a universal latency win. Maximum EliteSQL latency stayed below 0.7 ms in all
nine focused Fast runs. Raw data:
[`concurrent-writers-coordinator-2026-08-23.csv`](benchmark-results/concurrent-writers-coordinator-2026-08-23.csv).

### Safe strict-sync remediation — 2026-08-23

The Safe harness now records the effective sync primitive and runs SQLite in
two explicit profiles: `FULL + fullfsync=OFF` (ordinary `fsync`) and
`FULL + fullfsync=ON + checkpoint_fullfsync=ON` (`F_FULLFSYNC`). It verifies
both pragmas on every connection. EliteSQL additionally reports total physical
sync time, bytes, maximum group size, commits/sync, coalescing delay and leader
lock wait.

The table below is the median of three fresh 40K-row runs, 10 rows/transaction.
The SQLite column is the strict `F_FULLFSYNC` profile; ordinary `fsync` remains
in the CSV as a separately labeled non-equivalent baseline.

| Writers | EliteSQL Safe rows/s | SQLite strict rows/s | Ratio | Commits/sync | EliteSQL p99 |
|---:|---:|---:|---:|---:|---:|
| 1 | 2,954 | 2,858 | 1.03x | 1.00 | 4.25 ms |
| 2 | 4,319 | 2,845 | 1.52x | 1.64 | 8.04 ms |
| 4 | 9,960 | 2,760 | 3.61x | 4.00 | 4.94 ms |
| 8 | 19,829 | 2,749 | 7.21x | 8.00 | 5.00 ms |
| 16 | 36,045 | 2,826 | 12.75x | 15.75 | 6.09 ms |

Single-writer throughput remains limited by the measured 3.4-3.9 ms hardware
flush. In the 2026-08-23 run the two-writer point grouped almost no commits
(1.03 per sync) and showed roughly double the transaction latency; the
2026-09-04 rerun groups 1.64 commits per sync at two writers while its p99
stays near two flushes. At 16 writers, coordination reduces 4,000 logical
commits to a median 254 physical barriers and exceeds the 14 commits/sync
acceptance target.

The coalescing sweep retained 200 us: with 16 writers it reached 15.75
commits/sync and 38.7K rows/s in the focused run; 500 us added latency and
reduced throughput. A strict file probe found no stable benefit from reserving
64 MiB before the writes, so WAL preallocation was not added. Batch-size
artifacts for 1/10/100/1000 rows per transaction are retained separately.

Raw data:

- [`current-2026-09-04-concurrent-writers-safe-strict.csv`](benchmark-results/current-2026-09-04-concurrent-writers-safe-strict.csv)
  (previous run: [`current-2026-08-23-concurrent-writers-safe-strict.csv`](benchmark-results/current-2026-08-23-concurrent-writers-safe-strict.csv))
- [`safe-delay-200us-2026-08-23.csv`](benchmark-results/safe-delay-200us-2026-08-23.csv)
- [`current-2026-09-04-wal-preallocation.csv`](benchmark-results/current-2026-09-04-wal-preallocation.csv)
  (previous run: [`current-2026-08-23-wal-preallocation.csv`](benchmark-results/current-2026-08-23-wal-preallocation.csv))
- [`safe-batch-1-2026-08-23.csv`](benchmark-results/safe-batch-1-2026-08-23.csv), [`safe-batch-100-2026-08-23.csv`](benchmark-results/safe-batch-100-2026-08-23.csv), and [`safe-batch-1000-2026-08-23.csv`](benchmark-results/safe-batch-1000-2026-08-23.csv)

The new
[`concurrent_rw.rs`](crates/elitesql-core/benches/concurrent_rw.rs) harness
bulk-loads and checkpoints a persisted fixture before timing point readers,
then repeats the same reads alongside disjoint writers. It also times a full
paginated validation scan and exports query-pool waits, commit-lock times and
coordinator counts. Point lookups no longer reserve a 16 MiB operator slot:
their returned `Record` is caller-owned and they allocate no growing query
operator. Searches, scans and SQL retain the full admission budget.

The current complete run uses a 100K-row fixture, 1M point reads, 40K inserted
rows, 10 rows per transaction and three repetitions. Selected medians are:

| Readers | Writers | Reads/s | Reader p99 | Writes/s | Writer p99 |
|---:|---:|---:|---:|---:|---:|
| 1 | 0 | 998,841 | 1.29 us | — | — |
| 4 | 0 | 1,934,787 | 2.71 us | — | — |
| 16 | 0 | 3,583,376 | 4.12 us | — | — |
| 4 | 1 | 1,671,339 | 18.04 us | 66,854 | 172.54 us |
| 16 | 1 | 1,937,466 | 88.42 us | 77,499 | 857.96 us |
| 4 | 4 | 1,803,611 | 10.29 us | 72,144 | 397.71 us |
| 16 | 4 | 2,526,321 | 70.54 us | 101,053 | 486.88 us |

Point reads now borrow their payload from a read-only segment mapping instead
of issuing one `pread` system call per record, which is why read throughput
keeps scaling to 16 readers (the 2026-08-23 run peaked at 1,216,481 reads/s
with four readers and 1,031,466 reads/s with 16 readers and four writers).
Raw full matrix:
[`current-2026-09-04-concurrent-rw.csv`](benchmark-results/current-2026-09-04-concurrent-rw.csv)
(previous run:
[`current-2026-08-23-concurrent-rw.csv`](benchmark-results/current-2026-08-23-concurrent-rw.csv)).

The following two paragraphs retain the earlier 200K-read focused history that
motivated CPU-aware admission; they are not the current full-matrix values.

On the 100K-row, 200K-read focused run, median read throughput scaled from
362,557 reads/s with one reader to 666,045 with four and 841,250 with eight;
sixteen readers measured 815,137, with zero query-pool waits throughout. With
four writers active, median read/write rates were 570,538/57,054 at four
readers and 658,762/65,876 at sixteen. The sixteen-reader mixed case still
shows 11.1 ms writer p99, identifying state-lock contention as a remaining
tail-latency target rather than hiding it. Raw data:
[`concurrent-rw-2026-08-23.csv`](benchmark-results/concurrent-rw-2026-08-23.csv).

The follow-up adds CPU-aware admission only while a commit or identity
reservation is active. It leaves the read-only path unthrottled, admits at
most `available_parallelism - active_writers` point readers (with a minimum of
one), and yields excess point readers until a slot opens. This is scheduling,
not a consistency shortcut: record decoding and MVCC visibility still happen
under the same state read lock. The exact 100K-row/200K-read comparison was
repeated three times:

| 16 readers + 4 writers | Before | Adaptive admission | Change |
|---|---:|---:|---:|
| Read throughput | 658,762/s | 664,573/s | +0.9% |
| Write throughput | 65,876 rows/s | 66,457 rows/s | +0.9% |
| Reader p99 | 61.958 us | 221.542 us | +159.584 us |
| Writer p95 | 2,576.459 us | 451.958 us | -82.5% |
| Writer p99 | 11,146.500 us | 595.000 us | -94.7% |
| Commit-lock wait/commit | 247.640 us | 75.783 us | -69.4% |
| Commit-lock hold/commit | 72.534 us | 54.483 us | -24.9% |

The intended tradeoff is explicit: while writers are active, reader p99 rises
from 0.062 to 0.222 ms so queued writers can run. Aggregate mixed throughput
does not fall. At four readers/four writers the allowance never filled and
zero reads throttled; writer p99 stayed within 2% (0.450 versus 0.442 ms).
Read-only runs also recorded zero throttles: their observed throughput moved
-5.3% at four readers and +2.8% at sixteen, both inside the spread of the
baseline repetitions, while read-only p99 did not regress. Raw repetitions:
[`concurrent-rw-admission-comparison-2026-08-23.csv`](benchmark-results/concurrent-rw-admission-comparison-2026-08-23.csv).

The new
[`contention_matrix.rs`](crates/elitesql-core/benches/contention_matrix.rs)
extends the workload to inserts, updates, deletes, generated identity values,
foreign-key validation and synchronous equality/BM25/HNSW maintenance. Each
row below is the median of three fresh Fast runs with a 50K-row reader fixture,
100K point reads, 5K mutations in 10-row transactions, 16 readers and four
writers. Every run validates final row counts and values; derived runs also
query all three indexes.

| Profile | Cache mode | Reads/s | Writes/s | Reader p99 | Writer p99 |
|---|---|---:|---:|---:|---:|
| Insert | warm | 2,105,668 | 105,283 | 92.6 us | 419.9 us |
| Insert | reopened | 2,150,931 | 107,547 | 91.2 us | 1,087.5 us |
| Update | warm | 1,565,250 | 78,263 | 132.5 us | 1,085.2 us |
| Update | reopened | 1,637,955 | 81,898 | 112.1 us | 1,227.8 us |
| Delete | warm | 1,725,273 | 86,264 | 120.7 us | 954.1 us |
| Delete | reopened | 1,782,535 | 89,127 | 114.1 us | 1,137.8 us |
| Identity | warm | 1,221,203 | 61,060 | 190.5 us | 963.0 us |
| Identity | reopened | 1,280,003 | 64,000 | 174.8 us | 914.0 us |
| Foreign key | warm | 1,702,481 | 85,124 | 120.8 us | 812.0 us |
| Foreign key | reopened | 2,276,854 | 113,843 | 46.3 us | 751.8 us |
| Derived indexes | warm | 909,027 | 45,451 | 297.2 us | 1,513.5 us |
| Derived indexes | reopened | 1,077,327 | 53,866 | 252.4 us | 915.2 us |

Read and write throughput roughly doubled in every profile against the
2026-08-23 matrix (988,867/843,696/890,002/659,171/853,877/563,235 warm
reads/s), with writer p99 equal or lower except the reopened insert point,
where one repetition carried a 1.1 ms tail.

`cold` always closes/reopens the database and skips warmup. On Linux/Android it
also requests `POSIX_FADV_DONTNEED` for every database file and records
attempted/successful evictions. macOS lacks that API, so this machine reports
`evict=0/0`: those rows prove reopen/recovery behavior but are **not** a valid
OS-cold-versus-warm comparison. Raw data:
[`current-2026-09-04-contention-matrix.csv`](benchmark-results/current-2026-09-04-contention-matrix.csv)
(previous run:
[`current-2026-08-23-contention-matrix.csv`](benchmark-results/current-2026-08-23-contention-matrix.csv)).

The first matrix exposed a separate identity tail: reservation changed state
before commit admission became active. Announcing that short write section to
the same admission policy reduced median identity p99 from 8–10 ms in the two
pre-change matrix passes to 1.2–1.3 ms in the final pass. Allocating identity
ranges per transaction remains a plausible throughput optimization, but is a
larger semantic change and was not needed for this tail fix.

Two read-path regressions were corrected before this run. Immutable primary and
secondary cursors now binary-seek the complete exclusive continuation key
instead of replaying every earlier page for every batch. Unindexed equality
filtering again compares encoded records while walking physical segments and
decodes only matches. In the focused 100K-row diagnostic, paginated scan time
fell from 2.229 s to 0.141 s from the seek alone, and the single-match
unindexed equality scan fell to 0.004 s with physical filtering.

### Historical relational-compatibility writer baseline — 2026-08-09

The following tables are retained for comparison with the pre-compatibility
run; the current Fast/Balanced/Safe matrices are the tables earlier in this
section.

| Writers | EliteSQL rows/s | SQLite rows/s | EliteSQL / SQLite | Change vs prior EliteSQL |
|---:|---:|---:|---:|---:|
| 1 | 501,179 | 222,970 | 2.248× | +1.99% |
| 2 | 405,854 | 217,778 | 1.864× | +0.38% |
| 4 | 378,800 | 177,277 | 2.137× | -1.57% |
| 8 | 369,113 | 153,889 | 2.399× | -1.94% |

| Writers | Engine | p50 | p95 | p99 | Maximum |
|---:|---|---:|---:|---:|---:|
| 1 | EliteSQL | 18.9 µs | 24.2 µs | 28.3 µs | 0.515 ms |
| 1 | SQLite | 31.6 µs | 80.0 µs | 97.7 µs | 0.671 ms |
| 2 | EliteSQL | 48.8 µs | 53.0 µs | 63.3 µs | 0.510 ms |
| 2 | SQLite | 31.8 µs | 79.1 µs | 100.7 µs | 469.146 ms |
| 4 | EliteSQL | 103.7 µs | 111.9 µs | 131.1 µs | 1.983 ms |
| 4 | SQLite | 51.5 µs | 80.5 µs | 101.0 µs | 889.757 ms |
| 8 | EliteSQL | 210.5 µs | 223.4 µs | 318.5 µs | 4.382 ms |
| 8 | SQLite | 52.1 µs | 81.8 µs | 105.6 µs | 1202.220 ms |

EliteSQL wins median throughput at every writer count by 1.86x–2.40x. Relative
to the pre-compatibility run, EliteSQL throughput moved by no more than 2% at
any writer count. Its p99 improved at one, two and four writers, but increased
22.1% at eight writers. Two of the new runs also exposed isolated maximum
latency spikes at four and eight writers (median maxima 1.983 and 4.382 ms), so
the compatibility change is throughput-neutral in this test but the high-end
commit tail still needs attention. SQLite's serialized lock waits reached
469–1202 ms with 2–8 writers.

As in the scale harness, rows use explicit disjoint text IDs and no foreign
keys. This is a regression check for shared transaction machinery, not a
microbenchmark of identity allocation or cascade validation.

Raw repetitions are in
[`benchmark-results/concurrent-writers-relational-compat-2026-08-09.csv`](benchmark-results/concurrent-writers-relational-compat-2026-08-09.csv).
The pre-compatibility baseline remains in
[`benchmark-results/concurrent-writers-2026-08-08.csv`](benchmark-results/concurrent-writers-2026-08-08.csv).
The charts below show the current Fast matrix: they were regenerated from
[`current-2026-09-04-concurrent-writers-fast.csv`](benchmark-results/current-2026-09-04-concurrent-writers-fast.csv)
with `scripts/plot-concurrent-benchmark.py` (which now accepts the
`SQLite-fsync` engine label the current CSVs use). The 2026-08-09 values remain
in the table above and in its CSV:

![Concurrent write throughput](benchmark-results/concurrent-throughput.svg)

![Concurrent transaction p99 latency](benchmark-results/concurrent-p99-latency.svg)

![Worst concurrent transaction latency](benchmark-results/concurrent-max-latency.svg)

## Synthetic ANN: 100K vectors

The Criterion harness [`vector.rs`](crates/elitesql-core/benches/vector.rs)
uses 100K deterministic clustered vectors of dimension 64 and compares HNSW
against brute-force top-10 ground truth over 50 queries.

| `ef_search` | Recall@10 | Mean search interval |
|---:|---:|---:|
| 64 | 0.9520 | 0.210 ms |
| 128 | 0.9940 | 0.315 ms |
| 256 | 0.9980 | 0.591 ms |
| 512 | 1.0000 | 1.084 ms |

Indexed ingestion took 8.830 s and opening the persisted graph took 74.5 ms
(9.198 s and 10.937 s on 2026-09-04; 15.347 s and 22.606 s on 2026-08-23).
The 2026-09-04 open was a full rebuild, because the immutable runs published
while running were not kept on disk; since 2026-09-05 they are durable and the
open maps them. Every recall gate passed with recall identical to both previous
runs at every `ef_search`: the vectorized distance kernels change only the
floating-point summation order, neighbour prefetching evaluates the same
candidates in the same order, and the visited bitmap and incremental memory
accounting do not touch graph construction. Raw current quality and central
Criterion estimates are in
[`current-2026-09-05-ann.csv`](benchmark-results/current-2026-09-05-ann.csv);
the previous runs are in
[`current-2026-09-04-ann.csv`](benchmark-results/current-2026-09-04-ann.csv)
and
[`current-2026-08-23-ann.csv`](benchmark-results/current-2026-08-23-ann.csv),
and earlier diagnostic values remain in
[`ann-quality-history-2026-08-23.csv`](benchmark-results/ann-quality-history-2026-08-23.csv).

### Memory sizing on AWS t3.large

A focused restart harness in
[`vector_memory.rs`](crates/elitesql-core/examples/vector_memory.rs) measured the
same 100K-vector, 64-dimensional workload on an AWS `t3.large` (2 vCPU, 8 GiB
RAM). It reports how many nodes reach the durable HNSW base before close and how
many must be reconstructed during the next open.

| Total | Index delta | Maintenance | Memtable | Durable nodes | Catch-up rows | Open | Max RSS |
|---:|---:|---:|---:|---:|---:|---:|---:|
| 256 MiB | 64 MiB | 64 MiB | 64 MiB | 50,000 | 50,000 | 57.639 s | 144 MiB |
| 304 MiB | 109 MiB | 109 MiB | 64 MiB | 90,000 | 10,000 | 6.385 s | 198 MiB |
| 304 MiB | 110 MiB | 110 MiB | 64 MiB | 100,000 | 0 | 0.356 s | 196 MiB |
| 320 MiB | 112 MiB | 112 MiB | 64 MiB | 100,000 | 0 | 0.347 s | 196 MiB |
| 384 MiB | 128 MiB | 128 MiB | 64 MiB | 100,000 | 0 | 0.536 s | 196 MiB |

The measured index-delta peak was 115,000,000 bytes (109.67 MiB), explaining
the sharp boundary between 109 and 110 MiB. The exact 110 MiB minimum has less
than 0.4 MiB of headroom, so the profile selected as the default on 2026-08-09
is 384 MiB total, 128 MiB each for index delta and maintenance, and a 64 MiB
memtable. It retains the complete 100K graph while using only about 196 MiB of
observed process RSS. Re-run the harness when vector count, dimension, HNSW
parameters, ID sizes or indexed metadata change.

```bash
cargo run --release --locked -p elitesql-core --example vector_memory -- \
  --rows 100000 --total-mib 384 --index-mib 128 \
  --maintenance-mib 128 --memtable-mib 64
```

## Real multilingual ANN: Potion + MIRACL-es, 250K

The reproducible example is
[`examples/vector_search_potion/miracl_search.py`](examples/vector_search_potion/miracl_search.py).
It uses `minishlab/potion-multilingual-128M` through Model2Vec, 256-dimensional
normalized embeddings, 648 Spanish queries, and 250K real MIRACL-es passages.
Model and dataset revisions are pinned in the script.

### Build memory boundary

An undersized maintenance configuration completed insertion but rejected HNSW
construction with `Error::MemoryLimit`, the intended safe failure mode. The
current 384 MiB default has not yet been acceptance-tested on this 250K
workload.

The successful build used an explicit 640 MiB total envelope and 512 MiB
maintenance pool:

```bash
python3 examples/vector_search_potion/miracl_search.py \
  --corpus-size 250000 \
  --db target/potion-miracl-es-250k.esql \
  --total-memory-mib 640 \
  --maintenance-memory-mib 512 \
  --ef-search 128 \
  --rebuild \
  --output-json benchmark-results/potion-miracl-es-250k.json
```

| Phase | Time |
|---|---:|
| Embed 250K passages | 11.649 s |
| Insert into EliteSQL | 55.895 s |
| Build and persist HNSW V4 | 230.762 s |
| Complete process | 305.760 s |

The resulting database is 690.12 MiB, including a 283 MiB HNSW file. The
end-to-end Python process peaked at 2.16 GiB RSS; it retains the model,
250K-vector NumPy matrix, exact-search state, canonical records, and mutable
HNSW builder, so this is not an engine-only heap number.

A separate read-only Python process opened the persisted 250K database in
0.454 s and executed one cold `ef=128` search in 3.973 ms. It peaked at
376.66 MiB RSS and 65.88 MiB physical footprint; most of the RSS is mapped file
content touched during open/search.

### Search quality and latency on the same persisted index

Each row below is a fresh process reopening the same index. Search latency
excludes query embedding generation.

| `ef_search` | ANN recall@10 | Mean | p50 | p95 | p99 | Global Hit@10 |
|---:|---:|---:|---:|---:|---:|---:|
| 128 | 0.9630 | 1.341 ms | 1.324 ms | 1.598 ms | 1.893 ms | 0.5710 |
| 256 | 0.9789 | 1.721 ms | 1.709 ms | 2.106 ms | 2.423 ms | 0.5895 |
| 512 | 0.9880 | 2.486 ms | 2.440 ms | 3.173 ms | 3.463 ms | 0.5957 |

Exact global search obtains `Hit@10=0.5972`. `ef=256` remains a strong
quality/latency point, while `ef=512` nearly removes ANN loss. The reranking
subset remains perfect at `Hit@10=1.0`; MIRACL's reranking judgments are not an
exhaustive relevance labeling of the 250K global corpus.

Structured results:

- [`ef=128 build`](benchmark-results/potion-miracl-es-250k.json)
- [`ef=128 reopened`](benchmark-results/potion-miracl-es-250k-ef128.json)
- [`ef=256`](benchmark-results/potion-miracl-es-250k-ef256.json)
- [`ef=512`](benchmark-results/potion-miracl-es-250k-ef512.json)

## Performance work implied by these results

Immutable runs resolved the original superlinear checkpoint defect. The direct
bulk loader, single-pass unindexed scan, compact page-directory format,
transaction-local table interning, direct checkpoint run generation and raw
disjoint-page promotion close the measured SQL and physical memory gaps.
Remaining work is narrower:

1. **Implemented and measured for automatic and explicit checkpoints.** The
   full 10M rerun includes the WAL-bridge publication protocol; checkpoint work
   remains included through the explicit final barrier, not hidden in drain.
2. **Implemented for eligible Fast/Balanced/Safe inserts.** FIFO coordination,
   independent WAL recovery frames, vectored append and one state publication
   increased focused Fast throughput and now lets Safe batches share one strict
   barrier without acknowledging early. Complex transactions retain the
   proven general group-sync path.
3. **Implemented for point reads competing with transactional writers.**
   CPU-aware admission cut the focused 16-reader/four-writer commit p99 by
   94.7%, and the expanded matrix now covers update/delete, identity, foreign
   keys and synchronous derived indexes. Next, repeat true OS-cold runs on
   Linux, Safe/Balanced durability, and additional CPU counts; macOS reopening
   alone does not evict its page cache.
4. Extend primary/equality/BM25 byte/time counters to vector, segment and fsync
   work. Consider transaction-local identity range allocation only with an
   explicit decision about rollback gaps and persisted high-water semantics.
5. Make large HNSW construction more incremental: the 250K Potion build safely
   rejects the default maintenance pool and currently needs an explicit larger
   profile, although persisted search itself is mmap-backed and efficient.
   Since 2026-09-05 every published run survives a restart and comparably
   sized runs are merged in the background without stalling commits. The
   remaining limit is the rebuild ceiling: a merged graph must fit half the
   maintenance pool, so very large indexes keep several runs per size tier
   unless the pool grows or the merge learns to read vectors from the source
   mappings instead of copying them.
6. Repeat the acceptance matrix on additional hardware; one favorable machine
   is evidence, not a universal performance guarantee.

Every follow-up must preserve the bounded-memory and crash-recovery contracts.

## Reproducing

```bash
# Transactional scale with the current 384 MiB default
cargo bench -p elitesql-core --bench scale_vs_sqlite -- \
  --rows 10m --durability fast --batch-size 10k \
  --point-reads 10k --full-scans 3 \
  --csv benchmark-results/current-2026-09-05-scale-default-10m.csv

# Direct sorted bulk load
cargo bench -p elitesql-core --bench scale_vs_sqlite -- \
  --rows 10m --durability fast --bulk-sorted \
  --point-reads 10k --full-scans 3 \
  --csv benchmark-results/current-2026-09-05-scale-bulk-10m.csv

/usr/bin/time -l target/release/deps/scale_vs_sqlite-<hash> \
  --rows 10m --durability fast --batch-size 10k \
  --point-reads 1k --full-scans 1 --engine elitesql

# Sustained transaction Criterion microbenchmarks
cargo bench -p elitesql-core --bench vs_sqlite -- \
  --save-baseline current-small-txn-2026-09-05

# Fixed sustained workload; repeat five times with a fresh output filename
cargo bench -p elitesql-core --bench scale_vs_sqlite -- \
  --rows 1m --durability fast --batch-size 1k \
  --point-reads 100 --full-scans 1 \
  --csv benchmark-results/small-transactions-repetition-1.csv

# SQL, including bound parameters
cargo bench -p elitesql-core --bench sql -- \
  --save-baseline current-2026-09-05

# Concurrent writers and charts
cargo bench -p elitesql-core --bench concurrent_writers -- \
  --rows 200k --batch-size 10 --repetitions 3 --durability fast \
  --writers 1,2,4,8,16 \
  --csv benchmark-results/current-2026-09-05-concurrent-writers-fast.csv

cargo bench -p elitesql-core --bench concurrent_writers -- \
  --rows 40k --batch-size 10 --repetitions 3 --durability safe \
  --writers 1,2,4,8,16 --sqlite-sync both --safe-group-delay-us 200 \
  --csv benchmark-results/current-2026-09-05-concurrent-writers-safe-strict.csv
cargo bench -p elitesql-core --bench wal_preallocation -- \
  "$PWD/benchmark-results/current-2026-09-05-wal-preallocation.csv"
python3 scripts/plot-concurrent-benchmark.py \
  benchmark-results/current-2026-09-05-concurrent-writers-fast.csv \
  --output-dir benchmark-results

# Persisted concurrent readers and mixed readers/writers
cargo bench -p elitesql-core --bench concurrent_rw -- \
  --rows 100k --read-operations 1m --write-rows 40k --batch-size 10 \
  --readers 1,2,4,8,16 --writers 0,1,4 --repetitions 3 \
  --csv benchmark-results/current-2026-09-05-concurrent-rw.csv

# Updates/deletes, identity/FK, derived indexes and warm/reopened cache modes
cargo bench -p elitesql-core --bench contention_matrix -- \
  --workloads insert,update,delete,identity,foreign-key,derived \
  --cache warm,cold --readers 16 --writers 4 --rows 50k \
  --read-operations 100k --write-rows 5k --batch-size 10 \
  --repetitions 3 \
  --csv benchmark-results/current-2026-09-05-contention-matrix.csv

# Synthetic ANN
cargo bench -p elitesql-core --bench vector -- \
  --save-baseline current-2026-09-05
```

For the exact executable path used by `/usr/bin/time`, first run
`cargo bench -p elitesql-core --bench scale_vs_sqlite --no-run`. Re-run on the
target hardware before making capacity decisions; these values describe one
machine and one current worktree, not universal guarantees.
