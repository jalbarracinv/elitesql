# EliteSQL benchmarks — 2026-09-11

EliteSQL's current transactional path reaches **1.38–1.61 million rows/s** at 10M rows, delivering **23.9–26.8% more throughput than SQLite** in the measured one-writer workloads. At 1M rows the engines are effectively tied with 1K-row transactions, while EliteSQL leads by 1.7% with 10K-row transactions. These results use normal transactions, not `bulk_insert_sorted`, and include the final checkpoint and maintenance drain.

## Transactional ingest optimization

### Headline result

Each cell contains independent processes; brackets are minimum–maximum, not confidence intervals. Every workload has five final repetitions, except 10M/10K, which has ten because its first five-run prefix exposed large asymmetric WAL-write variation.

| Rows / transaction | EliteSQL total s [range] | SQLite total s [range] | EliteSQL rows/s | Result vs SQLite |
| --- | ---: | ---: | ---: | ---: |
| 1M / 1K | **0.759** [0.745–0.816] | 0.754 [0.743–0.784] | 1,317,051 | Effective parity (−0.7%) |
| 1M / 10K | **0.692** [0.688–1.004] | 0.703 [0.698–0.724] | 1,445,309 | **EliteSQL +1.7%** |
| 10M / 1K | **7.263** [6.908–8.167] | 9.208 [9.018–10.623] | 1,376,791 | **EliteSQL +26.8%** |
| 10M / 10K | **6.208** [5.815–9.387] | 7.689 [6.797–8.137] | 1,613,375 | **EliteSQL +23.9%** |

Percentages compare throughput for the same row count. The optimization comparison against the pre-change binary remains available in the [technical report](benchmark-results/transaction-ingest-2026-09-11/README.md), rather than occupying the product-facing headline. For 10M/10K, ten alternating repetitions were retained because the first five exposed substantial WAL-write variability.

### Non-overlapping instrumentation and profile

[Scale harness](crates/elitesql-core/benches/scale_vs_sqlite.rs) now separates record/value construction, staging/SQLite execution and commit calls. EliteSQL exposes exclusive commit counters for preparation, canonical record encoding, validation, WAL encoding, WAL append, durability sync wait, MVCC application and foreground maintenance wait. Background checkpoint/promotion work remains a separate accumulated counter because it can overlap ingest. Across all raw final runs, the exclusive phase sum never exceeded commit-call wall time; its maximum ratio was 92.5%.

| 10M phase | 1K initial s | 1K final s | Change | 10K initial s | 10K final s | Change |
| --- | ---: | ---: | ---: | ---: | ---: | ---: |
| staging | 1.995 | 1.425 | −28.6% | 1.894 | 1.300 | −31.4% |
| prepare/order | 0.111 | 0.043 | −61.5% | 0.114 | 0.044 | −61.1% |
| record encode | 0.748 | 0.544 | −27.2% | 0.769 | 0.543 | −29.4% |
| validation | 0.108 | 0.105 | −2.4% | 0.098 | 0.100 | +2.1% |
| WAL encode | 0.427 | 0.459 | +7.5% | 0.416 | 0.432 | +4.0% |
| WAL append | 1.906 | 1.675 | −12.1% | 1.747 | 0.832 | −52.4% |
| sync wait (Fast) | 0.000 | 0.000 | — | 0.000 | 0.000 | — |
| MVCC apply | 1.913 | 0.866 | −54.7% | 1.870 | 0.844 | −54.9% |
| maintenance wait | 0.0003 | 0.0003 | ~0% | 0.0002 | 0.0002 | ~0% |
| commit calls | 5.658 | 4.276 | −24.4% | 5.677 | 3.442 | −39.4% |

WAL append wrote identical bytes, so its variable improvement is not attributed to the implementation. The CPU sample found primary B-tree insertion/comparison, allocation/freeing of per-row intermediate state, record encoding, CRC, memory copies and `write(2)` as the dominant active stacks. This evidence led to three retained changes: append-oriented transaction staging with a general hash fallback, direct encoding into a transaction-owned shared payload arena, and an append-oriented primary MVCC delta with a B-tree fallback. Persisted formats and the WAL encoding are unchanged. A `SmallVec` version-list candidate was rejected: it saved calls but raised process footprint for negligible latency benefit.

