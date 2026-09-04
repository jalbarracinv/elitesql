# EliteSQL current acceptance run — 2026-09-04

This is the immutable summary for the complete sequential rerun whose raw
artifacts use the `current-2026-09-04-*` prefix. It measures the dirty worktree
on top of commit `fcdcde7` after the hot-path optimization pass (mapped segment
payload reads, encoded-payload equality scans, allocation-free primary merge,
vectorized HNSW kernels with incremental memory accounting, pre-sized record
encoding); the commit alone does not reproduce the results.

## Environment

- MacBook Air, Apple M5, 10 logical CPUs, 16 GiB RAM
- macOS 26.6.2 (25G83), arm64
- Rust/Cargo 1.93.1, release benchmark profile
- SQLite 3.45.0 through the bundled `rusqlite` build
- AC power, battery 98-100%; benchmarks executed sequentially from one script
  between 02:04 and 02:16 local time, none overlapping another measurement
- EliteSQL default memory profile: 384 MiB

## Scale

| Path | Rows | EliteSQL total | SQLite total | Result |
|---|---:|---:|---:|---:|
| Transactional | 1M | 0.832 s | 0.719 s | 1.157x SQLite time |
| Transactional | 10M | 7.330 s | 8.631 s | EliteSQL 1.178x faster |
| Direct sorted bulk | 1M | 0.525 s | 0.713 s | EliteSQL 1.358x faster |
| Direct sorted bulk | 10M | 4.559 s | 7.289 s | EliteSQL 1.599x faster |

At 10M transactional rows, ingest wall alone is 7.220 s versus SQLite's
6.005 s (1.202x). SQLite then spends 2.626 s in its explicit checkpoint,
whereas EliteSQL's final checkpoint is 0.111 s; EliteSQL had already performed
3.606 s of checkpoint and 0.433 s of promotion work during ingest. SQLite's
ingest wall matches the 2026-08-23 run to the millisecond (6.005 versus
6.004 s) while its checkpoint took 2.626 s instead of 1.611 s, so the total
load comparison at 10M is sensitive to that flush and should be read together
with the ingest-wall ratio.

Warm point reads over 10M rows measured 31.014 us (EliteSQL) versus 52.326 us
(SQLite); the direct bulk layout measured 2.187 us versus 38.026 us. The
unindexed full scan measured 0.690 s versus 0.736 s on the transactional
layout and 0.201 s versus 0.605 s on the bulk layout.

A separate EliteSQL-only 10M load under `/usr/bin/time -l` (1K point reads, one
full scan) completed in 7.83 s wall with a maximum resident set size of
1,102,577,664 bytes and a peak physical footprint of 315,687,584 bytes.

## Small transactions

The fixed sustained workload (1M explicit-ID rows in 1,000-row transactions,
Fast/OFF, median of five fresh runs) measured:

| Sustained 1M-row workload | EliteSQL | SQLite | Result |
|---|---:|---:|---:|
| Ingest wall | 0.802 s | 0.641 s | EliteSQL 1.251x SQLite time |
| Final checkpoint | 0.097 s | 0.128 s | EliteSQL 1.32x faster |
| Total load | 0.900 s | 0.769 s | EliteSQL 1.170x SQLite time |
| Throughput | 1,111,522 rows/s | 1,299,940 rows/s | EliteSQL 14.5% lower |

## Concurrent writers

Each Fast/Balanced point is the median of three 200K-row runs. Each Safe point
is the median of three 40K-row runs; SQLite strict enables both `fullfsync` and
`checkpoint_fullfsync` and verifies `F_FULLFSYNC`.

| Writers | Fast Elite/SQLite | Balanced Elite/SQLite | Safe Elite/SQLite strict | Safe commits/sync | Safe p99 |
|---:|---:|---:|---:|---:|---:|
| 1 | 908,883 / 433,806 | 738,731 / 434,920 | 2,954 / 2,858 | 1.00 | 4.25 ms |
| 2 | 725,236 / 398,894 | 583,086 / 401,658 | 4,319 / 2,845 | 1.64 | 8.04 ms |
| 4 | 681,175 / 325,371 | 574,563 / 283,223 | 9,960 / 2,760 | 4.00 | 4.94 ms |
| 8 | 759,007 / 264,338 | 677,290 / 266,580 | 19,829 / 2,749 | 8.00 | 5.00 ms |
| 16 | 889,489 / 175,909 | 789,294 / 176,202 | 36,045 / 2,826 | 15.75 | 6.09 ms |

