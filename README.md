<p align="center">
  <img src="elitesql.webp" alt="EliteSQL logo" width="240">
</p>

# EliteSQL

> **Alpha release 0.1** — EliteSQL is under active development and its APIs and on-disk format may still change.

> **A tiny operational database for AI-native apps.**
> SQLite-fast reads, better concurrent writes, native ANN.

EliteSQL is an **embedded** database engine (no enforced server, no daemon, no ceremonial tuning) written in Rust. A database is a self-contained directory you can copy, back up and move. When several processes need it, or the app runs on another machine, the same binary can [serve it](#multi-worker-and-remote-the-sidecar-mode) over a socket or a port — a deployment option, not a requirement.

It does not compete with big db projects like PostgreSQL or MySQL: it competes against the complexity of operating this tower in a modern app:

```text
SQLite + vector DB + cache + sync layer + files + embeddings metadata
```

But also it delivers [strong performance](benchmark.md) enough to handle millions of records and thousands of concurrent operations.

The promise is opening a single file and having records, JSON, blobs, indexes, ANN vector search, snapshots and sane concurrency inside:

```text
db.open("app.esql")
```

## Why EliteSQL

- **Real concurrent writes.** SQLite serializes all writers behind a global lock. EliteSQL uses MVCC with optimistic commits: writers prepare transactions in parallel and only meet at commit (`Readers never block writers. Writers only meet at commit.`).
- **Native vectors**: `vector<float32, N>` and an HNSW index as a first-class type, not a bolted-on extension.
- **A bounded resource footprint.** SQL operators, scalar/text/vector indexes,
  recovery and maintenance share an explicit database-wide memory budget.
  Immutable index bases are paged through read-only `mmap`; large sorts,
  aggregates and unindexed joins spill instead of assuming that RAM scales with
  the database.
- **Fail-safe by design.** Checksummed WAL, atomic manifest with a fallback (`manifest.prev`), idempotent replay and automatic recovery: after a crash, the database opens to the last fully committed state. A commit is either fully visible or not visible at all.
- **Small surface.** CRUD, filters, indexes, transactions, snapshots. Not a PostgreSQL recreation.

## Current status

| Phase | Contents | Status |
|---|---|---|
| Phase 0 | Append-only prototype + benchmarks vs SQLite | Complete |
| Phase 1 | WAL, manifest, MVCC, transactions, indexes, crash recovery | Complete |
| Phase 2 | Minimal SQL dialect (see [manual.md](manual.md)) | Complete |
| Phase 2.5 | Aggregates (COUNT/SUM/AVG/MIN/MAX, GROUP BY, HAVING) and date/time types | Complete |
| Phase 3 | Vector type + ANN search (our own HNSW, persisted graph) | Complete |
| Phase 4 | C ABI (with snapshots), Python/Node bindings, CLI, repair, read-only, sidecar, docs | Complete |
| Phase 5 | BM25 full-text, hybrid search (RRF), int8 vectors, blob chunking | Complete |
| Cross-cutting | Database-wide memory governor, bounded SQL/index maintenance, typed SQL parameters | Complete |

Current verification: 269 total Rust and doc tests (MVCC, recovery, sorted bulk loading, bounded-memory execution,
compaction, salvage, backup/restore, randomized model, SQL and parameter suites,
query plans, three-valued NULL logic, text collation, sidecar auth and transport
parity, vector recall, BM25/hybrid, blobs and read-only), crash injection with real
`kill -9` of live processes, corruption and SQL-parser fuzzing, plus Python FFI
parameter tests and Node encoding checks. Onboarding docs in [docs/](docs/).
Details in [specs.md](specs.md) and [plan.md](plan.md).

For a sustained mixed SQL workload, run the concurrent stress test. It checks
every operation against an independent in-memory model, then checkpoints,
compacts, closes, performs an offline integrity check, reopens and compares
every surviving row:

```bash
# Quick harness check
cargo run --release -p elitesql-core --example stress -- --smoke

# Three-minute reference run (safe durability)
cargo run --release -p elitesql-core --example stress -- --duration 3m
```

The generated database is retained under `target/stress-runs/` for inspection.
See [stress-test.md](stress-test.md) for the workload and all options.

## Quick installation

Requirements: [Rust](https://rustup.rs) 1.89 or newer. We recommend installing
Rust with `rustup` instead of an operating-system package, which may provide an
older version of Cargo:

```bash
curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs \
  | sh -s -- -y --default-toolchain 1.89.0 --profile minimal
source "$HOME/.cargo/env"
rustc --version
```

Install the `elitesql` command:

```bash
git clone https://github.com/jalbarracinv/elitesql.git
cd elitesql
cargo install --locked --path crates/elitesql-cli
elitesql --help
```

`cargo build --release` only creates `target/release/elitesql`; it does not add
the command to your `PATH`. `cargo install` copies it to `~/.cargo/bin`, which
`rustup` adds to your `PATH`.

To update an existing installation when a new release is available:

```bash
cd elitesql
git pull --ff-only
cargo install --locked --path crates/elitesql-cli --force
```

For development, run the test suite and benchmarks from the repository:

```bash
cargo test --locked
cargo bench --locked
```

To use it as a dependency in another Rust project:

```toml
[dependencies]
elitesql-core = { path = "../elitesql/crates/elitesql-core" }
# or straight from git:
# elitesql-core = { git = "<repo-url>" }
```

## Quick start

```rust
use elitesql_core::{Column, ColumnType, Db, Record, TableSchema, Value};

fn main() -> elitesql_core::Result<()> {
    let db = Db::open_or_create("app.esql")?;

    db.create_table(TableSchema::new(
        "docs",
        vec![
            Column::new("title", ColumnType::Text).not_null(),
            Column::new("score", ColumnType::Int64),
            Column::new("meta", ColumnType::Json),
        ],
    ))?;
    db.create_index("docs", "title", false)?;

    // Simple write (auto-commit). The id is a ULID generated by the engine.
    let mut rec = Record::new();
    rec.insert("title".into(), Value::Text("hello".into()));
    rec.insert("score".into(), Value::Int64(10));
    let id = db.insert("docs", rec)?;

    // Multi-operation transaction: atomic, isolated, optimistically
    // validated at commit (Error::Conflict => retry).
    let mut txn = db.begin();
    let mut patch = Record::new();
    patch.insert("score".into(), Value::Int64(99));
    txn.update("docs", &id, patch)?;
    txn.commit()?;

    // Snapshots: stable reads while others write.
    let snap = db.snapshot();
    let current = db.get("docs", &id)?.unwrap();
    let at_snapshot = db.get_at(&snap, "docs", &id)?.unwrap();
    assert_eq!(current["score"], at_snapshot["score"]);

    // Equality lookup (uses the secondary index when one exists).
    let hits = db.find_eq("docs", "title", &Value::Text("hello".into()))?;
    assert_eq!(hits.len(), 1);
    Ok(())
}
```

### Vector search (ANN)

Embeddings as a first-class type, with our own HNSW and metadata filters:

```rust
use elitesql_core::{Column, ColumnType, TableSchema, VectorIndexOptions, VectorSearchOptions};

db.create_table(TableSchema::new(
    "notes",
    vec![
        Column::new("body", ColumnType::Text).not_null(),
        Column::new("workspace", ColumnType::Text),
        Column::vector("embedding", 768),
    ],
))?;
db.create_vector_index("notes", "embedding", VectorIndexOptions::default())?; // cosine, sync

// ... insert records with Value::Vector(...) ...

let mut filter = elitesql_core::Record::new();
filter.insert("workspace".into(), Value::Text("acme".into()));
let hits = db.search_vector(
    "notes", "embedding", &query_embedding, 20,
    &VectorSearchOptions { filter: Some(filter), ..Default::default() },
)?;
for hit in hits {
    println!("{} (dist {:.3})", hit.id, hit.distance);
}
```

The current 100K-vector synthetic benchmark (dim 64) obtains recall@10 of
0.994 at `ef_search=128` and 1.0 at 256, with mean search intervals of about
2.0 ms and 3.4 ms respectively. An `Async` mode is available so commits do not
wait for indexing, plus a `quantized` (int8) option for roughly 4x smaller
vector payloads. See [benchmark.md](benchmark.md) for the measured memory,
latency, and quality trade-offs.

### Full-text and hybrid

```rust
db.create_text_index("notes", "body")?;                    // BM25
let hits = db.search_text("notes", "body", "query", 10, None)?;
let hits = db.search_hybrid("notes", &HybridQuery {        // RRF: text + vector
    text: Some(("body", "query")),
    vector: Some(("emb", &embedding)),
    top_k: 10,
    ..Default::default()
})?;
```

### SQL

The same engine exposes a deliberately small SQL dialect — full reference with examples in [manual.md](manual.md), and a migration guide for people arriving from MySQL in [mysql2elite.md](mysql2elite.md):

```rust
use elitesql_core::{QueryOutput, Record, Value};

db.query("CREATE TABLE users (name text NOT NULL, email text, age int, since date)")?;
db.query("CREATE UNIQUE INDEX ON users (email)")?;
db.query("INSERT INTO users (name, email, age, since) VALUES ('ana', 'ana@x.com', 30, '2026-08-07')")?;

if let QueryOutput::Rows { columns, rows } = db.query(
    "SELECT u.name, o.amount FROM users u \
     JOIN orders o ON o.user_id = u.id \
     WHERE u.email = 'ana@x.com' ORDER BY o.amount DESC LIMIT 10",
)? {
    // ...
}

// Aggregates with GROUP BY/HAVING and date-range filters:
db.query(
    "SELECT age, count(*) AS n FROM users \
     WHERE since >= '2026-01-01' GROUP BY age HAVING count(*) > 1 ORDER BY n DESC",
)?;

// Parameters are parsed and bound as typed values, never interpolated.
db.query_params(
    "SELECT name FROM users WHERE email = %s LIMIT ?",
    &[Value::Text("ana@x.com".into()), Value::Int64(10)],
)?;

let mut params = Record::new();
params.insert("email".into(), Value::Text("ana@x.com".into()));
params.insert("limit".into(), Value::Int64(10));
db.query_named_params(
    "SELECT name FROM users WHERE email = %(email)s LIMIT %(limit)s",
    &params,
)?;
```

## CLI

```bash
cargo build --release -p elitesql-cli     # produces target/release/elitesql

elitesql --create app.esql               # create a new database (once)
elitesql query app.esql "SELECT count(*) AS n FROM docs"
elitesql app.esql                       # interactive shell (SQLite-style shorthand)
elitesql repl app.esql                  # interactive shell (.exit to quit)
elitesql tables app.esql                # schemas as JSON
elitesql check app.esql                 # offline integrity check
elitesql compact app.esql
elitesql backup app.esql backup.esql    # snapshot-consistent copy, verified
elitesql restore backup.esql app.esql   # validate a backup and materialize it
elitesql export app.esql docs > docs.jsonl
elitesql import app.esql docs < docs.jsonl
elitesql repair damaged.esql rescued.esql    # salvage, never silent
elitesql serve app.esql /tmp/elitesql.sock   # sidecar mode
```

The interactive shell buffers SQL across lines until it finds a terminating
`;` outside string literals, `--` comments, and `/* ... */` comments.

Opening never creates a database on its own — `--create` does, once. A single
argument that is not a subcommand is read as a database path, so without this a
mistyped subcommand (`elitesql versio`) would silently leave a database
directory named after the typo in the working directory.

## Multi-worker and remote: the sidecar mode

A database directory is owned by **one process**. When several processes need it — or when the app runs on a different host — that process serves them: it owns the engine and answers a line-delimited JSON protocol, one thread per connection over a shared `Db`. Concurrency still comes from the engine: readers never block writers, writers only meet at commit.

### Same host: Unix socket

For multi-process deployments (gunicorn, PHP-FPM), the transport is a Unix socket, authenticated by filesystem permissions:

```bash
elitesql serve app.esql /tmp/elitesql.sock
```

```python
# each gunicorn worker:
from elitesql import SidecarClient
db = SidecarClient("/tmp/elitesql.sock")
db.query("INSERT INTO visits (who) VALUES ('ana')")
db.query("SELECT count(*) AS n FROM visits")
```

Reproducible demo with real gunicorn (4 workers, concurrent visitors reading and writing without blocking): `examples/gunicorn_demo/run_demo.sh`.

### Another host: TCP

```bash
export ELITESQL_TOKEN=$(openssl rand -hex 32)     # or --token-file <path>
elitesql serve app.esql --tcp 127.0.0.1:7070
```

```python
from elitesql import SidecarClient
db = SidecarClient(host="127.0.0.1", port=7070, token=os.environ["ELITESQL_TOKEN"])
db.query("SELECT count(*) AS n FROM visits")
```

```js
const db = await SidecarClient.connect({ host: '127.0.0.1', port: 7070, token });
```

A Unix socket is authenticated by the filesystem; a TCP port is not, so it **requires a token**. The server refuses to start without one, and refuses every request — including `ping` — until a connection sends `{"op":"auth","token":"..."}`. Authentication is per connection, comparison is constant-time, and the token is read from `--token-file` or `ELITESQL_TOKEN`, never a flag, because `ps` would expose it to every user on the host.

Two limits to plan around, neither of which the Unix socket had:

- **No encryption.** Traffic and the token itself travel in cleartext. Bind loopback and cross machines through an SSH tunnel (`ssh -N -L 7070:127.0.0.1:7070 user@db-host`), a VPN, or a private network. The server warns on startup when the bind address is not loopback.
- **Latency changes the performance profile.** A point lookup is ~4 µs; a network round trip is ~0.5 ms in a datacenter and 10–50 ms across regions. Over TCP the network dominates by orders of magnitude and the engine stops behaving like an embedded one. If you only need several workers, keep them on one host with the Unix socket.

`--max-connections` (default 128) caps concurrent connections on **both** transports, since each one costs a thread; past the cap the server answers with a refusal instead of queueing.

**Do not** put a database directory on NFS or SMB and open it from two machines. Durability relies on `fsync` plus atomic `rename`, and immutable index bases are read through `mmap`; network filesystems do not provide either reliably. The sidecar is the supported way to reach a database from elsewhere.

### Same behavior, different transport

The SQL is identical over a Unix socket, over TCP, and embedded: it is the same engine in the same process. Same dialect, same MVCC and read-committed reads, same automatic retry on UPDATE/DELETE, same atomic multi-row INSERT, same error codes. A query does not behave differently because it arrived over a socket — see [manual.md](manual.md#running-sql-from-another-process-or-another-host).

What the server mode is **not**, so the boundaries are clear:

- **One database per server process.** There is no `USE db` and no database listing: `serve` opens the directory you name and serves that one. Several databases mean several processes on different ports, each with its own memory budget — the memory governor is per-`Db`, so a single process serving many databases would break the bounded-footprint guarantee.
- **No TLS.** The token and the data travel in cleartext; a tunnel or a private network is doing the encrypting.
- **No session transactions.** No `BEGIN`/`COMMIT` over the wire, the same as embedded SQL. Multi-statement transactions use the Rust `Txn` API in the process that owns the database.
- **No replication, no failover, no connection multiplexing.** One process owns the directory; if it is down, the database is unreachable.

Use TCP when the app genuinely has to live on another machine — to give it separate resources, for instance. To scale workers, keep them on one host with the Unix socket and skip the network entirely.

## Bindings

**Python** ([bindings/python/elitesql.py](bindings/python/elitesql.py)) — embedded via the C ABI (ctypes releases the GIL on every call: threads truly parallelize) or via the sidecar:

```python
from elitesql import EliteSQL

with EliteSQL("app.esql") as db:
    db.query("CREATE TABLE notes (body text NOT NULL, emb vector(768))")
    db.create_vector_index("notes", "emb", metric="cosine")
    db.query("INSERT INTO notes (body, emb) VALUES (%s, %s)", ["hello", embedding])
    rows = db.query(
        "SELECT * FROM notes WHERE body = %(body)s LIMIT %(limit)s",
        {"body": "hello", "limit": 10},
    )
    hits = db.search_vector("notes", "emb", embedding, top_k=10, filter={"ws": "acme"})
```

Positional `?`/`%s` and named `%(name)s` placeholders are supported by the
Rust API, C ABI, embedded Python binding, sidecar protocol and Node client.
Parameter count and names are validated strictly; strings containing quotes or
SQL syntax stay data and cannot alter the parsed statement. Binding preserves
nulls, booleans, signed 64-bit integers, floats, text, blobs,
date/time/timestamp, JSON and vectors. `LIMIT` and `OFFSET` may be parameters
but must receive a non-negative `int64`. Rust also provides parameterized
cursor variants for large results.

**Node** ([bindings/node/elitesql.js](bindings/node/elitesql.js)) — dependency-free sidecar client:

```js
const { SidecarClient } = require('./elitesql');
const db = await SidecarClient.connect('/tmp/elitesql.sock');
const { rows } = await db.query('SELECT * FROM notes WHERE body = %s LIMIT %s', ['hello', 10]);
const hits = await db.searchVector('notes', 'emb', embedding, { topK: 10 });
```

**C** — header at [crates/elitesql-ffi/include/elitesql.h](crates/elitesql-ffi/include/elitesql.h); `cargo build --release -p elitesql-ffi` produces `libelitesql`.

## Durability

| Mode | fsync | On process crash | On OS crash |
|---|---|---|---|
| `Safe` (default) | Every commit | Loses nothing | Loses nothing |
| `Balanced` | Every ~25 ms | Loses nothing | May lose the last few ms |
| `Fast` | Checkpoints only | Loses nothing | May lose recent commits |

```rust
use elitesql_core::{Db, DbOptions, Durability};
let opts = DbOptions { durability: Durability::Balanced, ..Default::default() };
let db = Db::open_or_create_with("app.esql", opts)?;
```

## Database-wide memory budget

Every open database owns one shared governor. The default 384 MiB envelope is
partitioned into a 64 MiB concurrent-query pool, a 128 MiB mutable-index pool,
a 128 MiB maintenance pool and an 8 MiB emergency reserve, with the remainder
left as allocator/runtime headroom. Clean file-backed `mmap` pages and result
values already handed to the caller are not charged.
Traditional SQL queries are subject to a working-memory budget too. Scans run
in batches; `ORDER BY` and high-cardinality `GROUP BY` spill temporary sorted
runs, while unindexed equality joins use a partitioned Grace Hash Join with a
bounded skew fallback instead of retaining a complete build relation.
Use `Db::query_cursor` for a large unordered result so the caller does not have
to materialize every returned row.

The complete index is therefore not required to be heap-resident. Canonical
vectors remain exact at their declared dimension (256, 768, 1024 or higher),
while the persisted HNSW base is searched through `mmap` and only bounded recent
deltas are mutable. Dimensionality reduction is not part of the mandatory
storage path; int8 quantization remains an explicit optional index tradeoff.

```rust
use elitesql_core::{DbOptions, MemoryOptions};

let opts = DbOptions {
    memory: MemoryOptions {
        total_memory_bytes: 384 * 1024 * 1024,
        query_pool_bytes: 64 * 1024 * 1024,
        query_working_bytes: 16 * 1024 * 1024,
        index_delta_pool_bytes: 128 * 1024 * 1024,
        maintenance_pool_bytes: 128 * 1024 * 1024,
        reserved_memory_bytes: 8 * 1024 * 1024,
        scan_batch_rows: 512,
        spill_directory: None,
    },
    ..DbOptions::default()
};
```

`Db::query_memory_stats()` exposes spill-file count, bytes spilled and the
largest estimated operator buffer. `Db::global_memory_stats()` reports current
and peak pool use, waits and index consolidations. Concurrent queries wait when
the query pool is full; intrinsically oversized transactions/search requests
return `Error::MemoryLimit` before publishing a commit.

More memory can improve sustained ingest, but the useful knobs are workload
specific. Raise `memtable_max_bytes` and `index_delta_pool_bytes` to publish
fewer, larger deltas, and raise `maintenance_pool_bytes` when index construction
or compaction needs a larger bounded workspace. Increasing the query pool does
not accelerate inserts. The 384 MiB default was measured to retain a complete
100K x 64-dimensional HNSW graph and avoid restart catch-up on an AWS
`t3.large`; smaller deployments can explicitly select a tighter envelope.

For the larger 512 MiB ingest profile, use:

```rust
let opts = DbOptions::ingest_performance();
```

It keeps query admission at 64 MiB, assigns 192 MiB each to index deltas and
maintenance, and uses a 128 MiB memtable target. It is a convenience preset,
not an adaptive reservation.

For an initial or append-only import whose explicit text IDs are already in
strictly increasing order, `Db::bulk_insert_sorted(table, records)` is the
preferred path. It streams one canonical segment and one primary run under the
maintenance budget instead of building a WAL/memtable generation per batch.
The target table must not yet have equality, text or vector indexes; create
those after loading. Invalid or duplicate ordering is rejected without partial
publication, and the whole imported batch becomes visible at one commit
version.

The MVCC primary directory is a set of immutable, checksummed paged runs opened
read-only with `mmap`, plus a bounded mutable delta. For primary-only workloads,
an automatic checkpoint moves that delta to one immutable frozen generation in
O(1), lets commits continue in a fresh active delta, and flushes the frozen
generation on a dedicated worker. Reads merge active + frozen + mmap runs until
publication. The frozen heap and writer own the maintenance-pool reservation,
so there is never an unbudgeted second memtable. At publication EliteSQL copies
the record-aligned WAL tail written after the freeze into the next WAL before
swapping the manifest. Explicit checkpoints, DDL, compaction and close wait for
the worker. Tables with equality, BM25 or vector deltas currently retain the
synchronous checkpoint path. A background worker promotes groups of sixteen same-level primary
runs, while equality/BM25 retain fanout eight. Disjoint V2 primary ranges copy
their already checksummed pages directly instead of decoding and rebuilding
every entry. The atomic `primary.runs` manifest selects one exact generation.
Paged format V2 retains only one small offset per page in heap; keys remain
file-backed. Checkpoint snapshots intern table names and pack IDs contiguously,
and generate the primary run directly from captured segment offsets instead of
updating and rescanning the mutable tree. Missing, stale or damaged run state is
disposable and rebuilt from canonical segments with bounded external runs.
Secondary equality and BM25 indexes use the same leveled scheme. Their runs
store versioned additions and tombstones, so background promotion can discard
superseded operations without reviving an old value or posting; equality uses
a bounded k-way cursor and BM25 also persists exact document-count/token-length
statistics. HNSW uses immutable mmap graph runs. Checkpoint and data
compaction share the maintenance pool, and compaction streams its output rather
than duplicating the database in heap memory. A read-only open reuses valid
mapped indexes; if recovery would require a resident delta larger than its pool
it returns `Error::MemoryLimit` instead of risking an unbounded startup spike.

## Automatic compaction

Compaction is enabled by default and runs on one background maintenance
worker. Updates, deletes, and replaced record versions accumulate compaction
debt; a checkpoint evaluates how much immutable segment space can actually be
reclaimed. EliteSQL compacts when both the obsolete-operation threshold
(10,000 rows) and reclaimable ratio (25%) are reached, or when reclaimable
space reaches 256 MiB, or the database reaches 64 segments. Automatic attempts
are rate-limited to one per minute.

Live snapshots are always preserved. Reads continue during most of the rewrite;
writes wait behind the final serialized maintenance operation. `Db::compact()`
and `elitesql compact app.esql` remain available for an immediate manual run.
The policy can be tuned—or disabled for controlled benchmarks—through
`DbOptions::auto_compaction`:

```rust
use elitesql_core::{AutoCompactionOptions, DbOptions};

let opts = DbOptions {
    auto_compaction: AutoCompactionOptions {
        min_obsolete_operations: 50_000,
        ..AutoCompactionOptions::default()
    },
    ..DbOptions::default()
};
```

`Db::maintenance_stats()` reports the current debt, estimated reclaimable
bytes, segment count, completed/failed automatic compactions, elapsed time, and
bytes reclaimed, plus current run counts and checkpoint/promotion bytes for
primary, equality and BM25. The `wait_for_*_compaction()` barriers provide
explicit graceful-shutdown/testing synchronization.

## On-disk format

```text
app.esql/
  ELITESQL        # marker + format_version
  LOCK            # process exclusion (flock)
  catalog.json    # tables, columns, indexes
  manifest        # atomic pointer to the visible state
  manifest.prev   # recovery fallback
  wal/            # durable commits (per-record CRC)
  segments/       # immutable data (per-entry CRC)
  indexes/        # paged primary/equality/BM25 files
    primary.runs  # atomic generation + active primary run set
    primary.pidx  # stable primary base
    primary-L*    # immutable primary deltas and promoted runs
    *.sidx.runs   # equality run manifests; *.sidx.run are immutable levels
    *.tidx.runs   # BM25 run manifests; *.tidx.run are immutable levels
  vectors/        # persisted ANN graphs (CRC; disposable and rebuildable)
  blobs/          # out-of-line blob chunks (CRC)
```

The engine's golden rule: `Data files are canonical. Indexes are disposable.` If an index breaks, it is rebuilt from data; if the manifest breaks, the previous one is used; if the WAL has an incomplete entry, the whole entry is discarded (never half a commit).

## Integrity checking

```rust
let report = elitesql_core::check("app.esql")?;
assert!(report.is_ok());
```

To run the crash-injection suite and fuzzing with more iterations:

```bash
ELITESQL_CRASH_ITERS=500 cargo test --release --test crash_kill
ELITESQL_FUZZ_ITERS=5000 cargo test --release --test corruption
```

## Performance

Current measurements are deliberately published even where they are
unfavorable. The 2026-08-08 Apple Silicon 10M-row runs predate the current
memory defaults: the former 128 MiB transactional profile completed in
22.620 s versus SQLite's 13.663 s (1.66x SQLite, inside the 2x target), while
the former 256 MiB ingest profile completed in 18.798 s (1.38x).
`Db::bulk_insert_sorted` completed in 9.968 s versus SQLite's 13.822 s.
Subsequent point reads and the measured unindexed equality scan favored EliteSQL.

An isolated 10M transactional run stayed inside every logical pool
(16/64 MiB query, 22.81/24 MiB delta and 32/32 MiB maintenance) and reported a
65.56 MiB peak physical footprint. Max RSS was 879.47 MiB because it includes
clean file-backed mmap pages touched during the run; those pages are
reclaimable and intentionally not equivalent to mandatory heap. Remaining
performance work centers on synchronous checkpoint work and p99 commit latency
with four to eight writers. Re-run the scale matrix for the current 384/512 MiB
profiles before comparing them directly with those historical results.

For reproducible comparisons at 1–10 million rows, use the single-run scalable benchmark:

```bash
cargo bench -p elitesql-core --bench scale_vs_sqlite -- --rows 1m
cargo bench -p elitesql-core --bench scale_vs_sqlite -- --rows 10m
```

It gives both engines the same deterministic rows and 10K-row transaction batches. Durability is matched explicitly: EliteSQL `fast` ↔ SQLite WAL/`synchronous=OFF`, `balanced` ↔ `NORMAL`, and `safe` ↔ `FULL`. Use `--bulk-sorted` for the direct import path; `--durability balanced|safe`, `--batch-size`, `--point-reads`, `--full-scans`, `--engine both|elitesql|sqlite`, `--total-memory-mib`, `--index-delta-mib`, `--maintenance-mib`, and `--memtable-mib` change or isolate the workload; `--smoke` runs a quick 10K-row correctness check.

See [benchmark.md](benchmark.md) for the complete methodology, exact environment, timing definitions, reproducible commands, 1M/10M results, and the 1/2/4/8 concurrent-writer comparison with CSV data and SVG charts. The scalable benchmark reports the write path separately from all automatic/final checkpoints and prints `SQLite time / EliteSQL time`; a ratio above 1 means EliteSQL was faster.

## License

Licensed under either of

- Apache License, Version 2.0 ([LICENSE-APACHE](LICENSE-APACHE) or http://www.apache.org/licenses/LICENSE-2.0)
- MIT license ([LICENSE-MIT](LICENSE-MIT) or http://opensource.org/licenses/MIT)

at your option.

Unless you explicitly state otherwise, any contribution intentionally submitted
for inclusion in the work by you, as defined in the Apache-2.0 license, shall be
dual licensed as above, without any additional terms or conditions.
