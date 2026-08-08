# EliteSQL stress test

The mixed SQL stress test runs concurrent `SELECT`, `INSERT`, `UPDATE` and
`DELETE` statements continuously and validates both physical integrity and
logical correctness. It is intended to answer more than whether the process
stays alive: a successful run also rules out detected lost writes, phantom
rows, torn records and incorrect query results for the generated workload.

## Reference command

From the repository root:

```bash
cargo run --release -p elitesql-core --example stress -- --duration 3m
```

Defaults:

- four concurrent workers;
- three minutes;
- `Durability::Safe`;
- 100 initial live rows per worker;
- automatic checkpoints every 256 KiB of committed data;
- 50% `SELECT`, 15% `INSERT`, 20% `UPDATE` and 15% `DELETE`;
- deterministic pseudo-random seed;
- database retained under `target/stress-runs/`.

Use the two-second mode to verify the harness itself:

```bash
cargo run --release -p elitesql-core --example stress -- --smoke
```

Run `cargo run --release -p elitesql-core --example stress -- --help` for all
options. For example, this exercises eight workers for ten minutes and stores
the database at an explicit new path:

```bash
cargo run --release -p elitesql-core --example stress -- \
  --duration 10m --workers 8 --durability safe \
  --checkpoint-bytes 256k --seed 42 --path target/stress-10m.esql
```

The harness refuses to overwrite an existing path.

## What is checked

Each worker owns a disjoint key range, which permits real concurrent writes
without serializing the workload behind the test's reference model. It keeps
its expected rows in memory and verifies point selects plus periodic unindexed
owner scans while mutations continue. The generated IDs and payloads are
deterministic.

After the timed workload, the harness:

1. compares the complete live table with the combined in-memory model;
2. performs an explicit checkpoint and compares it again;
3. compacts the database and compares it again;
4. closes every database handle;
5. runs EliteSQL's offline checksum and structure verifier, requiring zero
   errors and zero warnings;
6. reopens the database and compares every surviving row one final time.

Only then does it print `PASS`. It also reports operation counts, throughput,
checkpoint activity, final live-row count, database size and retained path.

## Observed reference run

The default three-minute command was run on 2026-08-07 on the reference Apple
M5 development machine. It completed successfully:

| Metric | Result |
|---|---:|
| Duration | 180.014 s |
| Workers | 4 |
| Durability | `safe` |
| Total operations | 91,226 |
| Throughput | 507 ops/s |
| `SELECT` | 45,754 |
| `INSERT` | 13,830 |
| `UPDATE` | 18,069 |
| `DELETE` | 13,573 |
| Automatic/explicit checkpoints | 28 |
| Cumulative checkpoint time | 0.456 s |
| Final live rows | 657 |
| Size after compaction | 147.70 KiB |

The online model comparisons, post-checkpoint comparison, post-compaction
comparison, offline integrity check and post-reopen comparison all passed. The
offline check returned zero errors and zero warnings. This throughput is
diagnostic output from a correctness-oriented workload with safe durability;
it is not intended to replace the controlled performance benchmarks.

This test covers clean sustained operation. The separate `crash_kill` test
covers abrupt `SIGKILL` recovery and atomicity of acknowledged transactions.
