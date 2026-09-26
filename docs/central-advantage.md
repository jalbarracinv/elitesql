# A demonstrated central advantage: durable concurrent transactions

**EliteSQL can confirm many small, concurrent transactions durably without
requiring the application to batch its requests.** On the tested M5 Mac,
the Mini-SaaS achieved about 24×/27× the best tested SQLite throughput at
100/500 closed-loop users. Every paired repetition favored EliteSQL, including
a SQLite connection-pool control. This finding identifies a concrete product
strength; it does not establish superiority over every SQLite workload or
over other database engines.

## Evidence

Same fifteen-operation ecommerce service, 20K accounts and 5K products;
recommendations excluded because the engines answer different questions.
Text search uses native BM25/FTS5 rankings, which are not identical. Transactions
include cart changes, checkout, stock, loyalty credits and auditing; session
checks also update their last-seen timestamp. The simulator's “read operation”
classification therefore does not mean every such request is read-only.

| Users | EliteSQL Safe ops/s [range] | Best tested SQLite FULL ops/s [range] | Ratio |
| ---: | ---: | ---: | ---: |
| 10 | 3,157.7 [2,935.5–3,226.3] | 624.6 [613.7–643.9] | 5.06× |
| 100 | 15,147.5 [15,040.2–15,503.7] | 633.7 [605.8–642.4] | 23.90× |
| 500 | 16,956.4 [16,470.7–16,962.7] | 623.1 [620.2–633.4] | 27.21× |

Best tested SQLite is the FIFO configuration in each row; the direct baseline
is also retained. Values are medians [min–max] of three fresh runs, not confidence
intervals. At 100/500 users, EliteSQL's median per-run user p99 is bounded above
by 33.6/169.0 ms; SQLite FIFO's exact corresponding medians are 205.1/923.2 ms.
The conservative latency advantage exceeds 5× in **every paired repetition**
at those levels. At 10 users the throughput advantage is repeatable, but the
2× latency criterion does not pass in every repetition.

All 27 stages (nine runs × three levels) completed with zero failed operations,
zero read-your-writes violations and passing business invariants. Every final
database passed its offline integrity check after recovery. The existing
group-commit/checkpoint tests and a new real-process SIGKILL test verify
acknowledged canonical transactions and transaction atomicity; the new test
requires observed shared sync groups before every kill. Twelve initial rounds
passed with at least 392 new acknowledgements checked. These are process-crash
tests, not physical power-cut experiments.

## Why the advantage appears

SQLite WAL/FULL synchronizes its WAL after each transaction commit;
WAL/NORMAL permits losing recent commits after an OS crash or power failure.
On macOS, fullfsync requests the stronger F_FULLFSYNC barrier.
[SQLite's durability documentation](https://www.sqlite.org/pragma.html#pragma_synchronous)
and [fullfsync documentation](https://www.sqlite.org/pragma.html#pragma_fullfsync)
describe those contracts.

Both tested engines request the strict barrier. EliteSQL retains individual
transaction validation and acknowledgement while sharing WAL synchronization:
approximately 25 commits/sync at 100 users and 39 at 500, measured over the
whole stage including ramp/warmup. SQLite FIFO greatly reduces lock starvation
and user latency, but its individually committed transactions still pay the
barrier individually. Its generator consumes about a quarter of one CPU core,
consistent with a durability bottleneck rather than a Python CPU ceiling.

This is a known database technique, not a new algorithm. Its value here is
making it available through a local engine and ordinary transactions, using
the unchanged service code. It gives the current architecture a measurable
reason to exist, rather than requiring a complete rewrite to find one.

## Product implication and costs

The proposition to develop is **a local operational database for bursts of
concurrent state changes that must be synchronized before acknowledgement**:
job transitions, durable audit/event capture, or application transactions
where losing accepted updates is unacceptable. These are proposed target
applications; the actual measured application is the Mini-SaaS above.

At the achieved 500-user capacities, median mean server+generator use was
7.47 CPU cores and 805 MiB RSS for EliteSQL, versus 0.26 cores and 57 MiB for
SQLite FIFO. Different completed work and accumulated database sizes prevent
interpreting this as an equal-work efficiency comparison. EliteSQL's reopen
plus clean integrity check took 15.8–15.9 s after the sidecar was stopped
without clean close. That recovery cost is excluded from throughput. Memory,
CPU and recovery are material costs to improve while preserving the durable
transaction advantage.

The scope is one Apple M5, 16 GiB RAM, macOS 26.6.2, Python 3.14.7 and SQLite
3.53.4; three repetitions with 30 measured seconds per level, no think time.
EliteSQL uses a Unix-socket sidecar, SQLite is embedded. The FIFO control uses
one generator process/connection instead of ten processes/up to one connection
per user. Both retain automatic checkpoints and their configured caches.
The two transaction implementations differ in coordination/isolation mechanisms;
application invariants and durability settings are verified, not universal SQL
isolation equivalence. This is not a maximum-production-user claim, an overnight
steady-state test, or a guarantee of the same factors on Linux/storage with
cheaper barriers. Application batching or partitioned SQLite databases would
be different deployment strategies requiring their own comparisons.

The earlier NORMAL-mode and isolated-operation measurements remain in
[benchmark.md](../benchmark.md). Applications accepting weaker per-commit
durability should evaluate those results. SQL/text/vector integration remains
useful, but this report's central claim is specifically supported by durable
transaction measurements.

## Reproduce and audit

[Commands, hashes, source snapshot and raw measurements](../benchmark-results/durable-advantage-2026-09-26/README.md).
The [independent audit](../benchmark-results/durable-advantage-2026-09-26/audit.json)
passes a stricter gate against the best SQLite configuration: at least 2×
median throughput and a conservative 2× p99 improvement at 100/500 users,
every paired throughput comparison above 1, and zero correctness failures.
The observed margins substantially exceed those thresholds.