Fast and Balanced are within the run-to-run spread of the 2026-08-23 matrix;
the optimizations in this pass target reads and index maintenance, not the
commit path. The two-writer Safe point now groups 1.64 commits per sync
(1.03 before) and reaches 1.52x the strict SQLite result. At 16 writers
EliteSQL is 12.75x the like-for-like strict SQLite result.

The WAL preallocation probe completed five paired repetitions. Median p50 was
3.869 ms while growing and 3.735 ms with 64 MiB preallocated, a 3.5%
difference that stays inside the spread of the individual repetitions.

## Concurrent reads and contention

The complete 100K-fixture, 1M-read matrix peaked at 3,583,376 reads/s with 16
readers (3,239,033 with eight, 1,934,787 with four). With 16 readers and four
writers it sustained 2,526,321 reads/s and 101,053 writes/s, with reader/writer
p99 of 70.54/486.88 us. Point reads no longer issue a `pread` system call per
record: the payload is borrowed from a read-only segment mapping, which is
what lets read throughput keep scaling past eight readers.

At 16 readers and four writers, the warm mutation-profile medians were:

| Profile | Reads/s | Writes/s | Reader p99 | Writer p99 |
|---|---:|---:|---:|---:|
| Insert | 2,105,668 | 105,283 | 92.58 us | 419.92 us |
| Update | 1,565,250 | 78,263 | 132.54 us | 1.085 ms |
| Delete | 1,725,273 | 86,264 | 120.71 us | 0.954 ms |
| Identity | 1,221,203 | 61,060 | 190.50 us | 0.963 ms |
| Foreign key | 1,702,481 | 85,124 | 120.79 us | 0.812 ms |
| Derived indexes | 909,027 | 45,451 | 297.17 us | 1.514 ms |

On macOS, `cold` closes/reopens without warmup but cannot evict the OS page
cache (`evict=0/0`), so it validates reopen behavior rather than true cold I/O.

## Criterion, SQL and ANN

Central Criterion estimates:

- Matched 1K-row transaction: EliteSQL 2.615 ms, SQLite 0.653 ms.
- Autocommit 1K rows: EliteSQL 10.682 ms, SQLite 8.524 ms.
- Warmed steady 1K-row transactions: EliteSQL 0.991 ms (generated ids),
  1.033 ms (explicit ids), SQLite 0.719 ms. The EliteSQL intervals remain
  broad (0.87-1.05 ms and 0.93-1.10 ms); the fixed five-run workload above is
  the primary transaction result.
- Primary-key read: EliteSQL 0.351 us, SQLite 1.404 us (4.00x faster).
- SQL point literal/bound: 4.357/3.656 us.
- SQL indexed join literal/bound: 187.05/184.73 us.
- SQL full scan/filter over 1M rows: 34.23 ms (291.25 ms on 2026-08-23).

Synthetic ANN uses 100K deterministic 64-dimensional vectors, `M=16` and
`ef_construction=200`:

| `ef_search` | Recall@10 | Mean search |
|---:|---:|---:|
| 64 | 0.9520 | 0.237 ms |
| 128 | 0.9940 | 0.416 ms |
| 256 | 0.9980 | 0.773 ms |
| 512 | 1.0000 | 1.353 ms |

Indexed ingest took 9.198 s and persisted open took 10.937 s (15.347 s and
22.606 s on 2026-08-23). Recall is identical to the previous run at every
`ef_search`, so every quality gate passed with the same margins.

## Artifact inventory

- `current-2026-09-04-scale-default-{1m,10m}.csv`
- `current-2026-09-04-scale-bulk-{1m,10m}.csv`
- `current-2026-09-04-small-transactions-fixed.csv` (five concatenated runs)
- `current-2026-09-04-concurrent-writers-{fast,balanced,safe-strict}.csv`
- `current-2026-09-04-wal-preallocation.csv`
- `current-2026-09-04-concurrent-rw.csv`
- `current-2026-09-04-contention-matrix.csv`
- `current-2026-09-04-criterion.csv`
- `current-2026-09-04-ann.csv`
- `concurrent-{throughput,p99-latency,max-latency}.svg`, regenerated from the
  Fast writer CSV with `scripts/plot-concurrent-benchmark.py`

Criterion's full sample distributions and estimates are retained under
`target/criterion/*/current-2026-09-04/`; the CSV above preserves the central
estimates and confidence intervals in the repository results directory.