### Allocations, bytes and memory

The allocation counter is a separate diagnostic binary; its overhead is not mixed into normal latency. Three 10M/10K processes per version give:

| Metric | Initial | Final | Change |
| --- | ---: | ---: | ---: |
| allocation calls | 141,870,849 | 120,209,959 | −15.3% |
| commit allocation calls | 31,780,987 | 10,110,809 | −68.2% |
| cumulative allocated bytes | 23,155,193,871 | 23,239,006,257 | +0.36% |
| commit allocated bytes | 11,474,442,000 | 11,158,566,197 | −2.75% |
| peak live allocator bytes | 313,996,986 | 228,188,791 | −27.3% |
| live allocator bytes at end | 1,110,176 | 1,110,176 | unchanged |
| logical index-delta peak | 118,788,890 | 118,788,890 | unchanged |
| logical maintenance peak | 134,217,728 | 134,217,728 | unchanged |

At 10M/10K, both versions append 1,598,904,890 WAL bytes, write 559,191,396 checkpoint bytes and read/write 366,829,504/366,828,664 promotion bytes; final on-disk sizes also match. No memory budget or threshold changed.

Process footprint did not follow live heap. At 1M, final peak RSS/physical footprint fell from 393/263 MiB to 316/163 MiB (1K) and from 449/309 MiB to 306/213 MiB (10K). At 10M it rose from 997/263 MiB to 1,198/520 MiB (1K) and from 1,060/312 MiB to 1,294/580 MiB (10K). These whole-process peaks include allocator retention and mapped pages touched by maintenance, validation and the final scan. The unchanged governor peaks and lower counted live heap show that no configured budget was raised, but the observed 10M footprint regression remains a limitation.

### Correctness, durability, concurrency and queries

The retained source passed `cargo fmt --all --check`, Clippy with `-D warnings`, the complete locked Rust workspace suite, eight Python tests and both Node suites. The Rust tests cover atomicity, snapshot isolation, conflicts, statement rollback, uniqueness, foreign keys/cascades, identities, memory rejection, checkpoint/compaction, WAL corruption/torn tails, kill-9 recovery, DDL crash recovery, vector crash recovery and all durability modes.

The exact final writer binary was measured with three repetitions, batch size 10 and one/four writers. EliteSQL medians were 1.013M/0.707M rows/s in Fast, 0.896M/0.587M in Balanced, and 2,610/10,328 rows/s in Safe. Safe verified `F_FULLFSYNC`: one commit per sync with one writer and four per sync with four. The corresponding strict SQLite medians were 2,791/2,752 rows/s. Full rows, p99, maxima, sync counts and group sizes are in the raw CSVs.

Three fresh 1M-row Criterion processes found no query regression over 3% versus the pre-optimization medians: point unique −1.3%, bound point −1.5%, indexed join −3.0%, bound join −1.2%, LIMIT scan −1.6%, GROUP BY 997 groups −0.7%, and GROUP BY 10K groups +0.01%. The smaller scale sample's 1M/1K direct-get median moved from 1.284 to 1.351 µs (+5.2%), but its ranges overlap and the higher-sample Criterion point queries improved; this is retained as a noise-sensitive observation, not hidden as a pass.

### Exact evidence, reproduction and limitations

- [Full report and artifact map](benchmark-results/transaction-ingest-2026-09-11/README.md).
- [Machine-readable distributions](benchmark-results/transaction-ingest-2026-09-11/summary.json), [exact alternating matrix](benchmark-results/transaction-ingest-2026-09-11/run_matrix.sh), [allocation runner](benchmark-results/transaction-ingest-2026-09-11/run_allocations.sh) and [validation runner](benchmark-results/transaction-ingest-2026-09-11/run_validation_perf.sh).
- Raw CSV/stdout/resource files are under `baseline/`, `instrumented-baseline/`, each `candidate-*` directory and `final-retained/`. The baseline CPU profile is `instrumented-baseline/cpu-10m-b10k.sample.txt`.
- Base commit is `d7ddbde9fe1e099f05cbbc17b7601c42a3ab0a5c`. The original benchmark binary SHA-256 is `1e8d535dfe6edbffdfcfc83441f617ecdd7d69ad82776f3168885be05b303fd4`; the retained final binary is `cf77231c3ebacef25016b646bd63c3fd6b0f7a0d5a867a2fab68a4b7a5ff6f7f`. `final-source.patch` and `final-source.json` capture the exact dirty source.

