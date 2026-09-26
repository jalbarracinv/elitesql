# Audited durable Mini-SaaS results

Same service and strict durability. Medians of three fresh runs; ranges and every paired comparison are in [audit.json](audit.json). The best tested SQLite configuration is its single-connection FIFO control.

| Users | EliteSQL ops/s | SQLite direct ops/s | SQLite FIFO ops/s | EliteSQL / best SQLite | EliteSQL user p99 upper bound ms | SQLite FIFO exact user p99 ms |
| ---: | ---: | ---: | ---: | ---: | ---: | ---: |
| 10 | 3,157.7 | 604.3 | 624.6 | 5.06× | 15.0 | 31.1 |
| 100 | 15,147.5 | 504.7 | 633.7 | 23.90× | 33.6 | 205.1 |
| 500 | 16,956.4 | 500.0 | 623.1 | 27.21× | 169.0 | 923.2 |

All 27 stages: zero failed operations, zero read-your-writes violations, business invariants passed; all final reopened integrity checks passed. The stricter gate passed at 100/500 users, including every paired throughput comparison and a conservative >=2x true-user-p99 improvement.

Latency method: exact DB p99 + maximum nonnegative pool wait bounds each EliteSQL run's true user p99 above; the displayed median bound is rounded up. SQLite FIFO uses exact paired raw samples. See [the latency correction](README.md#latency-correction-and-provenance); the initial combined percentile fields are not used as exact measurements.

EliteSQL combines server plus generators; SQLite FIFO has one generator process. At 500 users their median mean total CPU/RSS was 7.47 cores / 804.56 MiB and 0.26 cores / 57.22 MiB respectively, at very different completed-work rates and accumulated database sizes. EliteSQL reopen plus clean check took 15.78–15.91 s outside throughput.

[Method, reproduction and raw evidence](README.md). [Product interpretation and limits](../../docs/central-advantage.md).
