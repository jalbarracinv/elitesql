# Full acceptance run — 2026-08-23

This file records the complete local benchmark rerun requested after the
writer-admission work. It is additive: no historical CSV or table is replaced.

## Environment

- Host: `Mac17,3`, Apple M5, 10 logical CPUs, 16 GiB RAM
- OS: Darwin 25.5.0, arm64
- Rust/Cargo: 1.93.1
- EliteSQL base commit: `b09de5b`, measured with the documented dirty worktree
- SQLite: bundled 3.45.0 through rusqlite 0.31
- Power at start: battery 100%, Low Power Mode disabled
- Start load averages: 1.62 / 1.76 / 2.01
- Runs are sequential; no benchmark is intentionally run in parallel.
- macOS cannot use the harness's `POSIX_FADV_DONTNEED` path. `cold` matrix
  rows therefore mean reopen/no warmup and report `evict=0/0`, not a verified
  OS-cold page cache.

## Historical baselines retained

- `scale-2026-08-08.csv`, `scale-current-2026-08-08.csv`,
  `scale-optimized-2026-08-08.csv`
- `scale-relational-compat-2026-08-09.csv`
- `concurrent-writers-2026-08-08.csv`,
  `concurrent-writers-relational-compat-2026-08-09.csv`
- `concurrent-writers-mutex-baseline-2026-08-23.csv`,
  `concurrent-writers-fair-2026-08-23.csv`,
  `concurrent-writers-coordinator-2026-08-23.csv`
- `concurrent-rw-2026-08-23.csv`,
  `concurrent-rw-admission-comparison-2026-08-23.csv`
- `contention-matrix-2026-08-23.csv`

## Current-run scope

Status: measurements complete and verified. The ANN recall regression and the
Safe comparison/throughput issue were remediated and remeasured after the full
run. Persisted ANN reopen remains an unresolved negative result.

- Scale versus SQLite: 1M and 10M, transactional current default profile;
  legacy 128/24/32/16 MiB comparison profile; direct sorted bulk load.
- Criterion microbenchmarks: 1K inserts/10K point reads and SQL over 1M rows.
- Concurrent writers: 1/2/4/8/16 writers, Fast/Balanced/Safe, three fresh
  200K-row runs per engine and writer count.
- Concurrent readers/writers: 1/2/4/8/16 readers crossed with 0/1/4 writers,
  three fresh runs, 1M point reads and 40K writes per mixed run.
- Contention shapes: insert/update/delete/identity/FK/derived indexes,
  warm/reopened, three fresh runs with 16 readers and four writers.
- Synthetic ANN: 100K vectors, dimension 64, recall@10 and latency at
  `ef_search=64/128/256/512`, plus persisted reopen time.

Structured files use the `full-2026-08-23-*` prefix. Criterion retains a
`full-2026-08-23` saved baseline under `target/criterion`; its summarized
measurements will also be copied into this document so comparisons do not
depend on build artifacts.

## Scale versus SQLite

All rows use Fast/OFF durability, 10K-row transactions, 10K point reads and
three unindexed scans. `SQLite / EliteSQL` above 1 means EliteSQL completed the
end-to-end load faster.

| Profile | Rows | EliteSQL total | SQLite total | SQLite / EliteSQL | EliteSQL rows/s | SQLite rows/s |
|---|---:|---:|---:|---:|---:|---:|
| Current default 384 MiB | 1M | 0.947 s | 0.700 s | 0.739x | 1,055,749 | 1,428,016 |
| Current default 384 MiB | 10M | 8.643 s | 7.595 s | 0.879x | 1,157,002 | 1,316,714 |
| Historical 128/24/32/16 MiB | 1M | 1.669 s | 0.711 s | 0.426x | 599,085 | 1,405,647 |
| Historical 128/24/32/16 MiB | 10M | 16.433 s | 7.572 s | 0.461x | 608,532 | 1,320,718 |
| Direct sorted bulk | 1M | 0.573 s | 0.707 s | 1.234x | 1,745,460 | 1,414,030 |
| Direct sorted bulk | 10M | 5.235 s | 8.260 s | 1.578x | 1,910,182 | 1,210,711 |

The like-for-like 128 MiB historical run changed from 24.049 to 16.433 s for
EliteSQL (-31.7%), while SQLite changed from 15.670 to 7.572 s (-51.7%). The
absolute EliteSQL result improved, but its ratio moved from 1.535x to 2.170x
SQLite time. This large movement in both engines is why the old data remains a
separate baseline instead of being overwritten.

Raw data:

- `full-2026-08-23-scale-default-1m.csv`
- `full-2026-08-23-scale-default-10m.csv`
- `full-2026-08-23-scale-legacy128-1m.csv`
- `full-2026-08-23-scale-legacy128-10m.csv`
- `full-2026-08-23-scale-bulk-1m.csv`
- `full-2026-08-23-scale-bulk-10m.csv`

## Criterion microbenchmarks and SQL

Values are Criterion means; confidence intervals and medians are retained in
`full-2026-08-23-criterion.csv`.

| Microbenchmark | EliteSQL | SQLite | Result |
|---|---:|---:|---:|
| 1K inserts, autocommit | 12.562 ms | 9.399 ms | EliteSQL 1.34x SQLite time |
| 1K inserts, one transaction | 3.821 ms | 0.663 ms | EliteSQL 5.76x SQLite time |
| Primary-key read | 0.347 us | 1.381 us | EliteSQL 3.99x faster |

| SQL over 1M orders | Current mean | Historical mean | Change |
|---|---:|---:|---:|
| Unique-index point, literal | 4.769 us | 4.050 us | +17.8% |
| Unique-index point, bound | 4.026 us | 4.019 us | +0.2% |
| Indexed join, literal | 228.951 us | 250.000 us | -8.4% |
| Indexed join, bound | 226.904 us | 230.680 us | -1.6% |
| Unindexed filter | 289.526 ms | 264.030 ms | +9.7% |

## Concurrent writers

Each cell is the median of three fresh 200K-row runs. Throughput ratios above
1 favor EliteSQL. p99 values are transaction latency in microseconds.

| Durability | Writers | EliteSQL rows/s | SQLite rows/s | Ratio | EliteSQL p99 | SQLite p99 |
|---|---:|---:|---:|---:|---:|---:|
| Fast/OFF | 1 | 813,464 | 419,243 | 1.94x | 17.3 | 50.3 |
| Fast/OFF | 2 | 701,572 | 399,192 | 1.76x | 39.0 | 49.8 |
| Fast/OFF | 4 | 709,706 | 323,870 | 2.19x | 84.6 | 56.0 |
| Fast/OFF | 8 | 791,457 | 267,873 | 2.95x | 138.9 | 56.7 |
| Fast/OFF | 16 | 917,197 | 161,057 | 5.69x | 292.2 | 65.8 |
| Balanced/NORMAL | 1 | 704,529 | 439,945 | 1.60x | 18.2 | 48.0 |
| Balanced/NORMAL | 2 | 600,278 | 373,836 | 1.61x | 41.5 | 52.9 |
| Balanced/NORMAL | 4 | 606,233 | 335,702 | 1.81x | 89.5 | 54.2 |
| Balanced/NORMAL | 8 | 689,535 | 265,076 | 2.60x | 148.4 | 56.5 |
| Balanced/NORMAL | 16 | 775,203 | 161,141 | 4.81x | 327.0 | 64.3 |
| Safe/FULL | 1 | 2,539 | 168,187 | 0.02x | 4,219.5 | 189.9 |
| Safe/FULL | 2 | 4,971 | 179,423 | 0.03x | 5,211.5 | 92.1 |
| Safe/FULL | 4 | 9,911 | 153,007 | 0.06x | 6,599.2 | 105.0 |
| Safe/FULL | 8 | 18,825 | 131,491 | 0.14x | 8,020.6 | 100.8 |
| Safe/FULL | 16 | 31,157 | 119,184 | 0.26x | 11,583.2 | 101.8 |

Fast and Balanced win aggregate throughput at every writer count. Safe is a
clear regression target: single-writer commits pay roughly one 4 ms physical
sync apiece. Group commit reduces 20,000 physical syncs to about 1,536 at 16
writers, but Safe remains 3.8x slower than SQLite FULL even there. SQLite's
median maximum transaction wait grows to 1.2-1.6 seconds at 16 writers, while
EliteSQL's median maximum is 0.47 ms Fast, 5.42 ms Balanced and 21.86 ms Safe.

Raw data:

- `full-2026-08-23-concurrent-writers-fast.csv`
- `full-2026-08-23-concurrent-writers-balanced.csv`
- `full-2026-08-23-concurrent-writers-safe.csv`

### Post-run Safe correction and remediation

The table above is retained as the original full run, but its Safe comparison
is not like-for-like on macOS. EliteSQL's Rust `File::sync_data` maps to
`F_FULLFSYNC`; SQLite `synchronous=FULL` controls when xSync runs but uses
ordinary `fsync` unless `PRAGMA fullfsync=ON`. The corrected harness records
the primitive and verifies `fullfsync` plus `checkpoint_fullfsync` on every
connection.