Limitations: one Apple M5 laptop, one deterministic narrow-row schema, warm filesystem state and no OS-cache eviction. The first five-run 10M/10K prefix was unfavorable (−9.1% by independent medians) before the ten-run result reached +32.1%; this sensitivity is why raw ranges and paired deltas are reported. Fast/OFF has no per-commit durability promise. The allocation diagnostic records process-wide deltas in disjoint foreground windows, so a background checkpoint can allocate during one of those windows. The unexplained 10M process-footprint increase requires further allocator-versus-mmap investigation.

## Broader pre-optimization benchmark refresh

The remaining sections are the broad refresh captured before this ingest optimization. They remain useful for workloads not rerun above. Superseded transactional scale, phase and process-memory rows have been removed so that every transactional figure still shown in this document is current.

## Historical broad refresh: source, environment and evidence

- Source base: `d7ddbde9fe1e099f05cbbc17b7601c42a3ab0a5c`, with uncommitted changes captured in [source.patch](benchmark-results/refresh-2026-09-11/source.patch). The SHA alone does not reproduce this run. The source inventory covers crates, bindings, scripts, CI and Cargo files; generated reports are stored separately.
- Apple M5, 10 logical CPUs, 16 GiB RAM; macOS-26.6.2-arm64-arm-64bit-Mach-O.
- Rust/Cargo 1.93.1; benchmark profile is optimized with debug symbols, fat LTO and one codegen unit. SQLite 3.45.0 is bundled through rusqlite.
- 47 sequential processes completed successfully, from `2026-09-11T09:56:47Z` to `2026-09-11T10:13:09Z`. Builds completed before measurement. No other benchmark or build was run concurrently.
- Power and thermal observations are captured before and after every process. These observations do not establish CPU temperature or eliminate unrelated OS activity.
- [metadata.json](benchmark-results/refresh-2026-09-11/metadata.json) stores source/binary hashes, platform and environment overrides. [jobs.json](benchmark-results/refresh-2026-09-11/jobs.json) contains every exact executable and argument list. Each job has CSV or Criterion samples, stdout, a resources file and a status record.
- These use the normal benchmark allocator. The separate allocation-instrumented review is not mixed into these latency numbers.

## Historical broad refresh: method and timing definitions

Battery charge was 100% at every recorded boundary. Every process-boundary observation reported AC power. These are boundary observations, not continuous power monitoring.

Scale/bulk have three fresh runs per engine, alternating engine order. The sustained 1K-transaction workload has five. Writer and mixed/contention matrices have three runs per condition. SQL and synthetic ANN each have three fresh Criterion processes; small-transaction microbenchmarks have one process with Criterion's repeated samples. Criterion keeps its default 3 s warmup and 5 s measurement target; expensive cases may take longer. SQL/inserts set 10 samples, point reads/ANN 30.

Unless stated otherwise, tables show the median across repetitions; brackets show the **minimum–maximum across runs**, not a confidence interval. Throughput ratios divide the two reported medians. A reported p99 is the median of per-run p99 values, not a percentile of pooled samples. Scale point-read and scan values are **per-operation averages**, then aggregated across runs. Criterion values are means estimated within each process; their 95% confidence intervals and raw samples are retained in the artifacts. None of these is a production latency SLO.

| Timing | Included work |
| --- | --- |
| Ingest wall | Staging and commits, including automatic maintenance/backpressure that overlaps ingestion |
| Final checkpoint | Explicit checkpoint after ingestion |
| Maintenance drain | Wait for pending primary-run promotion after the checkpoint |
| Total load | Ingest + final checkpoint + drain, measured as one elapsed interval |
| Checkpoint/promotion work | Accumulated background work; may overlap ingest, so do not add it to wall time |
| Concurrent writes | Timed transactions; final checkpoint is outside the throughput window and recorded separately |
| Mixed throughput | Operations divided by the whole concurrent run duration; readers/writers may finish at different times |

