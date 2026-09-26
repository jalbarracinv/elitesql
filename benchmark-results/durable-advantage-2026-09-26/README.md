# Durable concurrent transactions — 2026-09-26

This directory contains the proof behind
[the central-advantage report](../../docs/central-advantage.md).
The engine is v0.1.0 source (`3de0ce4`), with simulator durability settings
corrected for the comparison. No production Rust engine changes were made.

## Reproduction

On the measured macOS/Apple Silicon configuration, from the repository root:

```bash
cargo build --locked --release -p elitesql-cli -p elitesql-ffi
python3 scripts/prove-durable-advantage.py --output benchmark-results/durable-local
```

The runner refuses to overwrite evidence. Defaults are three repetitions,
10/100/500 users, 30 measured seconds plus 5-second ramp and warmup per level,
10 generator processes, Safe vs WAL/FULL/fullfsync, and the shared service
with recommendations excluded. It retains failed hypotheses as well as successes.
Run the FIFO control sequentially afterwards: copy `sqlite-pool-control.py`
and `audit.py` into that new output directory and run them there. They locate
the output/repository from their own paths, preserve commands and verify settings,
raw-sample hashes and correctness. Controls use one process and one connection,
the same levels, timing, seeds and service, with samples kept for exact user p99.

```bash
cp benchmark-results/durable-advantage-2026-09-26/sqlite-pool-control.py benchmark-results/durable-local/
cp benchmark-results/durable-advantage-2026-09-26/audit.py benchmark-results/durable-local/
python3 benchmark-results/durable-local/sqlite-pool-control.py
python3 benchmark-results/durable-local/audit.py
python3 -m unittest discover -s examples/saas_simulation/tests -v
cargo test --locked -p elitesql-core --test group_commit --test group_commit_crash --test crash_kill -- --nocapture
bash scripts/acceptance.sh
```

Databases are temporary and removed after each completed run; final integrity
checks, invariants, resource series, timing summaries and raw control samples
are retained. Do not run compilations, tests or other benchmark jobs alongside
the measurements. Caches are left to the OS; this is not a disk-cold test.

## Evidence map

- `metadata.json`, `source.patch`: measured source and binary hashes, Git state,
  exact tracked/untracked source changes, AC/thermal observations. All sources
  and binaries were verified unchanged throughout the paired run.
- `jobs.json`, `r*-*.status.json`, logs/resources: six successful primary runs,
  alternating engine order; every stage retains its `summary.json`, per-op
  metrics, resource series and time series.
- `sqlite-pool-jobs.json`, `sqlite-pool-control.json`, `sqlite-pooled-r*`:
  three successful FIFO controls and their unchanged-source/correctness checks.
- `samples-*.csv.gz`, per-stage `user-latency-audit.json`: paired raw control
  timings, SHA-256 receipts and exact request latency percentiles.
- `audit.py`, `audit.json`: an independent, stricter gate against the better
  SQLite configuration, including conservative bounds on EliteSQL user p99.
- `check.json` in every run: post-stop/reopened checks. EliteSQL's initial
  700,878–712,128 warnings were recoverable derived-index state; zero errors,
  then zero warnings/errors after reopening. Reopen + clean check took
  15.78–15.91 seconds outside throughput.
- `durability-tests.log`, `acceptance.log`, `verification.json`: focused tests,
  complete acceptance and final verification. The separate
  `durable-advantage-pilot-2026-09-26` directory retains the initial one-repetition
  15-second pilot; it is not part of the published medians.

## Latency correction and provenance

The measured simulator originally sorted query times and pool waits separately
before combining them. Its DB p99 is correct; its combined user p99 was not.
The current simulator computes paired total latency before those sorts and has
a regression test where marginal p99s must not be added. This aggregation-only
fix and acceptance integration were applied **after all measured runs**.

The proof does not depend on the erroneous field. For primary runs, use the
exact DB p99 and the conservative bound
`true user p99 <= DB p99 + maximum pool wait`; for FIFO controls, exact paired
raw samples were recomputed, their receipts retained, and their derived reports
regenerated. `generated-results.md` preserves the initial unreviewed output;
the audited report uses the correct methods. Original primary reports' `usr p99`
fields should therefore not be used as exact combined percentiles.

`post-measurement.patch` records the final implementation/documentation relative
to the release. The measured snapshot stays in `source.patch` and metadata;
the new aggregation formula changes reporting, not the service's SQL, operation
mix, native engine or measurement window.
