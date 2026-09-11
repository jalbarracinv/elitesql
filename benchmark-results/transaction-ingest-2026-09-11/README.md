# Transactional ingest optimization — 2026-09-11

This directory contains the raw data, frozen executables, profiling output and exact run scripts for the transactional-ingest optimization. The measured path is the ordinary `begin` / `insert` / `commit` path; `bulk_insert_sorted` is disabled in every result reported here.

## Result

The final implementation exceeded the 15% target in all four prioritized workloads relative to the original, uninstrumented baseline. Values are medians of independent processes; brackets are the observed minimum–maximum range.

| Rows / transaction | Original EliteSQL s | Final EliteSQL s | Reduction | Final rows/s | Contemporary SQLite s | SQLite time / final |
| --- | ---: | ---: | ---: | ---: | ---: | ---: |
| 1M / 1K | 0.943 [0.942–0.966] | 0.759 [0.745–0.816] | 19.5% | 1,317,051 | 0.754 [0.743–0.784] | 0.99× |
| 1M / 10K | 0.887 [0.881–0.898] | 0.692 [0.688–1.004] | 22.0% | 1,445,309 | 0.703 [0.698–0.724] | 1.02× |
| 10M / 1K | 8.876 [8.524–9.085] | 7.263 [6.908–8.167] | 18.2% | 1,376,791 | 9.208 [9.018–10.623] | 1.27× |
| 10M / 10K | 8.783 [7.885–10.615] | 6.208 [5.815–9.387] | 29.3% | 1,613,375 | 7.689 [6.797–8.137] | 1.24× |

There are five final repetitions for every cell and ten for 10M/10K, which was extended because its first five-process prefix was dominated by asymmetric WAL-write latency. The ten-process final CV is 22.6% and the initial CV is 13.8%. The paired median reduction over those ten repetitions is 22.4%; the ratio of independent medians is 32.1%. Both are retained in `summary.json`.

## Timing boundaries and dominant costs

The revised harness creates values before staging and records three disjoint caller windows: record/value construction, staging/SQLite execution, and commit calls. EliteSQL commit counters are also exclusive: preparation/order handling, canonical record encoding, validation under the commit mutex, WAL encoding/version-and-CRC patching, WAL append, durability sync wait, MVCC publication, and foreground maintenance wait. Background checkpoint and promotion work is reported separately because it may overlap ingestion. In all archived runs, the sum of exclusive core phases was at most 92.5% of commit-call wall time and never exceeded it.

Medians from the instrumentation-equivalent initial and final binaries:

| Workload / phase | Initial s | Final s | Change |
| --- | ---: | ---: | ---: |
| 10M/1K staging | 1.995 | 1.425 | −28.6% |
| 10M/1K prepare | 0.111 | 0.043 | −61.5% |
| 10M/1K record encode | 0.748 | 0.544 | −27.2% |
| 10M/1K MVCC apply | 1.913 | 0.866 | −54.7% |
| 10M/1K commit calls | 5.658 | 4.276 | −24.4% |
| 10M/10K staging | 1.894 | 1.300 | −31.4% |
| 10M/10K prepare | 0.114 | 0.044 | −61.1% |
| 10M/10K record encode | 0.769 | 0.543 | −29.4% |
| 10M/10K MVCC apply | 1.870 | 0.844 | −54.9% |
| 10M/10K commit calls | 5.677 | 3.442 | −39.4% |

Validation was effectively unchanged (−2.1% to +2.4% across the two 10M cases). WAL encoding became 4.0–7.5% slower, an absolute 0.017–0.032 s increase, and was retained because the eliminated record copies and MVCC work dominated it. The 10M/10K median WAL append improvement is not attributed to the code: identical bytes were written and that phase varied substantially with the host's storage state.

The baseline CPU sample is `instrumented-baseline/cpu-10m-b10k.sample.txt`. It identified `PrimaryIdx::push`/B-tree insertion and comparison, allocation/freeing while dropping intermediate record maps, canonical record encoding, CRC, memory copies and `write(2)` as the dominant active stacks. The separate allocation run confirmed 31.8M allocations inside commit calls before optimization.

## Retained implementation