Median stage values need not sum to the median total. Creation, fixture setup and validations follow the boundaries in the linked harnesses; whole-process resource files include those phases. Raw timings are retained so comparisons can be recalculated without inferring a different boundary.

## Historical broad refresh: durability and memory profiles

| Profile | EliteSQL | SQLite | Interpretation |
| --- | --- | --- | --- |
| Fast | Fast | WAL / synchronous=OFF | No per-commit sync guarantee |
| Balanced | Balanced, default periodic sync | WAL / synchronous=NORMAL | Durability/performance profiles, not identical loss-window contracts |
| Safe | Safe / F_FULLFSYNC on this Mac | FULL + fullfsync + checkpoint_fullfsync | Strict writer comparison uses F_FULLFSYNC in both engines |

Safe numbers below come from the strict concurrent-writer harness, which records and verifies the sync primitive. Fast/Balanced CSVs also label each engine's primitive, but Fast makes no timed physical sync calls; that label does not turn Fast into Safe. Scale, bulk, SQL, mixed/contention and ANN use Fast. SQLite scale disables automatic WAL checkpoints and performs its checkpoint explicitly; EliteSQL may checkpoint during ingestion, hence the separate total-load comparison.

The default EliteSQL envelope is 384 MiB: 64 MiB concurrent query pool, 16 MiB working budget per query, 128 MiB mutable-index pool, 128 MiB maintenance pool and 8 MiB reserve, with headroom remaining. This is **not an RSS limit**. Mapped pages, returned rows, allocator overhead, stacks and transport memory are not equivalent to governor reservations. Frozen ownership is reported without clamping even when temporarily above its pool. RSS and physical footprint appear separately below.

## Direct sorted load (separate API; pre-optimization run)

Only the still-relevant direct-load rows are retained from this older run. EliteSQL bulk uses `bulk_insert_sorted`; SQLite retains its transaction path with the same deterministic rows and 10K-row batches. Bulk is a specialized import API, not an acceleration of arbitrary transactions, and these results must not be compared as though they measured the optimized normal transaction path above.

| Path | Rows | EliteSQL total s [range] | SQLite total s [range] | SQLite time / EliteSQL time |
| --- | --- | --- | --- | --- |
| bulk | 1m | 0.492 [0.489–0.501] | 0.716 [0.710–0.739] | 1.45× |
| bulk | 10m | 4.443 [4.376–4.448] | 6.464 [6.451–6.592] | 1.45× |

| Path | Rows | EliteSQL point µs [range] | SQLite point µs [range] | EliteSQL scan s [range] | SQLite scan s [range] |
| --- | --- | --- | --- | --- | --- |
| bulk | 1m | 1.811 [1.745–1.910] | 2.381 [2.375–2.391] | 0.017 [0.017–0.017] | 0.027 [0.026–0.027] |
| bulk | 10m | 2.263 [2.231–2.337] | 3.113 [3.039–3.261] | 0.177 [0.176–0.177] | 0.263 [0.263–0.270] |

The raw historical bulk processes remain linked by [their run manifest](benchmark-results/refresh-2026-09-11/jobs.json). Their filesystem caches were not evicted.

## Small-transaction microbenchmarks (pre-optimization run)

The sustained 1M-row transaction table formerly in this section was superseded and removed; current 1K- and 10K-row transaction results are reported at the top. The following Criterion microbenchmarks are retained only as a historical characterization of smaller API calls.

| Criterion workload | Mean | 95% confidence interval (within this process) |
| --- | --- | --- |
| get_by_id/elitesql | 0.326 µs | 0.326 µs–0.327 µs |
| get_by_id/sqlite | 1.409 µs | 1.400 µs–1.421 µs |
| insert_1k_rows/elitesql | 11.806 ms | 11.537 ms–12.064 ms |
| insert_1k_rows/elitesql_single_txn | 2.209 ms | 2.025 ms–2.387 ms |
| insert_1k_rows/elitesql_single_txn_explicit_steady | 865.659 µs | 737.692 µs–996.051 µs |
| insert_1k_rows/elitesql_single_txn_steady | 751.065 µs | 649.074 µs–858.095 µs |
| insert_1k_rows/sqlite_autocommit | 8.248 ms | 8.214 ms–8.281 ms |
| insert_1k_rows/sqlite_single_txn | 626.660 µs | 622.122 µs–631.219 µs |
| insert_1k_rows/sqlite_single_txn_steady | 729.176 µs | 713.487 µs–747.148 µs |

