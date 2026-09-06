# EliteSQL current acceptance run — 2026-09-05

This is the immutable summary for the complete sequential rerun whose raw
artifacts use the `current-2026-09-05-*` prefix. It measures the dirty worktree
on top of commit `849190f` after the 2026-09-05 passes described in
`benchmark.md`: durable HNSW runs with a run manifest and background merges,
honest maintenance-pool accounting with an explicit maintenance lease, a
minimum size for background derived publications, HNSW neighbour prefetching,
clone-free SQL predicate evaluation and joins, LIMIT-aware scan batches, a
budgeted hash GROUP BY, the point-read lock scope, allocation-free
derived-index keys in the commit apply loop and fat LTO. The commit alone does
not reproduce the results.

## Environment

- MacBook Air, Apple M5, 10 logical CPUs, 16 GiB RAM
- macOS 26.6.2 (25G83), arm64
- Rust/Cargo 1.93.1, release benchmark profile (fat LTO, one codegen unit)
- SQLite 3.45.0 through the bundled `rusqlite` build
- Battery power, 82-79%, low power mode off; the 2026-09-04 run was on AC
  power. `pmset -g therm` reported no CPU speed limit before or after any
  step. An IDE was open during the run and used part of one core; benchmarks
  executed sequentially from one script between 10:57 and 11:09 local time,
  none overlapping another measurement or a build.
- EliteSQL default memory profile: 384 MiB

## Scale

| Path | Rows | EliteSQL total | SQLite total | Result |
|---|---:|---:|---:|---:|
| Transactional | 1M | 0.789 s | 0.700 s | 1.127x SQLite time |
| Transactional | 10M | 7.363 s | 7.693 s | EliteSQL 1.045x faster |
| Direct sorted bulk | 1M | 0.504 s | 0.707 s | EliteSQL 1.403x faster |
| Direct sorted bulk | 10M | 4.388 s | 6.821 s | EliteSQL 1.554x faster |

At 10M transactional rows, ingest wall alone is 7.207 s versus SQLite's
5.855 s (1.231x). SQLite then spends 1.838 s in its explicit checkpoint,
whereas EliteSQL's final checkpoint is 0.155 s; EliteSQL had already performed
3.943 s of checkpoint and 0.458 s of promotion work during ingest. The total
load comparison at 10M therefore still depends on SQLite's deferred flush
(2.626 s on 2026-09-04, 1.611 s on 2026-08-23) and should be read together
with the ingest-wall ratio, which moved from 1.202x to 1.231x within the
run-to-run spread observed earlier in the day (7.03-10.07 s across three
repetitions of this load on the committed tree).

Warm point reads over 10M rows measured 2.524 us (EliteSQL) versus 34.347 us
(SQLite); the direct bulk layout measured 2.338 us versus 11.580 us. The
unindexed full scan measured 0.167 s versus 0.568 s on the transactional
layout and 0.178 s versus 0.347 s on the bulk layout. The 10M point-read and
scan figures vary between repetitions with the timing of the background level
promotion relative to the read phase (31.0 us on 2026-09-04); they are
reported as measured.

A separate EliteSQL-only 10M load under `/usr/bin/time -l` (1K point reads, one
full scan) completed in 10.17 s wall with a maximum resident set size of
1,111,916,544 bytes and a peak physical footprint of 320,897,648 bytes.

## Small transactions

The fixed sustained workload (1M explicit-ID rows in 1,000-row transactions,
Fast/OFF, median of five fresh runs) measured:

| Sustained 1M-row workload | EliteSQL | SQLite | Result |
|---|---:|---:|---:|
| Ingest wall | 0.760 s | 0.621 s | EliteSQL 1.224x SQLite time |
| Final checkpoint | 0.096 s | 0.123 s | EliteSQL 1.28x faster |
| Total load | 0.855 s | 0.745 s | EliteSQL 1.148x SQLite time |
| Throughput | 1,168,947 rows/s | 1,342,804 rows/s | EliteSQL 12.9% lower |

## Concurrent writers

Each Fast/Balanced point is the median of three 200K-row runs. Each Safe point
is the median of three 40K-row runs; SQLite strict enables both `fullfsync` and
`checkpoint_fullfsync` and verifies `F_FULLFSYNC`.

| Writers | Fast Elite/SQLite | Balanced Elite/SQLite | Safe Elite/SQLite strict | Safe commits/sync | Safe p99 |
|---:|---:|---:|---:|---:|---:|
| 1 | 938,739 / 440,937 | 787,212 / 440,132 | 2,673 / 2,661 | 1.00 | 4.28 ms |
| 2 | 708,305 / 400,221 | 593,407 / 353,052 | 3,082 / 2,662 | 1.09 | 8.20 ms |
| 4 | 676,083 / 326,806 | 566,279 / 321,089 | 9,847 / 2,596 | 4.00 | 5.08 ms |
| 8 | 830,916 / 255,612 | 717,022 / 307,631 | 19,119 / 2,654 | 7.98 | 5.15 ms |
| 16 | 974,657 / 160,888 | 854,570 / 174,278 | 34,733 / 2,579 | 15.87 | 5.77 ms |