Eligible Safe inserts now use the existing independent-frame commit
coordinator. All batch members wait for one strict barrier; complex
transactions retain the general path. Median results from three fresh 40K-row
runs (10 rows/transaction) were:

| Writers | EliteSQL Safe | SQLite `F_FULLFSYNC` | EliteSQL / SQLite | Commits/sync |
|---:|---:|---:|---:|---:|
| 1 | 2,516 rows/s | 2,516 rows/s | 1.00x | 1.00 |
| 2 | 5,037 rows/s | 2,513 rows/s | 2.00x | 2.00 |
| 4 | 10,018 rows/s | 2,506 rows/s | 4.00x | 4.00 |
| 8 | 19,326 rows/s | 2,537 rows/s | 7.62x | 7.95 |
| 16 | 36,738 rows/s | 2,633 rows/s | 13.95x | 15.69 |

The single writer remains flush-physics-bound. At 16 writers the change raised
EliteSQL's median from 31,157 to 36,738 rows/s (+17.9%) and reduced p99 from
11.58 to 8.23 ms. Sync-failure fanout, reopen, concurrent checkpoints and
kill/recovery passed. Fast/Balanced confirmation runs stayed within the 3%
acceptance envelope; the focused seven-run Fast/4-writer median was 0.8% below
history. WAL preallocation was rejected after a five-repeat strict-sync probe
showed no stable benefit.

Raw data: `full-2026-08-23-concurrent-writers-safe-strict.csv`,
`safe-delay-{0,50,100,200,500}us-2026-08-23.csv`,
`safe-batch-{1,100,1000}-2026-08-23.csv`, and
`wal-preallocation-2026-08-23.csv`.

## Persisted concurrent readers and writers

Each row is the median of three runs with 1M point reads. Mixed runs add 40K
inserts in 10-row transactions. Latencies are p99 in microseconds.

| Readers | Writers | Reads/s | Writes/s | Read p99 | Write p99 | Throttled reads |
|---:|---:|---:|---:|---:|---:|---:|
| 1 | 0 | 755,720 | 0 | 1.6 | 0.0 | 0 |
| 2 | 0 | 771,473 | 0 | 3.5 | 0.0 | 0 |
| 4 | 0 | 1,163,168 | 0 | 5.5 | 0.0 | 0 |
| 8 | 0 | 1,147,572 | 0 | 9.8 | 0.0 | 0 |
| 16 | 0 | 1,125,929 | 0 | 12.9 | 0.0 | 0 |
| 1 | 1 | 718,787 | 28,751 | 2.1 | 41.8 | 0 |
| 2 | 1 | 749,534 | 29,981 | 8.7 | 115.4 | 0 |
| 4 | 1 | 1,018,558 | 40,742 | 22.1 | 162.3 | 0 |
| 8 | 1 | 983,444 | 39,338 | 70.7 | 363.1 | 0 |
| 16 | 1 | 899,467 | 35,979 | 118.1 | 402.3 | 42,132 |
| 1 | 4 | 725,170 | 29,007 | 2.3 | 126.5 | 0 |
| 2 | 4 | 773,662 | 30,946 | 4.0 | 208.6 | 0 |
| 4 | 4 | 1,107,839 | 44,314 | 14.6 | 347.9 | 0 |
| 8 | 4 | 1,041,673 | 41,667 | 38.7 | 519.0 | 16,770 |
| 16 | 4 | 1,027,846 | 41,114 | 95.5 | 427.3 | 28,853 |

The complete 16-reader/four-writer run confirms the focused result: writer p99
is 0.427 ms rather than the 11.147 ms pre-admission baseline. Read-only runs
never throttle.

Raw data: `full-2026-08-23-concurrent-rw.csv`.

## Mutation and derived-index contention

Each row is the median of three runs with 16 readers, four writers, 100K point
reads and 5K mutations. `reopened` is the harness's `cold` mode on this macOS
host and is not proof of an OS-cold page cache.