[Microbenchmark harness](crates/elitesql-core/benches/vs_sqlite.rs). Insert entries time 1,000 rows; get-by-id entries time one read over 10K resident rows. Auto-generated and explicit id workloads are separate. These raw API/microbenchmark timings are not SQL or network latency.

## Concurrent writers (pre-optimization broad matrix)

[Harness](crates/elitesql-core/benches/concurrent_writers.rs): batch size 10; 200K rows/run for Fast and Balanced, 40K for Safe; three runs per writer count. Engine order alternates internally. Writers insert disjoint ids; SQLite uses its single-writer WAL model. The workloads do not include FK, identity or secondary-index work, which is measured separately.

### Fast

| Writers | EliteSQL rows/s [range] | SQLite rows/s [range] | Throughput ratio | EliteSQL p99 µs | SQLite p99 µs | EliteSQL worst max ms | SQLite worst max ms |
| --- | --- | --- | --- | --- | --- | --- | --- |
| 1 | 920,258 [772,671–954,342] | 418,213 [346,035–430,175] | 2.20× | 17.3 | 52.7 | 0.6 | 8.8 |
| 2 | 723,235 [713,127–725,720] | 372,305 [368,361–397,745] | 1.94× | 36.8 | 54.4 | 0.3 | 266.1 |
| 4 | 696,879 [693,681–700,357] | 333,152 [321,977–334,539] | 2.09× | 81.9 | 56.5 | 0.5 | 471.7 |
| 8 | 733,201 [728,783–755,067] | 267,159 [265,104–267,362] | 2.74× | 165.2 | 57.2 | 3.6 | 679.0 |
| 16 | 838,929 [835,495–852,472] | 176,276 [175,694–194,971] | 4.76× | 302.8 | 65.6 | 0.7 | 1099.0 |

[Raw rows and lock/sync counters](benchmark-results/refresh-2026-09-11/writers-fast.csv). Maxima are the worst recorded transaction across repetitions; p99 can hide writer starvation affecting fewer than 1% of transactions. Group commit depends on concurrency and scheduling; a ratio measured here is not a guarantee at a given connection count.

### Balanced

| Writers | EliteSQL rows/s [range] | SQLite rows/s [range] | Throughput ratio | EliteSQL p99 µs | SQLite p99 µs | EliteSQL worst max ms | SQLite worst max ms |
| --- | --- | --- | --- | --- | --- | --- | --- |
| 1 | 776,818 [660,053–801,997] | 434,167 [428,467–435,146] | 1.79× | 16.8 | 49.0 | 5.2 | 1.0 |
| 2 | 599,883 [589,003–603,210] | 381,552 [372,836–401,471] | 1.57× | 38.4 | 52.0 | 5.3 | 267.1 |
| 4 | 595,209 [592,486–601,034] | 323,629 [320,340–325,649] | 1.84× | 89.6 | 55.5 | 5.2 | 475.4 |
| 8 | 713,801 [697,318–716,039] | 265,063 [263,397–267,072] | 2.69× | 146.2 | 57.7 | 6.2 | 681.6 |
| 16 | 771,821 [745,602–805,832] | 161,658 [148,812–175,944] | 4.77× | 278.8 | 66.4 | 12.4 | 1306.0 |

[Raw rows and lock/sync counters](benchmark-results/refresh-2026-09-11/writers-balanced.csv). Maxima are the worst recorded transaction across repetitions; p99 can hide writer starvation affecting fewer than 1% of transactions. Group commit depends on concurrency and scheduling; a ratio measured here is not a guarantee at a given connection count.

### Safe

