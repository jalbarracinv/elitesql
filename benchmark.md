# EliteSQL vs SQLite — 2026-09-26

Fresh measurements of EliteSQL against SQLite, including the mini-SaaS simulator. EliteSQL leads in SQL point reads and concurrent-writer throughput; SQLite is faster in the smaller load cases and in the isolated SaaS operation mix. SaaS throughput favors EliteSQL at 100 users, while the 500-user ranges overlap. Results are specific to the workloads below.

## How to read the results

Values are medians across independent fresh runs (see the recorded repetition count below). Brackets show the minimum–maximum across runs, not confidence intervals. **Ratio = SQLite time / EliteSQL time**, or equivalently **EliteSQL throughput / SQLite throughput**: above 1 favors EliteSQL; below 1 favors SQLite. Each p99 is the median of per-run p99 values, not a percentile of pooled requests.

## Transactional load

Normal transactions, deterministic narrow rows, Fast durability on EliteSQL and WAL/synchronous=OFF on SQLite. Total load includes ingestion, final checkpoint and maintenance drain. Neither configuration guarantees a durable sync per commit.

| Rows | Rows/transaction | EliteSQL total s [range] | SQLite total s [range] | Ratio |
| ---: | ---: | ---: | ---: | ---: |
| 1,000,000 | 1,000 | 0.879 [0.836–1.083] | 0.739 [0.734–0.750] | 0.84× |
| 1,000,000 | 10,000 | 0.797 [0.774–0.837] | 0.710 [0.707–0.732] | 0.89× |
| 10,000,000 | 1,000 | 7.245 [7.185–7.698] | 11.137 [10.076–12.112] | 1.54× |
| 10,000,000 | 10,000 | 7.808 [6.463–8.954] | 7.748 [7.673–8.612] | 0.99× |

Harness: [scale_vs_sqlite.rs](crates/elitesql-core/benches/scale_vs_sqlite.rs). Exact commands and raw CSVs: [jobs.json](benchmark-results/sqlite-comparison-2026-09-26/jobs.json).

## SQL point reads

Both engines execute the same parameterized SELECT returning title, body and score. SQLite uses a prepared statement; EliteSQL uses query_params. Each run performs 100,000 warmed primary-key lookups after the transactional load.

| Rows | Rows/transaction during load | EliteSQL µs/read [range] | SQLite µs/read [range] | Ratio |
| ---: | ---: | ---: | ---: | ---: |
| 1,000,000 | 1,000 | 1.732 [1.705–1.737] | 2.398 [2.382–2.415] | 1.38× |
| 1,000,000 | 10,000 | 1.742 [1.724–1.840] | 2.399 [2.396–2.415] | 1.38× |
| 10,000,000 | 1,000 | 2.106 [2.059–2.301] | 19.317 [15.915–19.684] | 9.17× |
| 10,000,000 | 10,000 | 2.098 [2.070–2.198] | 14.396 [12.931–16.649] | 6.86× |

## Concurrent writers

Disjoint ids, ten rows per transaction, one/four/eight writers. Each engine writes 200,000 rows per run in Fast/Balanced and 10,000 in Safe. Final checkpoints are outside this throughput window. Fast maps to SQLite OFF; Balanced maps to NORMAL, with different loss-window contracts. Safe uses FULL plus fullfsync/checkpoint_fullfsync on this Mac and verifies F_FULLFSYNC on both engines. EliteSQL uses a 200 µs group-commit coalescing window.

| Profile | Writers | EliteSQL rows/s [range] | SQLite rows/s [range] | Ratio | EliteSQL p99 ms | SQLite p99 ms |
| --- | ---: | ---: | ---: | ---: | ---: | ---: |
| Fast | 1 | 700,543 [641,337–715,841] | 440,401 [437,856–444,306] | 1.59× | 0.021 | 0.048 |
| Fast | 4 | 521,873 [519,782–523,147] | 323,875 [280,056–327,902] | 1.61× | 0.136 | 0.056 |
| Fast | 8 | 712,649 [711,592–719,546] | 266,974 [234,385–273,340] | 2.67× | 0.216 | 0.100 |
| Balanced | 1 | 590,763 [588,006–598,600] | 426,290 [423,315–427,690] | 1.39× | 0.024 | 0.052 |
| Balanced | 4 | 467,139 [467,085–471,700] | 326,032 [325,431–327,500] | 1.43× | 0.146 | 0.055 |
| Balanced | 8 | 640,377 [640,079–654,157] | 266,926 [266,088–267,389] | 2.40× | 0.269 | 0.058 |
| Safe | 1 | 2,704 [2,687–2,856] | 2,833 [2,765–2,914] | 0.95× | 4.884 | 5.023 |
| Safe | 4 | 9,892 [9,633–10,113] | 2,584 [2,533–2,711] | 3.83× | 5.149 | 5.048 |
| Safe | 8 | 19,255 [19,077–19,520] | 2,581 [2,417–2,704] | 7.46× | 5.177 | 5.158 |