| Workload | Cache | Reads/s | Writes/s | Read p99 us | Write p99 us |
|---|---|---:|---:|---:|---:|
| Insert | warm | 971,108 | 48,555 | 120.4 | 470.8 |
| Insert | reopened | 951,074 | 47,554 | 115.5 | 919.5 |
| Update | warm | 830,082 | 41,504 | 180.1 | 1,479.5 |
| Update | reopened | 860,435 | 43,022 | 182.2 | 1,512.2 |
| Delete | warm | 864,214 | 43,211 | 170.7 | 1,207.5 |
| Delete | reopened | 894,840 | 44,742 | 157.3 | 1,260.0 |
| Identity | warm | 635,016 | 31,751 | 261.6 | 1,346.9 |
| Identity | reopened | 655,891 | 32,795 | 250.8 | 1,197.0 |
| Foreign key | warm | 877,693 | 43,885 | 156.0 | 876.2 |
| Foreign key | reopened | 934,197 | 46,710 | 137.2 | 685.1 |
| Derived indexes | warm | 571,102 | 28,555 | 377.9 | 1,629.8 |
| Derived indexes | reopened | 619,045 | 30,952 | 351.5 | 1,051.8 |

Raw data: `full-2026-08-23-contention-matrix.csv`.

## Synthetic ANN, 100K vectors

| `ef_search` | Current recall@10 | Historical recall@10 | Current mean | Historical mean |
|---:|---:|---:|---:|---:|
| 64 | 0.8500 | 0.9320 | 0.188 ms | 1.235 ms |
| 128 | 0.9520 | 0.9940 | 0.320 ms | 1.998 ms |
| 256 | 0.9940 | 1.0000 | 0.574 ms | 3.368 ms |
| 512 | 0.9980 | 1.0000 | 1.057 ms | 5.553 ms |

Search is 5.3-6.6x faster, but recall regressed, especially at ef=64/128.
That is a quality regression and must not be described as a free throughput
win. Persisted reopen also regressed from 15.618 to 22.373 seconds (+43.3%).
The existing acceptance assertion (`ef=128 >= 0.85`) passes, but the historical
quality level is the relevant acceptance target; this run therefore fails
acceptance.

A same-machine rerun reproduced the current recall exactly. Rebuilding and
running historical commit `a8f759c` with the current compiler reproduced its
published quality within 0.004 (`0.928/0.990/1.000/1.000`), so this is neither
query randomness nor an unreliable historical number. The first bad commit is
`e5d6638` (`0.762/0.878/0.970/1.000`), which enlarged the memory defaults and
changed the HNSW generation boundaries. `b09de5b` partially recovered quality
through background generation publication, but did not restore the historical
curve. The same `ef_search` was effectively doing less aggregate graph work.

Diagnostics with the current checkpointed layout found that `M=32` reached
`0.942/0.988/1.000/1.000`, while `M=48` reached
`0.960/0.998/1.000/1.000`. At `M=48`, mean search intervals were
0.465/0.805/1.385/2.327 ms, still below the historical values. This is not yet
adopted: larger `M` raises construction work, mutable memory and persisted graph
size, and those write-side costs need a complete comparison first. Structured
history and all diagnostic profiles are retained in
`ann-quality-history-2026-08-23.csv`.

### Post-run ANN remediation

Doubling `M` was rejected after measurement: `M=32` doubled indexed ingestion
from 15.129 to 30.080 seconds and increased persisted reopen to 38.634 seconds.
Instead, the internal beam is now calibrated to twice the requested public
search effort, compensating for the reduced aggregate exploration without
changing HNSW construction, graph size or write throughput.

| Requested `ef_search` | Restored recall@10 | Restored mean | Historical recall@10 | Historical mean |
|---:|---:|---:|---:|---:|
| 64 | 0.9520 | 0.327 ms | 0.9320 | 1.235 ms |
| 128 | 0.9940 | 0.579 ms | 0.9940 | 1.998 ms |
| 256 | 0.9980 | 1.046 ms | 1.0000 | 3.368 ms |
| 512 | 1.0000 | 1.774 ms | 1.0000 | 5.553 ms |

The tightened historical-quality gate passes. Indexed ingestion stayed at
15.121 seconds versus the 15.129-second uncalibrated control because this
change affects searches only. Persisted reopen was 22.080 seconds: slightly
better than the failed full-run value, but still 41.4% above the historical
15.618 seconds and therefore still a follow-up target.

## Post-run verification

- All 12 structured current-run CSVs parsed successfully, contained their
  expected 198 data rows in total, and contained no NaN/infinite values.
- `cargo clippy --workspace --all-targets -- -D warnings`: passed.
- `cargo test --workspace`: passed (359 tests including doc tests).
- `cargo bench -p elitesql-core --bench vector -- --save-baseline
  ann-quality-restored-2026-08-23`: passed the tightened recall gate.
- `cargo fmt --all -- --check` and `git diff --check`: passed.
- End state: battery 100%, Low Power Mode disabled; load averages
  2.91 / 2.88 / 2.75 immediately after the test suite.