| Writers | EliteSQL rows/s [range] | SQLite rows/s [range] | Throughput ratio | EliteSQL p99 µs | SQLite p99 µs | EliteSQL worst max ms | SQLite worst max ms |
| --- | --- | --- | --- | --- | --- | --- | --- |
| 1 | 2,920 [2,806–2,930] | 2,884 [2,714–2,894] | 1.01× | 4296.7 | 4218.6 | 86.5 | 288.0 |
| 2 | 4,490 [4,366–5,071] | 2,838 [2,756–2,931] | 1.58× | 8001.0 | 4556.3 | 159.4 | 7200.8 |
| 4 | 9,874 [9,864–9,928] | 2,841 [2,781–2,858] | 3.48× | 5015.0 | 4389.2 | 21.1 | 11021.6 |
| 8 | 19,929 [19,769–20,049] | 2,865 [2,783–2,906] | 6.96× | 5087.0 | 4338.7 | 7.0 | 12705.0 |
| 16 | 38,282 [38,065–38,763] | 2,731 [2,685–2,871] | 14.02× | 5148.3 | 4867.5 | 9.1 | 13956.9 |

| Writers | EliteSQL commits/sync [range] |
| --- | --- |
| 1 | 1.00 [1.00–1.00] |
| 2 | 1.71 [1.63–1.83] |
| 4 | 4.00 [4.00–4.00] |
| 8 | 8.00 [8.00–8.00] |
| 16 | 15.94 [15.94–16.00] |

[Raw rows and lock/sync counters](benchmark-results/refresh-2026-09-11/writers-safe.csv). Maxima are the worst recorded transaction across repetitions; p99 can hide writer starvation affecting fewer than 1% of transactions. Group commit depends on concurrency and scheduling; a ratio measured here is not a guarantee at a given connection count.

## Persisted reads with concurrent writes (pre-optimization broad matrix)

| Readers | Writers | Reads/s | Writes/s | Read p99 µs | Write p99 µs |
| --- | --- | --- | --- | --- | --- |
| 1 | 0 | 1,060,854.2 | 0.0 | 1.2 | 0.0 |
| 1 | 1 | 979,366.9 | 39,174.7 | 1.5 | 22.3 |
| 1 | 4 | 1,008,636.4 | 40,345.5 | 1.5 | 83.2 |
| 2 | 0 | 1,438,520.2 | 0.0 | 1.7 | 0.0 |
| 2 | 1 | 1,426,973.7 | 57,078.9 | 2.6 | 41.3 |
| 2 | 4 | 1,456,565.0 | 58,262.6 | 1.8 | 121.9 |
| 4 | 0 | 2,396,411.9 | 0.0 | 2.0 | 0.0 |
| 4 | 1 | 2,177,719.3 | 87,108.8 | 14.0 | 142.2 |
| 4 | 4 | 2,332,756.7 | 93,310.3 | 8.6 | 360.1 |
| 8 | 0 | 3,366,829.5 | 0.0 | 3.3 | 0.0 |
| 8 | 1 | 2,348,924.5 | 93,957.0 | 46.0 | 307.3 |
| 8 | 4 | 2,761,325.5 | 110,453.0 | 27.1 | 402.1 |
| 16 | 0 | 3,949,067.6 | 0.0 | 4.5 | 0.0 |
| 16 | 1 | 2,350,555.8 | 94,022.2 | 75.7 | 358.2 |
| 16 | 4 | 2,777,772.3 | 111,110.9 | 66.1 | 352.2 |

100K persisted fixture rows, 1M point reads and 40K inserted rows in mixed runs, batch size 10, Fast. Zero writers isolates reads. Validation and its scan run outside the timed window. [Raw repetitions and tails](benchmark-results/refresh-2026-09-11/mixed.csv); [harness](crates/elitesql-core/benches/concurrent_rw.rs).

## Mutations, identities, foreign keys and derived indexes (pre-optimization broad matrix)