Raw data: [writers-fast.csv](benchmark-results/sqlite-comparison-2026-09-26/writers-fast.csv), [writers-balanced.csv](benchmark-results/sqlite-comparison-2026-09-26/writers-balanced.csv), [writers-safe.csv](benchmark-results/sqlite-comparison-2026-09-26/writers-safe.csv).

## Mini-SaaS application

The same ecommerce service, deterministic seed, 20,000 accounts and 5,000 products, baseline schema on both engines. Fifteen weighted operations cover catalogue browsing, product details, authentication, full-text search, carts, checkout, reviews, orders and administration. Recommendations are excluded from the comparison: EliteSQL's vector search and SQLite's category-based fallback answer different questions. Full-text search uses each engine's native implementation; this is an application comparison, not proof of identical ranking. See [the simulator](examples/saas_simulation/README.md).

### Cost of one operation

EliteSQL runs embedded through its Python binding; SQLite uses Python sqlite3. Independent account/cart state is prepared outside the timed intervals. There are 200 samples per operation/run, 40 for checkout and admin_dashboard. The underlying full-v2 run retains all sixteen operation measurements; the following weighted total removes recommend and normalizes the remaining weights to their sum.

| Operation | EliteSQL µs [range] | SQLite µs [range] | Ratio |
| --- | ---: | ---: | ---: |
| **Weighted operation** | **72.84** | **45.19** | **0.62×** |
| browse | 121.07 [120.53–121.29] | 81.77 [79.79–85.59] | 0.68× |
| product_detail | 13.58 [12.34–14.14] | 5.06 [5.00–5.44] | 0.37× |
| session_check | 23.45 [22.78–24.04] | 12.43 [12.32–15.30] | 0.53× |
| add_to_cart | 43.34 [42.01–43.61] | 22.85 [22.63–22.96] | 0.53× |
| search_text | 133.00 [129.87–133.09] | 89.26 [88.84–93.25] | 0.67× |
| view_cart | 23.10 [22.53–23.44] | 6.58 [6.48–6.70] | 0.28× |
| checkout | 181.28 [177.40–260.03] | 95.70 [93.99–100.40] | 0.53× |
| update_cart_item | 31.86 [31.32–32.81] | 11.99 [11.88–12.07] | 0.38× |
| order_history | 23.12 [22.91–23.36] | 7.84 [7.74–7.91] | 0.34× |
| write_review | 33.81 [32.54–34.34] | 22.71 [22.39–22.87] | 0.67× |
| relogin | 87.44 [71.42–87.73] | 50.05 [49.48–52.45] | 0.57× |
| update_profile | 23.75 [23.74–23.96] | 5.89 [5.77–6.01] | 0.25× |
| admin_dashboard | 891.94 [891.82–903.01] | 668.57 [661.34–675.07] | 0.75× |
| signup | 40.28 [39.67–42.20] | 35.24 [34.94–37.82] | 0.87× |
| restock | 33.51 [32.76–34.11] | 16.90 [16.86–17.55] | 0.50× |

Raw timings and operation weights: [saas-operations.json](benchmark-results/sqlite-comparison-2026-09-26/saas-operations.json).

### Concurrent requests

Closed loop, no think time, 10/100/500 virtual users, ten generator processes, up to one connection per user. Each level has a five-second ramp, five-second excluded warmup and 30 measured seconds. Every repetition starts a fresh database; it then accumulates across the three levels. Engine order alternates between repetitions. EliteSQL runs as a Unix-socket sidecar; SQLite is embedded in the generator processes, so these figures include different transport overheads.

