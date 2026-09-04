# EliteSQL current benchmarks

This document publishes the complete current acceptance run collected on
2026-09-04, plus clearly labeled historical and focused diagnostic results.
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
reads and vectorized HNSW distance kernels. In the current 10M transactional
workload EliteSQL's ingest wall is 1.202x SQLite's, while its total load
(7.330 s) beat SQLite's (8.631 s) because SQLite's deferred checkpoint took
2.626 s; the direct sorted bulk path was 1.599x faster. Current Fast/Balanced
throughput beat SQLite at every measured writer count. Safe is compared
separately using the same macOS `F_FULLFSYNC` primitive on both engines.

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
- Rust/Cargo 1.93.1, release benchmark profile
- SQLite 3.45.0 through `rusqlite`'s bundled build
- EliteSQL 0.0.1 dirty worktree on top of commit `fcdcde7`

The worktree contains the changes being measured; the commit hash alone is not
sufficient to reproduce these numbers until those changes are committed.
Benchmarks ran sequentially on the same machine. No benchmark ran in parallel
with another measurement.

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

## Full acceptance rerun — 2026-09-04

A complete local rerun was performed on AC power after the hot-path
optimizations described above, using the same harnesses, parameters and
sequential procedure as the 2026-08-23 run. The immutable environment, result
summary and artifact inventory are in
[`current-acceptance-2026-09-04.md`](benchmark-results/current-acceptance-2026-09-04.md);
all raw files use the `current-2026-09-04-*` prefix. The 2026-08-23 artifacts
remain in place for comparison.

Headline results from the current run:

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

Headline results from that run (superseded by the 2026-09-04 tables below):

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
accounting.

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
| 1M | EliteSQL | 0.728 s | 0.104 s | 0.832 s | 1,202,018 | 1.856 µs | 0.019 s |
| 1M | SQLite | 0.596 s | 0.123 s | 0.719 s | 1,390,903 | 2.417 µs | 0.027 s |
| 10M | EliteSQL | 7.220 s | 0.111 s | 7.330 s | 1,364,219 | 31.014 µs | 0.690 s |
| 10M | SQLite | 6.005 s | 2.626 s | 8.631 s | 1,158,656 | 52.326 µs | 0.736 s |