| Profile | Cache preparation | Reads/s | Writes/s | Read p99 µs | Write p99 µs |
| --- | --- | --- | --- | --- | --- |
| insert | warm | 2,262,765.5 | 113,138.3 | 84.5 | 391.0 |
| insert | reopened | 2,343,988.1 | 117,199.4 | 86.9 | 408.8 |
| update | warm | 1,771,917.3 | 88,595.9 | 124.5 | 1,066.1 |
| update | reopened | 1,877,047.1 | 93,852.4 | 119.8 | 1,728.7 |
| delete | warm | 1,933,023.9 | 96,651.2 | 115.6 | 950.3 |
| delete | reopened | 1,975,851.8 | 98,792.6 | 102.8 | 810.5 |
| identity | warm | 1,438,316.0 | 71,915.8 | 152.3 | 773.0 |
| identity | reopened | 1,520,575.3 | 76,028.8 | 149.2 | 737.2 |
| foreign-key | warm | 1,749,794.0 | 87,489.7 | 109.8 | 621.1 |
| foreign-key | reopened | 2,016,883.0 | 100,844.1 | 91.2 | 506.0 |
| derived | warm | 921,709.6 | 46,085.5 | 295.7 | 1,344.0 |
| derived | reopened | 1,070,650.0 | 53,532.5 | 242.6 | 866.2 |

16 readers, four writers, 50K persisted rows, 100K reads, 5K mutations, batch size 10, three runs per condition. The raw cache label `cold` means reopen without warmup: this Mac reports eviction counters 0/0. It **does not mean disk-cold I/O**. Derived is a different workload with more per-write index work; the difference from insert does not isolate one index's overhead. [contention.csv](benchmark-results/refresh-2026-09-11/contention.csv); [harness](crates/elitesql-core/benches/contention_matrix.rs).

## SQL and bound parameters (pre-optimization broad matrix)

| Query | Median estimated mean | Range of process means |
| --- | --- | --- |
| sql_1m/full_scan_filter_1m | 308.343 µs | 301.741 µs–309.712 µs |
| sql_1m/group_by_10k_groups_1m | 342.844 ms | 331.587 ms–342.904 ms |
| sql_1m/group_by_997_groups_1m | 315.492 ms | 308.751 ms–317.505 ms |
| sql_1m/indexed_join_100_of_1m | 160.469 µs | 159.690 µs–164.455 µs |
| sql_1m/indexed_join_100_of_1m_bound | 156.360 µs | 155.272 µs–156.722 µs |
| sql_1m/point_by_unique_index | 4.159 µs | 4.109 µs–4.352 µs |
| sql_1m/point_by_unique_index_bound | 3.471 µs | 3.439 µs–3.481 µs |

[Harness](crates/elitesql-core/benches/sql.rs): 1M orders and 10K users, with unique email and user-id indexes. Timings include parsing, planning and execution. The indexed join matches about 100 orders and returns the top 10. `full_scan_filter_1m` asks for **LIMIT 5** and may stop early: the name identifies fixture size, not a full-table traversal. GROUP BY cases consume the 1M rows. Bound and literal variants use the same result shape; differences this small require the retained confidence intervals.

## Synthetic ANN: 100K vectors (pre-optimization broad matrix)

| ef_search | Recall@10 range | Median search mean | Search mean range |
| --- | --- | --- | --- |
| 64 | 0.9520–0.9520 | 195.359 µs | 193.556 µs–201.897 µs |
| 128 | 0.9940–0.9940 | 334.714 µs | 313.717 µs–344.786 µs |
| 256 | 0.9980–0.9980 | 613.453 µs | 564.708 µs–614.069 µs |
| 512 | 1.0000–1.0000 | 1.089 ms | 1.087 ms–1.120 ms |

Indexed ingest: **9.232 s** [9.099–9.240]. Open with the persisted graph: **81.7 ms** [79.1–100.2]. All recall gates passed in all three fresh builds.

[Harness](crates/elitesql-core/benches/vector.rs): dimension 64, 1,024 deterministic clusters, noise 0.6, cosine top-10. Quality compares against brute force for 50 fixed queries; Criterion samples independently generated queries and includes their generation in the timed loop. Construction uses m=16 and ef_construction=200, with the normal profile rather than the monolithic override. This synthetic corpus does not predict semantic retrieval quality or high-dimensional production memory needs. The previous Potion/MIRACL 250K run is historical and was not rerun for this refresh.

## WAL preallocation diagnostic (pre-optimization broad matrix)

