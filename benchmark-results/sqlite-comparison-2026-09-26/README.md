# EliteSQL vs SQLite — fresh run, 2026-09-26

The public comparison is [benchmark.md](../../benchmark.md). This directory
contains only measurements of the current EliteSQL tree against SQLite. The
previous public report is preserved in [old_benchmark.md](../../old_benchmark.md).

The runner is [scripts/compare-sqlite.py](../../scripts/compare-sqlite.py).
Builds finish before measurements; all 34 jobs run sequentially. Every engine
and workload has three repetitions, with engine order alternating.

| Suite | Configuration | Evidence |
| --- | --- | --- |
| Transactional load and SQL point reads | 1M/10M rows, 1K/10K rows per transaction, Fast/OFF; 100K warmed SQL lookups per run | `scale-*.csv` |
| Concurrent writers | 1/4/8 writers; batch size 10; 200K rows in Fast/Balanced, 10K in Safe; strict fullfsync in Safe | `writers-*.csv` |
| SaaS cost per operation | Baseline schema; 20K accounts, 5K products; 200 samples, 40 for heavy operations; embedded Python drivers | [saas-operations.json](saas-operations.json) |
| SaaS concurrency | Baseline schema; 10/100/500 users; five-second ramp, five-second warmup, 30-second measured window; sidecar vs embedded SQLite | `saas-r*/stages.csv` and per-stage files |

The SaaS concurrency mix excludes `recommend`: the engines implement different
recommendation semantics. The per-operation runner measures the complete
sixteen-operation `full-v2` mix; the public summary excludes `recommend` and
renormalizes the other fifteen weights. Both engines retain their native
full-text indexes. This compares application behavior, not identical text
ranking algorithms.

- [metadata.json](metadata.json): source/binary hashes, source commit, environment,
  configuration and execution boundaries.
- [source.patch](source.patch): measured local source changes, including the runner.
- [build.jsonl](build.jsonl): Cargo executable inventory.
- [jobs.json](jobs.json): exact command matrix and per-job environment overrides.
- `*.status.json`: command, before/after power/thermal observations, exit code and elapsed time.
- `*.log` and `*.resources.txt`: stdout, stderr and whole-process resource measurements.
- `saas-r*/check.json`: checks after stopping the sidecar without a clean close,
  after reopening, and SQLite integrity checks. Recoverable warnings after an
  unclean stop must be read separately from the clean-reopen result.
- [summary.json](summary.json): derived paired medians, ranges, ratios and validation.

The Rust suite uses rusqlite's bundled SQLite, while the SaaS suite uses Python's
SQLite. Exact versions are in the logs and metadata; do not combine them into
one supposed SQLite build. Databases are temporary benchmark fixtures, not
user databases. Caches are not forcibly evicted.

## Recovery checks

All three EliteSQL sidecars were stopped with SIGTERM without a clean close.
The immediate checks returned exit code 3 with recoverable warnings and zero
errors. Reopening the databases cleared the warnings; all subsequent checks
returned exit code 0 with zero warnings and errors. Reopen and check time are
outside the concurrent-throughput window.

| Run | Warnings after stop | Warnings after reopen | Errors after reopen |
| --- | ---: | ---: | ---: |
| saas-r1-sidecar | 961,198 | 0 | 0 |
| saas-r2-sidecar | 843,042 | 0 | 0 |
| saas-r3-sidecar | 843,748 | 0 | 0 |

Exact check output and reopen times: [recovery-checks.json](recovery-checks.json).
