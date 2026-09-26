# Durable Mini-SaaS: EliteSQL vs SQLite

Medians [minimum–maximum], not confidence intervals. User p99 includes pool wait.

| Users | EliteSQL ops/s | SQLite ops/s | Throughput ratio | EliteSQL p99 ms | SQLite p99 ms |
| ---: | ---: | ---: | ---: | ---: | ---: |
| 10 | 3,028.1 [3,028.1–3,028.1] | 605.9 [605.9–605.9] | 5.00× | 15.0 [15.0–15.0] | 386.1 [386.1–386.1] |
| 100 | 16,098.2 [16,098.2–16,098.2] | 491.1 [491.1–491.1] | 32.78× | 31.6 [31.6–31.6] | 3,851.5 [3,851.5–3,851.5] |

Correctness gate: **True**. Predeclared performance gate: **False**.

Safe vs WAL/FULL; fullfsync and checkpoint_fullfsync enabled on SQLite. Automatic checkpoints stay enabled; EliteSQL uses its default resource budgets. Same baseline service, 20K accounts, 5K products, 15 operations; recommendations excluded. Text rankings are engine-specific. Sidecar vs embedded SQLite; transport differs. Ten generator processes, no think time. Fresh database per repetition; levels accumulate.

See jobs.json for exact commands, metadata.json for source/binary hashes, each stage's summary.json/resources.csv/timeseries.csv for latency, CPU, RSS, errors and group-commit counters; check.json records recovery outside the timed window. Engine counters cover the entire stage, including ramp and warmup, rather than only the measurement window.
