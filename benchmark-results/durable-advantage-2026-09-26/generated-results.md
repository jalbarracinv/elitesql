# Durable Mini-SaaS: EliteSQL vs SQLite

Medians [minimum–maximum], not confidence intervals. User p99 includes pool wait.

| Users | EliteSQL ops/s | SQLite ops/s | Throughput ratio | EliteSQL p99 ms | SQLite p99 ms |
| ---: | ---: | ---: | ---: | ---: | ---: |
| 10 | 3,157.7 [2,935.5–3,226.3] | 604.3 [592.0–659.4] | 5.23× | 14.9 [14.0–16.8] | 296.5 [239.0–387.5] |
| 100 | 15,147.5 [15,040.2–15,503.7] | 504.7 [458.3–572.9] | 30.01× | 33.3 [33.2–34.4] | 4,207.3 [3,754.1–4,483.5] |
| 500 | 16,956.4 [16,470.7–16,962.7] | 500.0 [464.7–534.1] | 33.91× | 167.8 [157.1–169.3] | 15,653.8 [13,889.8–16,374.7] |

Correctness gate: **True**. Predeclared performance gate: **True**.

Safe vs WAL/FULL; fullfsync and checkpoint_fullfsync enabled on SQLite. Automatic checkpoints stay enabled; EliteSQL uses its default resource budgets. Same baseline service, 20K accounts, 5K products, 15 operations; recommendations excluded. Text rankings are engine-specific. Sidecar vs embedded SQLite; transport differs. Ten generator processes, no think time. Fresh database per repetition; levels accumulate.

See jobs.json for exact commands, metadata.json for source/binary hashes, each stage's summary.json/resources.csv/timeseries.csv for latency, CPU, RSS, errors and group-commit counters; check.json records recovery outside the timed window. Engine counters cover the entire stage, including ramp and warmup, rather than only the measurement window.
