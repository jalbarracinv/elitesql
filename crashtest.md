# EliteSQL crash stress test

This report documents the repeated-`SIGKILL` mixed-workload test executed on a
dedicated ARM64 Linux test machine. The objective was to detect lost acknowledged
writes, partial mutations, phantom rows, invalid query results and physical
corruption after repeatedly killing and reopening EliteSQL.

The run passed every validation. It was originally configured for two hours,
but was intentionally stopped after exceeding one million observed operations
to limit unnecessary disk activity.

## Result

**PASS — every completed recovery matched the durable external oracle.**

| Metric | Result |
|---|---:|
| Timed workload before the threshold | 743.1 s (12 min 23.1 s) |
| Observed operations | at least 1,008,342 |
| `SIGKILL`/recovery cycles | 440 |
| Journal-acknowledged mutations | 545,335 |
| Uncertain mutations recovered as committed | 536 |
| Total confirmed mutations | 545,871 |
| Uncertain mutations recovered as aborted | 300 |
| Final live rows | 2,650 |
| Final database/oracle directory | 1.2 MiB |
| Harness log | 8 KiB |
| Final offline integrity check | zero errors, zero warnings |
| Final reopen and full-model comparison | passed |

“Observed operations” is a lower bound. Each child publishes metrics every 250
ms, and `SIGKILL` can discard the unreported tail of a cycle. Mutation counts
come from the fsync-backed oracle journal and recovery reconciliation, so they
are tracked independently from that approximate operations counter.

The final interrupted cycle contributed 585 acknowledged mutations and one
mutation that recovery correctly classified as aborted. It left no partial
line in the oracle journal. The database was then checkpointed, compacted,
checked offline, reopened and compared with all 2,650 expected rows.

## Reference environment

The test ran on a dedicated ARM64 Linux test machine:

| Component | Value |
|---|---|
| Architecture | ARM64 (`aarch64`) |
| CPU | Multi-core ARM64 processor |
| Memory | 8 GiB class |
| Storage | Local NVMe SSD |
| Operating system | Debian-based Linux |
| Kernel | Linux 6.x |
| Rust | Stable ARM64 toolchain |
| EliteSQL | package 0.0.1, Alpha release 0.1 |

Resource utilization remained within the machine’s available memory and storage.
The system did not use swap and remained within normal operating temperatures.

## Workload configuration

The installed test executable was invoked with the equivalent of:

```bash
elitesql-crash-stress \
  --duration 2h \
  --workers 4 \
  --checkpoint-bytes 256k \
  --min-kill 200ms \
  --max-kill 3s \
  --check-every 10 \
  --path /path/to/elitesql-soak/run-data
```

The workload child used four concurrent threads and `Durability::Safe`. Each
thread owned a disjoint ID range, allowing concurrent writes without forcing
the reference model itself to serialize database operations.

The random operation distribution was:

| Operation | Approximate share |
|---|---:|
| Point and unindexed-owner `SELECT` | 50.0% |
| `INSERT` | 15.0% |
| `UPDATE` | 20.0% |
| `DELETE` | 14.8% |
| Explicit compaction | 0.2% |

Automatic checkpoints were triggered after every 256 KiB of committed
memtable data. Explicit random compactions allowed kills to land during storage
maintenance as well as during ordinary SQL activity.

The controller selected a pseudo-random lifetime between 200 ms and 3 s for
each workload process, sent `SIGKILL`, waited for process termination, recovered
the database and verified it before starting the next child.

## Durable oracle protocol

The expected state was not kept only in the process being killed. Every
mutation followed this protocol:

1. Append an `Intent` containing the row's complete before/after states to an
   external checksummed journal.
2. Call `sync_data` on the oracle journal.
3. Execute the SQL mutation against EliteSQL using safe durability.
4. Append and sync a `Commit` event after EliteSQL acknowledges the mutation.

After `SIGKILL`, an intent can legitimately have no commit acknowledgement:
the database commit may or may not have completed before the process died. The
controller reopens EliteSQL and compares the actual row with both states:

- if it equals the `after` state, the mutation is classified as committed;
- if it equals the `before` state, it is classified as aborted;
- any third or partial state fails the test immediately.

Committed events are replayed into an independent `BTreeMap` model. After
resolving every uncertain intent, a full ordered SQL scan must exactly equal
the complete model. The recovered model is atomically persisted and the
per-cycle journal is reset before another workload child starts.

Journal lines contain a CRC32 checksum. A final incomplete line caused by
`SIGKILL` is safe to discard because the corresponding intent or
acknowledgement was not fully persisted; a checksum failure in any complete
line aborts the test.

## Validation performed

The harness performed the following after every killed workload process:

1. EliteSQL WAL/manifest recovery during `Db::open_with`.
2. Replay of acknowledged oracle events.
3. Before/after reconciliation of every unacknowledged intent.
4. Exact comparison of every live database row with the recovered model.
5. An offline checksum/structure check every ten crashes, requiring zero
   errors and zero warnings.

After manually stopping at the one-million-operation threshold, the
`--recover-only` mode completed one final recovery and then:

1. compared the complete database with the oracle;
2. performed an explicit checkpoint and compared again;
3. compacted and compared again;
4. closed all database handles;
5. ran the offline integrity verifier;
6. reopened EliteSQL and compared every row one final time.

The independent CLI check also reported:

```text
ok: database validates
```

No controller or workload process remained running after finalization.

## Artifacts on the test machine

The retained artifacts are:

```text
/path/to/elitesql-soak/run-data
/path/to/elitesql-soak/run.log
```

The finalized database is located at:

```text
/path/to/elitesql-soak/run-data/database.esql
```

The run directory also contains the external oracle snapshot, its reset
journal and the final worker metrics.

To recover and finalize an intentionally interrupted run:

```bash
elitesql-crash-stress \
  --recover-only \
  --path /path/to/elitesql-soak/run-data \
  --workers 4 \
  --checkpoint-bytes 256k \
  --check-every 1
```

## Preflight verification

Before the main run, the ARM64 binary completed an eight-second smoke test on
the same test machine:

| Metric | Result |
|---|---:|
| `SIGKILL`/recovery cycles | 41 |
| Observed operations | at least 5,317 |
| Confirmed mutations | 6,900 |
| Uncertain committed/aborted | 33 / 27 |
| Final live rows | 81 |
| Result | passed |

The updated SQL/DDL implementation was also validated before installation with
43 regular SQL/DDL tests and two `kill -9` DDL tests. All 45 passed. `DELETE`
was exercised by the long mixed workload. The new `ALTER TABLE` operations
were exercised by the separate DDL and DDL-crash suites, not by this long
mixed-workload run.

## Interpretation and limitations

This result is strong evidence that, for this workload and environment,
EliteSQL preserves logical consistency and on-disk integrity across frequent
process termination. More than half a million confirmed mutations survived
or were correctly classified across 440 recovery cycles, with exact model
agreement after every completed recovery.

It does not prove the absence of every corruption bug. In particular:

- only the workload child was killed; the controller performing recovery was
  not itself killed;
- `SIGKILL` does not simulate complete power loss, controller failure or every
  possible storage-device reordering;
- workers used disjoint key ranges, so this run did not intentionally create
  same-row optimistic conflicts;
- it covered one ARM64 machine, filesystem and kernel;
- it stopped after the operation threshold rather than running for the full
  configured two hours;
- long-running mixed DDL/DML was not part of this workload.

Useful follow-up tests would kill recovery itself, add contested-key
transactions, mix DDL with sustained DML and repeat on other filesystems and
hardware. A native `--max-operations` option would also make the disk-write
limit reproducible without an external monitor.