| Users | EliteSQL ops/s [range] | SQLite ops/s [range] | Ratio | EliteSQL p99 ms | SQLite p99 ms | EliteSQL success % | SQLite success % |
| ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: |
| 10 | 13,018 [12,742–14,829] | 15,385 [14,635–16,390] | 0.85× | 6.824 | 9.289 | 100.0000 | 100.0000 |
| 100 | 15,248 [14,858–17,633] | 12,320 [11,853–12,550] | 1.24× | 46.234 | 148.058 | 100.0000 | 100.0000 |
| 500 | 12,081 [11,095–13,968] | 11,390 [11,059–11,706] | 1.06× | 208.810 | 1299.358 | 100.0000 | 100.0000 |

Validation: 0 failed operations; 0 read-your-writes violations; business invariants passed at every level; offline integrity checks passed for both engines after reopening. Success columns show the lowest rate across repetitions. Conflicts, retries, individual operation p99, maximum latency and resource samples remain in each simulator run's report and stage files.

EliteSQL checks immediately after stopping the sidecar without a clean close returned exit code 3 with 843,042–961,198 recoverable warnings and zero errors. After reopening, all three checks returned exit code 0 with zero warnings and errors. Recovery and check time are outside the throughput window. [Exact check output and reopen times](benchmark-results/sqlite-comparison-2026-09-26/recovery-checks.json).

Simulator runs: [saas-r1-sidecar/report.md](benchmark-results/sqlite-comparison-2026-09-26/saas-r1-sidecar/report.md), [saas-r1-sqlite/report.md](benchmark-results/sqlite-comparison-2026-09-26/saas-r1-sqlite/report.md), [saas-r2-sqlite/report.md](benchmark-results/sqlite-comparison-2026-09-26/saas-r2-sqlite/report.md), [saas-r2-sidecar/report.md](benchmark-results/sqlite-comparison-2026-09-26/saas-r2-sidecar/report.md), [saas-r3-sidecar/report.md](benchmark-results/sqlite-comparison-2026-09-26/saas-r3-sidecar/report.md), [saas-r3-sqlite/report.md](benchmark-results/sqlite-comparison-2026-09-26/saas-r3-sqlite/report.md).

## Environment and reproduction

- Apple M5, 10 logical CPUs, 16 GiB RAM; macOS-26.6.2-arm64-arm-64bit-Mach-O.
- 3 repetitions per engine and workload.
- Source commit: `5ff119005c8fe6581b1721cce990cb756540c30f`. Measured source/binary hashes and local changes: [metadata.json](benchmark-results/sqlite-comparison-2026-09-26/metadata.json), [source.patch](benchmark-results/sqlite-comparison-2026-09-26/source.patch).
- rustc 1.93.1 (01f6ddf75 2026-02-11) (Homebrew); Python 3.14.7. SQLite 3.45.0 is bundled with the Rust suite; Python uses SQLite 3.53.4 for SaaS. These are separate benchmark suites.
- Builds finished before measurement. Jobs ran sequentially on AC power; power/thermal observations are recorded at job boundaries. Caches were warmed or left to the OS; no disk-cold or cache-eviction claim is made.

```bash
# Choose an unused output directory.
out=benchmark-results/sqlite-comparison-local
mkdir -p "$out"
cargo bench --locked -p elitesql-core --bench scale_vs_sqlite \
  --bench concurrent_writers --no-run --message-format=json > "$out/build.jsonl"
cargo build --locked --release -p elitesql-ffi -p elitesql-cli
python3 scripts/compare-sqlite.py --output "$out" \
  --build-json "$out/build.jsonl" --repetitions 3 --saas-duration 30
```

The runner records every command, timestamp, exit status and resource log, verifies source and binary hashes, and generates the result tables and [summary.json](benchmark-results/sqlite-comparison-2026-09-26/summary.json). Use `--resume` only with unchanged sources, binaries and settings; `--summarize-only` regenerates the tables and base report from the completed run. This published document also includes reviewed interpretation and recovery notes.

## Limits

One laptop, one narrow-row load fixture and one small SaaS catalogue. The SaaS windows characterize this local configuration, not maximum supported users or a production latency promise. The load, concurrent-write and SaaS suites have different transaction sizes, timing boundaries and durability settings; compare engines within a row. Throughput gains may coexist with worse latency tails. High concurrency can be limited by the Python generator, transport, checkpoints or scheduling. Per-run reports retain stalls, maximum latency, errors and retries so the medians do not conceal failures.

The previous report, including comparisons between EliteSQL builds and historical diagnostics, is preserved as [old_benchmark.md](old_benchmark.md).