| Mode | Sync p50 µs [range] | Sync p95 µs [range] |
| --- | --- | --- |
| growing | 3,972.5 [3,970.9–3,983.9] | 4,098.5 [4,071.2–4,175.2] |
| preallocated | 3,980.2 [3,937.8–3,991.5] | 4,069.8 [4,066.9–4,107.9] |

Five paired repetitions, alternating mode, 100 writes of one 4 KiB frame per mode; the preallocated file reserves 64 MiB before timing. This measures `File::sync_data` on this storage, not SQL commit throughput or a shipped WAL-preallocation feature. [wal-preallocation.csv](benchmark-results/refresh-2026-09-11/wal-preallocation.csv).

## Observed process memory (retained historical workloads)

| Whole process | Engine | Peak RSS MiB [range] | Peak physical footprint MiB [range] |
| --- | --- | --- | --- |
| scale-bulk-10m | elitesql | 2,414.0 [2,414.0–2,414.0] | 7.8 [7.8–7.9] |
| scale-bulk-10m | sqlite | 9.8 [9.8–9.8] | 4.9 [4.9–4.9] |
| ANN including ground truth | elitesql | 245.9 [241.4–250.9] | 139.4 [138.9–143.9] |

Values come from `/usr/bin/time -l`. Each peak covers setup, workload, validation and cleanup; the ANN process also retains original vectors and brute-force ground truth. These are not engine-only heap measurements and exclude system-wide filesystem cache; RSS ratios therefore do not compare total host-memory use between engines. Clean mapped pages can contribute to RSS while remaining reclaimable. The configured 384 MiB envelope must not be advertised as a hard process-memory maximum.

## Reproducing and inspecting the historical broad refresh

```bash
# Build first; do not measure while the compiler is running.
cargo bench --locked -p elitesql-core --no-run --message-format=json \
  > /tmp/elitesql-bench-build.jsonl
python3 scripts/refresh-benchmarks.py \
  --build-json /tmp/elitesql-bench-build.jsonl \
  --output benchmark-results/local-refresh
```

Use a new output directory. `--resume` skips successful jobs only when source and binary hashes still match. This archived run's [job manifest](benchmark-results/refresh-2026-09-11/jobs.json) is the authoritative complete command matrix. Every measurement uses temporary benchmark databases; the runner does not publish or commit repository changes.

For this captured dataset, `python3 benchmark-results/refresh-2026-09-11/summarize.py` regenerates this document and [summary.json](benchmark-results/refresh-2026-09-11/summary.json). It verifies successful statuses and unchanged measured source hashes. To reproduce the measured dirty tree elsewhere, start at the recorded base SHA and apply [source.patch](benchmark-results/refresh-2026-09-11/source.patch) before building. Criterion samples/estimates are preserved under [criterion/](benchmark-results/refresh-2026-09-11/criterion/); their timestamps and status files distinguish this run from earlier artifacts.

## Historical broad refresh: interpretation and limitations

- Transactional load and direct sorted load have different APIs and preconditions; do not apply the bulk speedup to arbitrary transactions.
- High writer throughput can coexist with latency tails or starvation. Compare p99, maxima, checkpoint costs and durability together.
- The runs use one laptop, deterministic fixtures and filesystem caches that are not forcibly cold. They do not characterize long-duration service behavior, network/client overhead, all data widths or every selectivity.
- Synthetic ANN recall measures approximation of exact vector neighbors. It is not a relevance benchmark, and the 64D result cannot establish 256D/768D memory capacity.
- CI/functional acceptance and crash-recovery tests are documented in the implementation report; benchmark success is not a proof of every integrity invariant or power-loss scenario.

Historical material: [archived September 5 benchmark document](benchmark-results/archive/benchmark-2026-09-05.md), [September 5 acceptance](benchmark-results/current-acceptance-2026-09-05.md), [September 4 acceptance](benchmark-results/current-acceptance-2026-09-04.md), [August 23 acceptance](benchmark-results/current-acceptance-2026-08-23.md), and [the separate September 11 instrumented integrity/performance comparison](benchmark-results/review-2026-09-11/README.md). Those figures are not relabelled as fresh measurements. [Implementation and validation](docs/implementacion-plan.md).