Fast beat SQLite by 1.77-6.06x and Balanced by 1.68-4.90x, within the spread
of the two previous matrices (Fast 1.82-5.06x and Balanced 1.45-4.48x on
2026-09-04). Safe strict reached 1.00x/1.16x/3.79x/7.20x/13.47x; the
two-writer point grouped 1.09 commits per sync in this run against 1.64 on
2026-09-04 and 1.03 on 2026-08-23, so that point remains sensitive to how the
two writers interleave. At 16 writers EliteSQL is 13.47x the like-for-like
strict SQLite result.

The WAL preallocation probe completed five paired repetitions. Median p50 was
3.982 ms while growing and 3.980 ms with 64 MiB preallocated, a difference
inside the spread of the individual repetitions.

## Concurrent reads and contention

The complete 100K-fixture, 1M-read matrix peaked at 3,548,239 reads/s with 16
readers (3,173,633 with eight, 2,112,103 with four). With 16 readers and four
writers it sustained 2,620,233 reads/s and 104,809 writes/s, with reader/writer
p99 of 65.25/341.67 us (2,526,321 / 101,053 and 70.54/486.88 us on
2026-09-04). Point reads now release the shared state lock before decoding the
record, so committers waiting for the write lock queue behind a shorter
critical section.

At 16 readers and four writers, the warm mutation-profile medians were:

| Profile | Reads/s | Writes/s | Reader p99 | Writer p99 |
|---|---:|---:|---:|---:|
| Insert | 2,277,265 | 113,863 | 84.25 us | 390.46 us |
| Update | 1,891,866 | 94,593 | 115.79 us | 0.961 ms |
| Delete | 1,880,747 | 94,037 | 112.96 us | 1.461 ms |
| Identity | 1,413,835 | 70,692 | 162.62 us | 0.806 ms |
| Foreign key | 1,699,014 | 84,951 | 117.04 us | 0.684 ms |
| Derived indexes | 930,363 | 46,518 | 290.54 us | 1.490 ms |

Read throughput rose in every profile except foreign key (unchanged) against
2026-09-04, and writer p99 fell for insert, update, identity, foreign key and
derived indexes. The delete profile's writer p99 rose from 0.954 to 1.461 ms
while its reopened-cache run measured 0.937 ms, so that value is inside the
profile's own warm/reopened spread rather than a trend. On macOS, `cold`
closes/reopens without warmup but cannot evict the OS page cache
(`evict=0/0`), so it validates reopen behavior rather than true cold I/O.

## Criterion, SQL and ANN

Central Criterion estimates:

- Matched 1K-row transaction: EliteSQL 3.052 ms, SQLite 0.616 ms.
- Autocommit 1K rows: EliteSQL 11.221 ms, SQLite 8.351 ms.
- Warmed steady 1K-row transactions: EliteSQL 0.796 ms (generated ids),
  0.884 ms (explicit ids), SQLite 0.698 ms. The EliteSQL intervals remain
  broad (0.67-0.93 ms and 0.75-1.01 ms); the fixed five-run workload above is
  the primary transaction result.
- Primary-key read: EliteSQL 0.318 us, SQLite 1.385 us (4.35x faster).
- SQL point literal/bound: 4.061/3.447 us.
- SQL indexed join literal/bound: 155.58/153.05 us (187.05/184.73 us on
  2026-09-04).
- SQL unindexed filter with `LIMIT 5` over 1M rows: 0.309 ms (34.23 ms on
  2026-09-04): the scan batch is trimmed to the rows the bounded query still
  needs.
- SQL `GROUP BY` over 1M orders, new in this run: 310.34 ms for 997 groups
  and 340.47 ms for 10K groups (845.08 and 940.49 ms on the committed tree
  with the same benchmark functions).

Synthetic ANN uses 100K deterministic 64-dimensional vectors, `M=16` and
`ef_construction=200`:

| `ef_search` | Recall@10 | Mean search |
|---:|---:|---:|
| 64 | 0.9520 | 0.210 ms |
| 128 | 0.9940 | 0.315 ms |
| 256 | 0.9980 | 0.591 ms |
| 512 | 1.0000 | 1.084 ms |

Indexed ingest took 8.830 s and opening the persisted graph took 74.5 ms
(9.198 s and 10.937 s on 2026-09-04; that open was a full rebuild because the
published runs were not kept on disk). Recall is identical to the previous two
runs at every `ef_search`, so every quality gate passed with the same margins;
searches are 11-24% faster than on 2026-09-04 from prefetching neighbour
vectors during the beam search.

## Artifact inventory

- `current-2026-09-05-scale-default-{1m,10m}.csv`
- `current-2026-09-05-scale-bulk-{1m,10m}.csv`
- `current-2026-09-05-small-transactions-fixed.csv` (five concatenated runs)
- `current-2026-09-05-concurrent-writers-{fast,balanced,safe-strict}.csv`
- `current-2026-09-05-wal-preallocation.csv`
- `current-2026-09-05-concurrent-rw.csv`
- `current-2026-09-05-contention-matrix.csv`
- `current-2026-09-05-criterion.csv`
- `current-2026-09-05-ann.csv`
- `concurrent-{throughput,p99-latency,max-latency}.svg`, regenerated from the
  Fast writer CSV with `scripts/plot-concurrent-benchmark.py`

Criterion's full sample distributions and estimates are retained under
`target/criterion/*/current-2026-09-05/` (`current-small-txn-2026-09-05/` for
`vs_sqlite`); the CSV above preserves the central estimates and confidence
intervals in the repository results directory.
