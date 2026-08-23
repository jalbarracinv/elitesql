# EliteSQL current acceptance run — 2026-08-23

This is the immutable summary for the complete sequential rerun whose raw
artifacts use the `current-2026-08-23-*` prefix. It measures the dirty worktree
on top of commit `b09de5b`; the commit alone does not reproduce the results.

## Environment

- MacBook Air, Apple M5, 10 logical CPUs, 16 GiB RAM
- macOS 26.5.2 (25F84), arm64
- Rust/Cargo 1.93.1, release benchmark profile
- SQLite 3.45.0 through the bundled `rusqlite` build
- AC power, battery 100%; benchmarks executed sequentially
- EliteSQL default memory profile: 384 MiB

## Scale

| Path | Rows | EliteSQL total | SQLite total | Result |
|---|---:|---:|---:|---:|
| Transactional | 1M | 0.963 s | 0.718 s | 1.342x SQLite time |
| Transactional | 10M | 8.623 s | 7.615 s | 1.132x SQLite time |
| Direct sorted bulk | 1M | 0.579 s | 0.741 s | EliteSQL 1.280x faster |
| Direct sorted bulk | 10M | 5.190 s | 8.513 s | EliteSQL 1.640x faster |

At 10M transactional rows, ingest wall alone is 8.510 s versus SQLite's
6.004 s (1.417x). SQLite then spends 1.611 s in its explicit checkpoint,
whereas EliteSQL's final checkpoint is 0.113 s; EliteSQL had already performed
3.746 s of checkpoint and 0.446 s of promotion work during ingest.

## Concurrent writers

Each Fast/Balanced point is the median of three 200K-row runs. Each Safe point
is the median of three 40K-row runs; SQLite strict enables both `fullfsync` and
`checkpoint_fullfsync` and verifies `F_FULLFSYNC`.

| Writers | Fast Elite/SQLite | Balanced Elite/SQLite | Safe Elite/SQLite strict | Safe commits/sync | Safe p99 |
|---:|---:|---:|---:|---:|---:|
| 1 | 822,577 / 445,393 | 711,126 / 438,676 | 2,518 / 2,517 | 1.00 | 4.15 ms |
| 2 | 703,584 / 374,428 | 604,841 / 406,949 | 2,596 / 2,509 | 1.03 | 8.24 ms |
| 4 | 695,985 / 327,307 | 612,479 / 327,989 | 9,804 / 2,616 | 3.99 | 5.07 ms |
| 8 | 755,724 / 266,866 | 643,297 / 267,526 | 19,221 / 2,683 | 7.97 | 5.14 ms |
| 16 | 889,721 / 175,264 | 749,281 / 174,811 | 35,910 / 2,605 | 15.69 | 9.39 ms |

The two-writer Safe point did not form useful groups in this run and has about
twice the single-writer transaction latency. From four writers onward the
coordinator groups nearly all available commits; at 16 writers EliteSQL is
13.79x the like-for-like strict SQLite result.

The WAL preallocation probe also completed five paired repetitions. Median p50
was 3.975 ms while growing and 3.983 ms with 64 MiB preallocated; the current
machine therefore shows no strict-sync benefit from reserving the file.

## Concurrent reads and contention

The complete 100K-fixture, 1M-read matrix peaked at 1,216,481 reads/s with four
readers. With 16 readers and four writers it sustained 1,031,466 reads/s and
41,259 writes/s, with reader/writer p99 of 91.96/424.58 us.

At 16 readers and four writers, the warm mutation-profile medians were:

| Profile | Reads/s | Writes/s | Reader p99 | Writer p99 |
|---|---:|---:|---:|---:|
| Insert | 988,867 | 49,443 | 117.38 us | 424.54 us |
| Update | 843,696 | 42,185 | 179.46 us | 1.363 ms |
| Delete | 890,002 | 44,500 | 170.08 us | 2.418 ms |
| Identity | 659,171 | 32,959 | 271.96 us | 1.258 ms |
| Foreign key | 853,877 | 42,694 | 162.54 us | 0.748 ms |
| Derived indexes | 563,235 | 28,162 | 389.33 us | 1.612 ms |

On macOS, `cold` closes/reopens without warmup but cannot evict the OS page
cache (`evict=0/0`), so it validates reopen behavior rather than true cold I/O.

## Criterion, SQL and ANN

Central Criterion estimates:

- Matched 1K-row transaction: EliteSQL 4.060 ms, SQLite 0.673 ms.
- Autocommit 1K rows: EliteSQL 13.397 ms, SQLite 9.372 ms.
- Primary-key read: EliteSQL 0.350 us, SQLite 1.394 us (3.98x faster).
- SQL point literal/bound: 4.706/4.029 us.
- SQL indexed join literal/bound: 233.30/228.41 us.
- SQL full scan/filter over 1M rows: 291.25 ms.

Synthetic ANN uses 100K deterministic 64-dimensional vectors, `M=16` and
`ef_construction=200`:

| `ef_search` | Recall@10 | Mean search |
|---:|---:|---:|
| 64 | 0.9520 | 0.326 ms |
| 128 | 0.9940 | 0.587 ms |
| 256 | 0.9980 | 1.066 ms |
| 512 | 1.0000 | 1.828 ms |

Indexed ingest took 15.347 s and persisted open took 22.606 s. Every recall
gate passed; the current values remain separate from earlier runs so small
quality variations are visible rather than overwritten.

## Artifact inventory

- `current-2026-08-23-scale-default-{1m,10m}.csv`
- `current-2026-08-23-scale-bulk-{1m,10m}.csv`
- `current-2026-08-23-concurrent-writers-{fast,balanced,safe-strict}.csv`
- `current-2026-08-23-wal-preallocation.csv`
- `current-2026-08-23-concurrent-rw.csv`
- `current-2026-08-23-contention-matrix.csv`
- `current-2026-08-23-criterion.csv`
- `current-2026-08-23-ann.csv`

Criterion's full sample distributions and estimates are retained under
`target/criterion/*/current-2026-08-23/`; the CSV above preserves the central
estimates and confidence intervals in the repository results directory.