- Transaction staging keeps strictly increasing ids in a contiguous vector and converts once to a hash map for updates, deletes, duplicates or out-of-order ids. The general path is still sorted before commit, and statement savepoint behavior is unchanged.
- Canonical payloads are encoded directly into one transaction-owned arena. MVCC versions hold reference-counted ranges into that arena; the WAL format and recovery decoder are unchanged.
- The primary MVCC delta keeps monotonically increasing ids in a contiguous ordered vector and converts once to a B-tree for a general workload. Point lookup, ordered range traversal, multi-version merge and persisted run formats retain their existing behavior.
- Exact-capacity record encoding replaces repeated buffer growth.
- `SmallVec` was evaluated and rejected from the final result. It reduced allocation calls but increased the inline entry size and process footprint, while changing ingest latency by only −0.3% to +3.0% in the direct A/B.

No validation branch was removed. The same validation and derived-index code runs for nontrivial schemas, and the optimized coordinated-commit candidate remains restricted to inserts without identities, foreign keys or derived indexes. Memory governor capacities and thresholds were not changed.

## Allocation, bytes and memory

The allocation diagnostic is a separate binary with a global counting allocator and is not mixed into normal latency results. Three 10M/10K processes per version produced these medians:

| Metric | Initial | Final | Change |
| --- | ---: | ---: | ---: |
| Allocation calls | 141,870,849 | 120,209,959 | −15.3% |
| Commit allocation calls | 31,780,987 | 10,110,809 | −68.2% |
| Cumulative allocated bytes | 23,155,193,871 | 23,239,006,257 | +0.36% |
| Commit allocated bytes | 11,474,442,000 | 11,158,566,197 | −2.75% |
| Peak live allocator bytes | 313,996,986 | 228,188,791 | −27.3% |
| Live bytes at end | 1,110,176 | 1,110,176 | unchanged |
| Logical index-delta peak | 118,788,890 | 118,788,890 | unchanged |
| Logical maintenance peak | 134,217,728 | 134,217,728 | unchanged |

Persistent work is byte-identical between initial and final for a given workload. At 10M/10K both append 1,598,904,890 WAL bytes, write 559,191,396 checkpoint bytes, and read/write 366,829,504/366,828,664 promotion bytes. The normal benchmark reports identical final disk sizes as well. This is evidence that the speedup did not skip WAL, checkpoint or promotion work.

Process memory remains a limitation. At 1M, final peak RSS/physical footprint fell from 393/263 MiB to 316/163 MiB for 1K batches and from 449/309 MiB to 306/213 MiB for 10K batches. At 10M it rose from 997/263 MiB to 1,198/520 MiB (1K) and from 1,060/312 MiB to 1,294/580 MiB (10K). `/usr/bin/time -l` includes allocator retention and mapped pages touched by checkpointing, validation and the final scan; it is not an engine-only heap metric. The lower counted live heap and unchanged governor peaks show that no configured budget was raised, but they do not erase the observed process-footprint regression.

## Correctness, durability, concurrency and reads

The exact retained source passed:

```text
cargo fmt --all --check
cargo clippy --workspace --all-targets --locked -- -D warnings
cargo test --workspace --locked
python3 -m unittest discover -s bindings/python/tests -v
node bindings/node/test.js
node bindings/node/test-integration.js
```

The Rust suite includes transaction atomicity/isolation and out-of-order staging, conflicts, uniqueness, identities, foreign keys/cascades, statement rollback, memory rejection, checkpoint/compaction, WAL corruption and torn-tail recovery, kill-9 recovery, DDL crash recovery, vector crash recovery and all durability modes. Python reported 8/8 tests; both Node commands exited successfully.

The final concurrent-writer harness used three repetitions, batch size 10, writer counts 1 and 4:

| Durability / writers | EliteSQL rows/s | EliteSQL p99 µs | SQLite rows/s | SQLite p99 µs | EliteSQL sync evidence |
| --- | ---: | ---: | ---: | ---: | --- |
| Fast / 1 | 1,013,013 | 15.4 | 436,403 | 49.7 | no timed sync |
| Fast / 4 | 707,211 | 81.3 | 356,312 | 51.8 | no timed sync |
| Balanced / 1 | 896,057 | 15.7 | 437,842 | 49.3 | 4 syncs; 2,500 commits/sync |
| Balanced / 4 | 586,700 | 89.3 | 364,931 | 51.4 | 6 syncs; 1,666.7 commits/sync |
| Safe / 1 | 2,610 | 4,236 | 2,791 | 5,068 | F_FULLFSYNC; 1 commit/sync |
| Safe / 4 | 10,328 | 5,055 | 2,752 | 5,031 | F_FULLFSYNC; 4 commits/sync |

Three fresh 1M-row Criterion processes show no query regression above 3% versus the pre-optimization medians: point unique −1.3%, bound point −1.5%, indexed join −3.0%, bound join −1.2%, LIMIT scan −1.6%, GROUP BY 997 groups −0.7%, and GROUP BY 10K groups +0.01%. The scale harness's 1M/1K direct-get median moved from 1.284 to 1.351 µs (+5.2%), but the run ranges overlap (1.222–1.338 versus 1.278–1.507 µs), the persisted bytes and post-checkpoint lookup path are unchanged, and the higher-sample Criterion point queries improved; it is therefore recorded as noise-sensitive rather than a confirmed read regression. Scale-harness 10M scans were even more sensitive to page residency and are retained in every raw CSV rather than used as a stable latency claim.

## Reproduction and artifact map

Environment: Apple M5, 10 logical CPUs, 16 GiB RAM, macOS 26.6.2 arm64; Rust/Cargo 1.93.1; bundled SQLite 3.45.0. Scale uses EliteSQL Fast and SQLite WAL with `synchronous=OFF`, deterministic text ids and identical payloads, one writer, 1,000 point reads, one scan and an explicit final checkpoint. Processes are sequential; engine order rotates by repetition. Filesystem caches are not evicted.

- `baseline/`: original uninstrumented five-run matrix and frozen binary (`1e8d535dfe6edbffdfcfc83441f617ecdd7d69ad82776f3168885be05b303fd4`). The exact starting dirty source is also captured by `../refresh-2026-09-11/source.patch` and its source inventory; relevant initially-untracked file hashes match `baseline/untracked.sha256`.
- `instrumented-baseline/`: instrumentation-equivalent initial binary, allocation binary, CPU sample and raw timings. Scale binary SHA-256: `d3c63656319ac3212e990faa7a164f53a238be1f3e886d5ad61d95c3c0ba5a6e`.
- `candidate-*`: isolated candidate A/B data, including the rejected `SmallVec` variant.
- `final-retained/`: definitive raw CSV/stdout/resource files, allocation JSONL, concurrent writers and Criterion SQL output. Scale binary SHA-256: `cf77231c3ebacef25016b646bd63c3fd6b0f7a0d5a867a2fab68a4b7a5ff6f7f`.
- `run_matrix.sh`, `run_allocations.sh`, `run_no_smallvec_ab.sh` and `run_validation_perf.sh`: exact invocations. Run the matrix into a new directory to avoid overwriting archived results.
- `summarize.py`: regenerates `summary.json`, including distributions, CVs, paired deltas, resource peaks and the exclusive-phase invariant.
- `final-source.patch` and `final-source.json`: exact dirty source capture and hashes relative to base commit `d7ddbde9fe1e099f05cbbc17b7601c42a3ab0a5c`.

## Limitations

- This is one laptop, one deterministic narrow-row schema and warm filesystem state. It is not a production SLO or a confidence interval.
- Storage variation is material. The first five final 10M/10K runs alone gave −9.1% by ratio of medians because three final WAL appends were unusually slow; extending the predeclared rotation to ten gave +32.1%. The paired ten-run median is +22.4%. Reproduction should use multiple alternating processes and report the raw range.
- Fast/OFF does not promise per-commit durability. Balanced and Safe are validated separately; only Safe is a strict `F_FULLFSYNC` comparison on this Mac.
- Allocation counters are process-wide deltas in disjoint foreground windows; background checkpoint work can occur during a window. They diagnose allocation pressure but are not latency measurements.
- The reduction in live heap did not translate into lower 10M process footprint. Further work should isolate allocator retention from mmap page residency before changing memory ownership again.