At 10M rows EliteSQL's ingest wall is 1.202x SQLite's. EliteSQL performs 3.606 s
of checkpoint work and 0.433 s of promotion work during ingest, while SQLite
defers its checkpoint until after ingest; that checkpoint took 2.626 s here
against 1.611 s on 2026-08-23 with an identical SQLite ingest wall, so the
end-to-end result (EliteSQL 0.849x SQLite's total load time) depends on that
flush. At 1M rows EliteSQL takes 1.157x SQLite's total load time. Raw data:
[`current-2026-09-04-scale-default-1m.csv`](benchmark-results/current-2026-09-04-scale-default-1m.csv)
and
[`current-2026-09-04-scale-default-10m.csv`](benchmark-results/current-2026-09-04-scale-default-10m.csv).

### Direct sorted bulk path

`Db::bulk_insert_sorted` accepts strictly increasing explicit IDs and requires
derived indexes to be created after loading. It streams one canonical segment
and one primary run with bounded memory.

| Rows | Engine | Total load | Rows/s | Point read | Full scan |
|---:|---|---:|---:|---:|---:|
| 1M | EliteSQL bulk | 0.525 s | 1,904,250 | 1.835 µs | 0.019 s |
| 1M | SQLite | 0.713 s | 1,401,958 | 2.367 µs | 0.027 s |
| 10M | EliteSQL bulk | 4.559 s | 2,193,550 | 2.187 µs | 0.201 s |
| 10M | SQLite | 7.289 s | 1,371,974 | 38.026 µs | 0.605 s |

At 10M rows EliteSQL bulk is 1.599x faster end-to-end (1.358x at 1M). Raw
data:
[`current-2026-09-04-scale-bulk-1m.csv`](benchmark-results/current-2026-09-04-scale-bulk-1m.csv)
and
[`current-2026-09-04-scale-bulk-10m.csv`](benchmark-results/current-2026-09-04-scale-bulk-10m.csv).

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
| Unique-index point lookup | 4.357 µs | 3.656 µs | -16.1% |
| Indexed join, ~100 matching orders, top 10 | 187.05 µs | 184.73 µs | -1.2% |
| Unindexed 1M-row filter, `LIMIT 5` | 34.23 ms | — | — |

The bound point-lookup confidence interval does not overlap the literal path in
this run; the indexed join difference is inside noise. Neither result indicates
a binding penalty. The bound path should still be chosen for type preservation
and injection safety. The table reports Criterion's measured query intervals,
not fixture-build time. The unindexed filter fell from 291.25 ms on
2026-08-23 because the primary-directory scan now tests the predicate on the
encoded payload, borrows segment bytes from a mapping instead of one `pread`
per row, and merges runs without per-record allocation. Raw estimates:
[`current-2026-09-04-criterion.csv`](benchmark-results/current-2026-09-04-criterion.csv).

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

### Current Fast and Balanced matrix — 2026-09-04

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
| 64 | 0.9520 | 0.237 ms |
| 128 | 0.9940 | 0.416 ms |
| 256 | 0.9980 | 0.773 ms |
| 512 | 1.0000 | 1.353 ms |

Indexed ingestion took 9.198 s and opening the persisted graph took 10.937 s
(15.347 s and 22.606 s on 2026-08-23). Every recall gate passed with recall
identical to the previous run at every `ef_search`: the vectorized distance
kernels change only the floating-point summation order, and the visited bitmap
and incremental memory accounting do not touch graph construction. Raw current
quality and central Criterion estimates are in
[`current-2026-09-04-ann.csv`](benchmark-results/current-2026-09-04-ann.csv);
the previous run is in
[`current-2026-08-23-ann.csv`](benchmark-results/current-2026-08-23-ann.csv)
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
6. Repeat the acceptance matrix on additional hardware; one favorable machine
   is evidence, not a universal performance guarantee.

Every follow-up must preserve the bounded-memory and crash-recovery contracts.

## Reproducing

```bash
# Transactional scale with the current 384 MiB default
cargo bench -p elitesql-core --bench scale_vs_sqlite -- \
  --rows 10m --durability fast --batch-size 10k \
  --point-reads 10k --full-scans 3 \
  --csv benchmark-results/current-2026-09-04-scale-default-10m.csv

# Direct sorted bulk load
cargo bench -p elitesql-core --bench scale_vs_sqlite -- \
  --rows 10m --durability fast --bulk-sorted \
  --point-reads 10k --full-scans 3 \
  --csv benchmark-results/current-2026-09-04-scale-bulk-10m.csv

/usr/bin/time -l target/release/deps/scale_vs_sqlite-<hash> \
  --rows 10m --durability fast --batch-size 10k \
  --point-reads 1k --full-scans 1 --engine elitesql

# Sustained transaction Criterion microbenchmarks
cargo bench -p elitesql-core --bench vs_sqlite -- \
  --save-baseline current-small-txn-2026-09-04

# Fixed sustained workload; repeat five times with a fresh output filename
cargo bench -p elitesql-core --bench scale_vs_sqlite -- \
  --rows 1m --durability fast --batch-size 1k \
  --point-reads 100 --full-scans 1 \
  --csv benchmark-results/small-transactions-repetition-1.csv

# SQL, including bound parameters
cargo bench -p elitesql-core --bench sql -- \
  --save-baseline current-2026-09-04

# Concurrent writers and charts
cargo bench -p elitesql-core --bench concurrent_writers -- \
  --rows 200k --batch-size 10 --repetitions 3 --durability fast \
  --writers 1,2,4,8,16 \
  --csv benchmark-results/current-2026-09-04-concurrent-writers-fast.csv

cargo bench -p elitesql-core --bench concurrent_writers -- \
  --rows 40k --batch-size 10 --repetitions 3 --durability safe \
  --writers 1,2,4,8,16 --sqlite-sync both --safe-group-delay-us 200 \
  --csv benchmark-results/current-2026-09-04-concurrent-writers-safe-strict.csv
cargo bench -p elitesql-core --bench wal_preallocation -- \
  "$PWD/benchmark-results/current-2026-09-04-wal-preallocation.csv"
python3 scripts/plot-concurrent-benchmark.py \
  benchmark-results/current-2026-09-04-concurrent-writers-fast.csv \
  --output-dir benchmark-results

# Persisted concurrent readers and mixed readers/writers
cargo bench -p elitesql-core --bench concurrent_rw -- \
  --rows 100k --read-operations 1m --write-rows 40k --batch-size 10 \
  --readers 1,2,4,8,16 --writers 0,1,4 --repetitions 3 \
  --csv benchmark-results/current-2026-09-04-concurrent-rw.csv

# Updates/deletes, identity/FK, derived indexes and warm/reopened cache modes
cargo bench -p elitesql-core --bench contention_matrix -- \
  --workloads insert,update,delete,identity,foreign-key,derived \
  --cache warm,cold --readers 16 --writers 4 --rows 50k \
  --read-operations 100k --write-rows 5k --batch-size 10 \
  --repetitions 3 \
  --csv benchmark-results/current-2026-09-04-contention-matrix.csv

# Synthetic ANN
cargo bench -p elitesql-core --bench vector -- \
  --save-baseline current-2026-09-04
```

For the exact executable path used by `/usr/bin/time`, first run
`cargo bench -p elitesql-core --bench scale_vs_sqlite --no-run`. Re-run on the
target hardware before making capacity decisions; these values describe one
machine and one current worktree, not universal guarantees.
