<p align="center">
  <img src="elitesql.webp" alt="EliteSQL logo" width="240">
</p>

# EliteSQL

> **Alpha release 0.1.0** — EliteSQL is under active development and its APIs and on-disk format may still change. This README describes the current source tree; a published wheel may lag behind it.

> **A tiny operational database for AI-native apps.**
> Records, SQL, concurrent transactions and native ANN in one engine.

EliteSQL is an **embedded** database engine (no enforced server, no daemon, no ceremonial tuning) written in Rust and made for **Python** applications first: one `pip install`, one shared library, `import elitesql`. A closed database is a self-contained directory you can copy and move. Rust's `Db::backup()` copies a live database consistently; the backup CLI opens and owns the database for its operation. When several processes need it, or the app runs on another machine, the same binary can [serve it](#multi-worker-and-remote-the-sidecar-mode) over a socket or a port — a deployment option, not a requirement.

It does not compete with big db projects like PostgreSQL or MySQL: it competes against the complexity of operating this tower in a modern app:

```text
SQLite + vector DB + cache + sync layer + files + embeddings metadata
```

The [current benchmark](benchmark.md) compares EliteSQL with SQLite at up to ten million records and in a concurrent mini-SaaS workload, including cases where SQLite is faster.

The promise is opening one database directory and having records, JSON, blobs, indexes, ANN vector search, snapshots and concurrency inside:

```python
from elitesql import EliteSQL
db = EliteSQL("app.esql")
```

## Why EliteSQL

- **Concurrent transaction preparation.** SQLite permits one writer at a time. EliteSQL uses MVCC and optimistic validation: writers stage transactions in parallel, then coordinate publication at commit. Snapshots preserve stable reads while other transactions commit.
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

Verification covers Rust and doc tests (MVCC, recovery, sorted bulk loading, bounded-memory execution,
compaction, salvage, backup/restore, randomized model, SQL and parameter suites,
query plans, three-valued NULL logic, text collation, sidecar auth and transport
parity, vector recall, BM25/hybrid, blobs and read-only), crash injection with real
`kill -9` of live processes, corruption and SQL-parser fuzzing, plus Python FFI
parameter tests and real Node sidecar roundtrips. Run `bash scripts/acceptance.sh`
for formatting, Clippy, the workspace suite, FFI, both clients and external sort
under a 128-descriptor limit. CI defines Linux/macOS and Rust 1.89/1.93.1 jobs,
with Python 3.12 and Node.js 24 for the bindings.
The [implementation report](docs/implementacion-plan.md) records local evidence
and the [audit](docs/auditoria-y-plan-2026-09-10.md) explains the changes.
Onboarding docs in [docs/](docs/).
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

For an application-shaped load test, `examples/saas_simulation/` runs a mini
e-commerce SaaS (logins, catalogue, BM25 and vector search, persistent carts,
checkout with stock, reviews, dashboard) against the engine with 10 to 5 000
concurrent virtual users, verifying business invariants and integrity at every
level, and records the capacity and latency curves:

```bash
cargo build --locked --release -p elitesql-cli -p elitesql-ffi
python3 examples/saas_simulation/sweep.py --transport sidecar --levels 10,100,500,1000,2000,5000
```

The [2026-09-26 comparison](benchmark.md#mini-saas-application) uses 20,000
accounts, 5,000 products and fifteen operations shared by both engines;
recommendations are excluded because their vector/category implementations
answer different questions. Three repetitions at 10/100/500 users, with no
think time and 30 measured seconds per level, give EliteSQL 0.85×/1.24×/1.06×
SQLite's throughput. The 500-user ranges overlap. All runs completed with no
failed operations or read-your-writes violations, and passed business
invariants and offline integrity checks after reopening.

EliteSQL runs over a Unix-socket sidecar in this concurrent test; SQLite is
embedded in the generator processes. The isolated embedded-operation mix
favors SQLite (45.19 µs versus EliteSQL's 72.84 µs weighted median cost).
These are local workload measurements, not a maximum-user capacity claim.

See [examples/saas_simulation/README.md](examples/saas_simulation/README.md)
for simulator options and
[the current raw results](benchmark-results/sqlite-comparison-2026-09-26/README.md)
for commands, repetitions, resource logs and recovery checks. The command
above exercises the full simulator; the paired comparison is reproduced by
the runner in [Performance](#performance).

## Install for Python

Python 3.9 or newer; Linux (x86_64, aarch64, glibc 2.28+) and macOS (Apple
Silicon). The wheel contains the whole engine: no Rust, no compiler, no
separate library to install.

```bash
pip install elitesql
```

Published releases are available on [PyPI](https://pypi.org/project/elitesql/).
Wheels are also attached to
[GitHub Releases](https://github.com/jalbarracinv/elitesql/releases).
To use changes made after the published release, build from source below.

**From source** (Linux or macOS with the Rust toolchain, 1.89 or newer):

```bash
git clone https://github.com/jalbarracinv/elitesql.git
cd elitesql
pip install build wheel
bash bindings/python/build_wheel.sh          # builds libelitesql and the wheel
pip install bindings/python/dist/elitesql-*.whl
```

For development inside the checkout, `pip install -e ./bindings/python` after
`cargo build --release -p elitesql-ffi` also works: the package finds the
library in `target/release`. A library elsewhere is reached with
`ELITESQL_LIB=/path/to/libelitesql.so` or `EliteSQL(path, lib_path=...)`.

## Quick start (Python)

```python
from elitesql import EliteSQL

with EliteSQL("app.esql") as db:                 # creates the directory if missing
    db.query("""
        CREATE TABLE users (
          id int AUTO_INCREMENT PRIMARY KEY,
          email varchar(255) NOT NULL,
          plan enum('free', 'pro') NOT NULL DEFAULT 'free',
          created_at timestamp NOT NULL DEFAULT CURRENT_TIMESTAMP
        )
    """)
    db.query("CREATE UNIQUE INDEX ON users (email)")

    # Parameters are bound as typed values, never interpolated.
    result = db.query("INSERT INTO users (email) VALUES (%s)", ["ana@example.com"])
    ana_id = result["lastrowid"]                 # 1

    rows = db.query(
        "SELECT id, email, plan FROM users WHERE plan = %(plan)s ORDER BY id LIMIT %(n)s",
        {"plan": "free", "n": 10},
    )
    print(rows["columns"], rows["rows"])         # ['id', 'email', 'plan'] [[1, 'ana@example.com', 'free']]

    updated = db.query("UPDATE users SET plan = 'pro' WHERE id = %s", [ana_id])
    print(updated["affected"])                   # 1
```

`query()` returns `{"columns", "rows"}` for `SELECT`, `{"inserted": [...],
"lastrowid": ...}` for `INSERT`, `{"affected": n}` for `UPDATE`/`DELETE` and
`{"ok": True}` for DDL. Values come back as Python types: `int`, `float`, `str`,
`bytes`, `bool`, `None`, `datetime.date`/`time`/`datetime` (UTC), `dict`/`list`
for `json`, and `list[float]` for vectors.

The following Python examples continue with that database. Reopen it with
`db = EliteSQL("app.esql")` after the context manager exits, and call
`db.close()` when finished. The quick start creates its schema once; use a
fresh directory when repeating it.

### DB-API style cursors

```python
cur = db.cursor()
cur.execute("SELECT id, email FROM users WHERE id = ?", [ana_id])
print(cur.description[0][0])   # 'id'
print(cur.fetchone())          # [1, 'ana@example.com']

cur.execute("INSERT INTO users (email) VALUES (%s)", ["bo@example.com"])
print(cur.lastrowid)           # 2
cur.executemany("INSERT INTO users (email) VALUES (%s)", [["c@x.com"], ["d@x.com"]])
print(cur.rowcount)            # 2
```

### Transactions

Several operations become one atomic commit. Transactions stage writes in
parallel, then validate and publish at commit; a conflict raises `EliteSQLError` with code 9
(`CONFLICT_RETRY`) and the whole unit can be rerun.

```python
from elitesql import EliteSQLError

with db.transaction() as tx:                      # commits on success, rolls back on exception
    new = tx.insert("users", {"email": "eve@example.com"})
    tx.query("UPDATE users SET plan = 'pro' WHERE id = %s", [new["record"]["id"]])
    row = tx.get("users", new["id"])              # opaque physical key; record["id"] is the SQL integer
    tx.update("users", new["id"], {"plan": "free"})

db.query("CREATE TABLE accounts (id int AUTO_INCREMENT PRIMARY KEY, credits int NOT NULL)")
db.query("INSERT INTO accounts (id, credits) VALUES (7, 5)")

def promote(tx):
    tx.query("UPDATE accounts SET credits = credits - 1 WHERE id = 7 AND credits >= 1")
    return tx.query("SELECT credits FROM accounts WHERE id = 7")["rows"][0][0]

credits = db.run_transaction(promote)             # retries only on code 9

try:
    db.query("INSERT INTO users (email) VALUES (%s)", ["ana@example.com"])
except EliteSQLError as error:
    print(error.code, error.retry_safe, error.maybe_published)   # 11 False False
```

Every error carries a stable `code`; `error.retry_safe` tells you when nothing
was published (safe to rerun) and `error.maybe_published` when the write may
already be visible (verify before retrying). The table of codes is in the
[Python README](bindings/python/README.md#error-codes-and-retries).

### Vectors, full text and hybrid search

Embeddings are an ordinary column type with a native HNSW index; BM25 full-text
and reciprocal-rank-fusion hybrid search sit next to it.

```python
# Supply 768-dimensional embeddings from your embedding model.
db.query("CREATE TABLE notes (body text NOT NULL, workspace text, emb vector(768))")
db.create_vector_index("notes", "emb", metric="cosine")   # sync by default; mode="async", quantized=True available
db.create_text_index("notes", "body")                     # BM25

db.query("INSERT INTO notes (body, workspace, emb) VALUES (%s, %s, %s)",
         ["quarterly numbers look good", "acme", embedding])

hits = db.search_vector("notes", "emb", query_embedding, top_k=10, filter={"workspace": "acme"})
for hit in hits:
    print(hit["id"], hit["distance"], hit["record"]["body"])

hits = db.search_text("notes", "body", "quarterly numbers", top_k=10)
# A hit carries the whole row by default, indexed text included. `columns`
# narrows it, and `columns=[]` returns ids and scores alone: for a search that
# only ranks, that is most of what each result costs.
hits = db.search_text("notes", "body", "quarterly numbers", top_k=10, columns=["workspace"])
hits = db.search_hybrid("notes", text=("body", "quarterly numbers"),
                        vector=("emb", query_embedding), top_k=10)
```

Replacing an embedding is a plain `UPDATE`; the index follows the committed
row. Vector, filter and text search also work over the sidecar with the same
method names.

### Snapshots and threads

`EliteSQL` is thread-safe and `ctypes` releases the GIL on every call, so
Python threads read and write in parallel. A snapshot is a stable read
position while others keep committing:

```python
with db.snapshot() as snap:
    before = snap.scan("users")          # every row as of this instant
    one = snap.get("users", new["id"])   # physical key returned by tx.insert(), not the SQL integer
```

### Multiple processes: gunicorn, uwsgi, cron jobs

A database directory is owned by one process. When several processes need it,
run `elitesql serve` and connect each worker with `SidecarClient`; the API is
the same, the engine is the same, only the transport changes. See
[the sidecar section](#multi-worker-and-remote-the-sidecar-mode) and the
runnable demo in `examples/gunicorn_demo/run_demo.sh`.

### SQL

The dialect is deliberately small and documented in [manual.md](manual.md),
with a [MySQL migration guide](mysql2elite.md). It covers `CREATE TABLE` with
`AUTO_INCREMENT`/identity primary keys, `enum`, `varchar(N)`, defaults and
`CURRENT_TIMESTAMP`, one-column foreign keys with `RESTRICT`/`CASCADE`,
unique indexes, `INSERT ... RETURNING`, `INSERT IGNORE`/`ON CONFLICT DO
NOTHING`, `UPDATE`/`DELETE` with arithmetic and automatic conflict retry,
`SELECT` with joins, `GROUP BY`/`HAVING`, aggregates including
`COUNT(DISTINCT col)`, `ORDER BY`/`LIMIT`/`OFFSET`, date ranges, `EXPLAIN`,
and `ALTER TABLE` (add, drop and rename columns, rename tables) that is
crash-safe through an intent journal. Placeholders are `?`, `%s` or
`%(name)s`; `LIMIT`/`OFFSET` may be parameters.

```python
db.query("CREATE TABLE orders (user_id int NOT NULL REFERENCES users(id), amount float64)")
db.query("INSERT INTO orders (user_id, amount) VALUES (%s, %s)", [ana_id, 19.99])
db.query("""
    SELECT u.email, o.amount FROM users u
    JOIN orders o ON o.user_id = u.id
    WHERE u.email = %s ORDER BY o.amount DESC LIMIT 10
""", ["ana@example.com"])

db.query("""
    SELECT plan, count(*) AS n FROM users
    WHERE created_at >= '2026-01-01' GROUP BY plan HAVING count(*) > 1 ORDER BY n DESC
""")
```

## CLI

The `elitesql` command creates, inspects, checks, backs up, repairs and serves
databases. Install it once from the checkout (needs the Rust toolchain):

```bash
cargo install --locked --path crates/elitesql-cli   # copies it to ~/.cargo/bin
elitesql --help
# update later with: git pull --ff-only && cargo install --locked --path crates/elitesql-cli --force
```

```bash

elitesql --create app.esql               # create a new database (once)
elitesql query app.esql "CREATE TABLE docs (title text NOT NULL)"
elitesql query app.esql "INSERT INTO docs (title) VALUES ('hello')"
elitesql query app.esql "SELECT count(*) AS n FROM docs"
elitesql app.esql                       # interactive shell (SQLite-style shorthand)
elitesql repl app.esql                  # interactive shell (.exit to quit)
elitesql tables app.esql                # schemas as JSON
elitesql check app.esql                 # offline integrity check
elitesql compact app.esql
elitesql backup app.esql backup.esql    # snapshot-consistent copy, verified
elitesql restore backup.esql restored.esql   # destination must not exist
elitesql export app.esql docs > docs.jsonl
elitesql query restored.esql "CREATE TABLE imported_docs (title text NOT NULL)"
elitesql import restored.esql imported_docs < docs.jsonl
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

A database directory is owned by **one process**. When several processes need it — or when the app runs on a different host — that process serves them: it owns the engine and answers a line-delimited JSON protocol, one thread per connection over a shared `Db`. Transactions stage writes in parallel; the engine coordinates validation and publication at commit.

### Same host: Unix socket

For multi-process deployments (gunicorn, PHP-FPM), the transport is a Unix socket, authenticated by filesystem permissions:

```bash
elitesql query app.esql "CREATE TABLE visits (who text NOT NULL)"  # initialize once
elitesql serve app.esql /tmp/elitesql.sock
```

```python
# each gunicorn worker:
from elitesql import SidecarClient
db = SidecarClient("/tmp/elitesql.sock")
db.query("INSERT INTO visits (who) VALUES ('ana')")
db.query("SELECT count(*) AS n FROM visits")

# Large unordered results stay bounded on both server and client.
with db.streaming_cursor("SELECT id, who FROM visits", batch_rows=512) as rows:
    for row in rows:
        consume(row)
```

Reproducible demo with real gunicorn (4 workers, concurrent visitors reading and writing): `examples/gunicorn_demo/run_demo.sh`.

### Another host: TCP

```bash
export ELITESQL_TOKEN=$(openssl rand -hex 32)     # or --token-file <path>
elitesql serve app.esql --tcp 127.0.0.1:7070
```

```python
import os
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
- **Latency changes the performance profile.** TCP adds network round trips to every request; embedded benchmark timings do not predict remote latency. Measure on the intended network. If you only need several workers, keep them on one host with the Unix socket.

`--max-connections` (default 128) caps concurrent connections on **both** transports, since each one costs a thread; past the cap the server answers with a refusal instead of queueing.

Requests and responses are each capped at 8 MiB. Compatibility `query` calls
also cap buffered SELECT results at 10,000 rows. For larger unordered SELECTs,
the protocol exposes `query_open`, `query_next` and `query_close`; Python wraps
them with `streaming_cursor()` and Node with `stream()`/`nextBatch()`.

**Do not** put a database directory on NFS or SMB and open it from two machines. Durability relies on `fsync` plus atomic `rename`, and immutable index bases are read through `mmap`; network filesystems do not provide either reliably. The sidecar is the supported way to reach a database from elsewhere.

### Same behavior, different transport

The SQL is identical over a Unix socket, over TCP, and embedded: it is the same engine in the same process. Same dialect, same MVCC and read-committed reads, same automatic retry on UPDATE/DELETE, same atomic multi-row INSERT, same error codes. A query does not behave differently because it arrived over a socket — see [manual.md](manual.md#running-sql-from-another-process-or-another-host).

What the server mode is **not**, so the boundaries are clear:

- **One database per server process.** There is no `USE db` and no database listing: `serve` opens the directory you name and serves that one. Several databases mean several processes on different ports, each with its own memory budget — the memory governor is per-`Db`, so a single process serving many databases would break the bounded-footprint guarantee.
- **No TLS.** The token and the data travel in cleartext; a tunnel or a private network is doing the encrypting.
- **Explicit transactions are connection-bound.** The sidecar protocol exposes
  `begin`, `query_in_txn`, `commit` and `rollback`; the Python
  `SidecarClient.transaction()` wrapper pins the connection until completion.
  A remote transaction is rolled back on disconnect or after 30 seconds.
- **No replication, no failover, no connection multiplexing.** One process owns the directory; if it is down, the database is unreachable.

Use TCP when the app genuinely has to live on another machine — to give it separate resources, for instance. To scale workers, keep them on one host with the Unix socket and skip the network entirely.

## Other bindings

**Python** is covered above and in [bindings/python/README.md](bindings/python/README.md)
(error codes, retries, timeouts, durability notes).

Positional `?`/`%s` and named `%(name)s` placeholders are supported by the
Python binding, the sidecar protocol, the Node client, the C ABI and the Rust
API. Parameter count and names are validated strictly; strings containing
quotes or SQL syntax stay data and cannot alter the parsed statement. Binding
preserves nulls, booleans, signed 64-bit integers, floats, text, blobs,
date/time/timestamp, JSON and vectors. `LIMIT` and `OFFSET` may be parameters
but must receive a non-negative `int64`.

**Node** ([bindings/node/elitesql.js](bindings/node/elitesql.js)) — dependency-free sidecar client:

The client declares Node.js 18 or newer; CI uses Node.js 24.

```js
const { SidecarClient } = require('./elitesql');
const db = await SidecarClient.connect('/tmp/elitesql.sock');
const { rows } = await db.query('SELECT * FROM notes WHERE body = %s LIMIT %s', ['hello', 10]);
const hits = await db.searchVector('notes', 'emb', embedding, { topK: 10 });
await db.close();   // waits for requests already sent
```

Int64 values beyond 2^53 arrive as `BigInt`, timestamps keep their microseconds
(`timestampMicros(date)`), and JSON columns keep large integers exact.

**C** — header at [crates/elitesql-ffi/include/elitesql.h](crates/elitesql-ffi/include/elitesql.h); `cargo build --release -p elitesql-ffi` produces `libelitesql`, the same library the Python binding loads.

## Durability

| Mode | fsync | On process crash | On OS crash / power loss |
|---|---|---|---|
| `Safe` (default) | Every commit group, before acknowledging | Loses nothing | Loses nothing (see the macOS note) |
| `Balanced` | Within `balanced_sync_interval_ms` (25 ms) of a commit, by a timer thread; commits never wait for it | Loses nothing | May lose commits acknowledged in the last interval |
| `Fast` | Checkpoints and clean close only | Loses nothing | May lose every commit since the last checkpoint or close |

In every mode a clean `close()`/drop syncs the WAL, so nothing acknowledged
is left in the page cache; only a crash before close can lose the tail. A
torn or zero-filled WAL tail left by a power loss is truncated to the last
complete commit; damage *followed by* a complete commit is refused as
corruption. Atomicity holds in all modes: never half a commit.

Concurrent `Safe` commits share a physical WAL sync when they overlap; every
caller still waits for that group's sync result before returning. The default
`safe_group_commit_delay_us` is 200; setting it to zero removes the intentional
coalescing window without changing durability. `Balanced`
commits are acknowledged as soon as they are applied: the timer thread runs the
barrier on a duplicated file handle with the commit mutex released, so an
fsync never stalls the commit pipeline. If a sync fails, a `Safe` commit
returns `CommitUnknown` (the write is visible, its durability is not) and in
every mode further writes are fenced until the database is reopened and its
WAL chain re-validated.

**macOS:** `fsync` does not flush the drive's write cache, so `Safe` survives
a process or kernel crash but not a power loss unless you set
`DbOptions { full_fsync: true, .. }` (`F_FULLFSYNC`, opt-in like SQLite's
`fullfsync`; roughly an order of magnitude slower per barrier). Linux needs
nothing extra. The current Safe writer benchmark enables strict drive-cache
flushes on both engines; Fast and Balanced measurements do not make the same
per-commit power-loss guarantee. Python exposes `EliteSQL(path,
full_fsync=True)` and the CLI accepts `--full-fsync`.

```rust
use elitesql_core::{Db, DbOptions, Durability};
let opts = DbOptions { durability: Durability::Balanced, ..Default::default() };
let db = Db::open_or_create_with("app.esql", opts)?;
```

## Database-wide memory budget

Every open database owns one shared governor. The envelope is partitioned into
a concurrent-query pool, a 128 MiB mutable-index pool, a 128 MiB maintenance
pool and an 8 MiB emergency reserve, with 64 MiB left as allocator/runtime
headroom. Clean file-backed `mmap` pages and result values already handed to
the caller are not charged.

The query pool bounds how many statements run at once, so it follows the cores
that can run them: 24 MiB per core, between 64 and 512 MiB. On a ten-core
machine that is a 240 MiB pool inside a 568 MiB envelope. The pool is a ceiling
the governor accounts against, not an allocation.
`elitesql serve --memory-mib <n>` scales the whole
profile from one number.
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
        total_memory_bytes: 568 * 1024 * 1024,
        query_pool_bytes: 240 * 1024 * 1024,
        query_working_bytes: 16 * 1024 * 1024,
        index_delta_pool_bytes: 128 * 1024 * 1024,
        maintenance_pool_bytes: 128 * 1024 * 1024,
        reserved_memory_bytes: 8 * 1024 * 1024,
        scan_batch_rows: 512,
        query_admission_timeout_ms: 5000,
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
not accelerate inserts. The default index and maintenance pools were measured
to retain a complete 100K x 64-dimensional HNSW graph and avoid restart
catch-up on an AWS `t3.large`; smaller deployments can explicitly select a
tighter envelope with `--memory-mib`.

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
read-only with `mmap`, plus a bounded mutable delta. For automatic checkpoints,
an automatic checkpoint moves that delta to one immutable frozen generation in
O(1), lets commits continue in a fresh active delta, and flushes the frozen
generation on a dedicated worker. Reads merge active + frozen + mmap runs until
publication. The frozen heap and writer own the maintenance-pool reservation,
so there is never an unbudgeted second memtable. Before releasing the commit
mutex, EliteSQL installs durable empty bridge + active WAL successors. It then
copies the record-aligned old-WAL tail and atomically publishes the manifest
without blocking later commits; recovery follows either manifest generation
through the consecutive WAL chain. Explicit checkpoints, DDL, compaction and
close wait for the worker. Equality, BM25 and vector overlays have a matching freeze pipeline:
the current overlay becomes an immutable queryable generation in O(number of
indexes), commits continue into a fresh active overlay, and a dedicated worker
serializes runs/graphs outside the commit mutex. Manifest preparation and I/O
are ordered by dedicated publication mutexes; the global commit mutex is used
only for short validation, WAL-writer swap and in-memory adoption. A background worker promotes groups of sixteen same-level primary
runs, while equality/BM25 retain fanout eight. Disjoint V3 primary ranges copy
their already checksummed pages directly instead of decoding and rebuilding
every entry. The atomic `primary.runs` manifest selects one exact generation.
Paged format V3 checksums navigation metadata. Mapped runs retain compact
page-navigation metadata and lazily build table/page fences for primary-key
lookups; complete keys remain file-backed. Checkpoint snapshots intern table names and pack IDs contiguously,
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
primary, equality and BM25. It also exposes physical WAL syncs, commits served
by shared sync groups, and completed/background derived-publication time. The
`wait_for_*_compaction()` barriers provide
explicit graceful-shutdown/testing synchronization.

## On-disk format

```text
app.esql/
  ELITESQL        # marker + format_version
  LOCK            # process exclusion (flock)
  catalog.json    # compatibility/tooling catalog mirror
  manifest        # atomic data + schema generation
  manifest.prev   # redundant recovery fallback
  wal/            # durable commits (per-record CRC)
  segments/       # immutable data (per-entry CRC)
  indexes/        # paged primary/equality/BM25 files
    primary.runs  # atomic generation + active primary run set
    primary.pidx  # stable primary base
    primary-L*    # immutable primary deltas and promoted runs
    *.sidx.runs   # equality run manifests; *.sidx.run are immutable levels
    *.tidx.runs   # BM25 run manifests; *.tidx.run are immutable levels
  vectors/        # persisted ANN graphs (CRC; disposable and rebuildable)
    *.vidx        # base graph written when an index is created over rows
    *-<ulid>.vidx.run   # immutable HNSW runs published or merged while running
    *.vidx.runs   # vector run manifests (which runs cover which generation)
  blobs/          # out-of-line blob chunks (CRC)
```

The engine's golden rule: `Data files are canonical. Indexes are disposable.` If an index breaks, it is rebuilt from data; if the manifest breaks, the previous one is used; if the WAL has an incomplete entry, the whole entry is discarded (never half a commit).

## Integrity checking

```rust
let report = elitesql_core::check("app.esql")?;
assert!(report.is_ok());
```

`check` is offline: close the writer first. It verifies canonical segments and
the complete WAL chain, independently reconstructs live rows using temporary
sorted files, and validates types, uniqueness, foreign keys and identity
watermarks. It compares that image with primary/secondary reads. Canonical
violations are errors; damaged or inconsistent disposable indexes are warnings
and can be rebuilt. Temporary files require disk space proportional to the
database. Legacy manifests cannot prove that a previously missing final WAL
successor existed; see [disk format](docs/disk-format.md).

To run the crash-injection suite and fuzzing with more iterations:

```bash
ELITESQL_CRASH_ITERS=500 cargo test --release --test crash_kill
ELITESQL_FUZZ_ITERS=5000 cargo test --release --test corruption
```

## Performance

[benchmark.md](benchmark.md) contains the current EliteSQL-versus-SQLite
comparison, measured on 2026-09-26 on an Apple M5 with three repetitions per
engine and workload. It covers transactional load at 1M/10M rows, identical
parameterized SQL point reads, concurrent writers and the mini-SaaS simulator.
Ratios above 1 favor EliteSQL; ranges and exact timing boundaries are in the
report.

- **Transactional load, Fast/OFF:** SQLite wins at 1M rows. At 10M rows,
  EliteSQL is 1.54× faster with 1K-row transactions; 10K-row transactions are
  effectively tied (0.99×, overlapping ranges).
- **Warmed SQL point reads:** EliteSQL is 1.38× faster at 1M rows and
  6.86–9.17× faster at 10M rows in this narrow-row fixture.
- **Concurrent writes:** EliteSQL delivers 1.39–2.67× SQLite's throughput
  in Fast/Balanced at one/four/eight writers. Safe with strict macOS flushes
  is near parity at one writer and reaches 3.83×/7.46× at four/eight writers;
  throughput gains do not imply lower commit p99 in every case.
- **Mini-SaaS:** SQLite has lower isolated operation cost. Concurrent
  throughput favors SQLite at 10 users and EliteSQL at 100; the 500-user
  ranges overlap. See [the simulator results](#current-status) and the
  benchmark for transport and operation-mix differences.

Reproduce the paired suites, including SaaS, from the checkout:

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

The runner records commands, source/binary hashes, exit statuses, resource
logs and raw results, then regenerates the base `benchmark.md` report.
Build comparisons, internal diagnostics and older measurements remain in
[old_benchmark.md](old_benchmark.md). The
[September 11 integrity/performance review](benchmark-results/review-2026-09-11/README.md)
is historical evidence, not a measurement of the current source tree.

## Using from Rust

The engine is a Rust crate, `elitesql-core`; everything the Python binding does
goes through this API. Requirements: [Rust](https://rustup.rs) 1.89 or newer,
with `rustup` as one installation option:

```bash
curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs \
  | sh -s -- -y --default-toolchain 1.89.0 --profile minimal
source "$HOME/.cargo/env"
```

```toml
[dependencies]
elitesql-core = { path = "../elitesql/crates/elitesql-core" }
# or straight from git:
# elitesql-core = { git = "https://github.com/jalbarracinv/elitesql.git" }
```

For development, run the test suite and benchmarks from the repository:

```bash
cargo test --locked
cargo bench --locked
bash scripts/acceptance.sh     # fmt, clippy, workspace tests, FFI, Python, Node
```

### Quick start (Rust)

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
    rec.insert("title", Value::Text("hello".into()));
    rec.insert("score", Value::Int64(10));
    let id = db.insert("docs", rec)?;

    // Multi-operation transaction: atomic, isolated, optimistically
    // validated at commit (Error::Conflict => retry).
    let mut txn = db.begin();
    let mut patch = Record::new();
    patch.insert("score", Value::Int64(99));
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

### Relational and MySQL compatibility (SQL)

Tables without a declared `id` expose an implicit text key, normally a
generated ULID. A declared integer identity `id` is instead the primary lookup
key, stored internally as an order-preserving text encoding. SQL returns the
integer; low-level CRUD uses the opaque physical key returned by insert.
This permits direct MySQL-style primary keys:

```sql
CREATE TABLE users (
  id int AUTO_INCREMENT PRIMARY KEY,
  email varchar(255) NOT NULL,
  plan enum('free', 'pro') NOT NULL DEFAULT 'free',
  created_at timestamp NOT NULL DEFAULT CURRENT_TIMESTAMP
);
CREATE UNIQUE INDEX ON users (email);

CREATE TABLE products (
  id int AUTO_INCREMENT PRIMARY KEY,
  name text NOT NULL,
  category text NOT NULL,
  price_cents int NOT NULL
);
CREATE INDEX ON products (category, price_cents);
SELECT id, name, price_cents
FROM products
WHERE category = 'books'
ORDER BY price_cents ASC
LIMIT 20 OFFSET 40;

CREATE TABLE documents (
  id int GENERATED BY DEFAULT AS IDENTITY PRIMARY KEY,
  owner_id int NOT NULL REFERENCES users(id) ON DELETE CASCADE,
  body longtext
);

INSERT INTO users (email) VALUES ('ana@example.com') RETURNING id;
CREATE TABLE accounts (id int AUTO_INCREMENT PRIMARY KEY, credits int NOT NULL);
INSERT INTO accounts (id, credits) VALUES (7, 5);
UPDATE accounts SET credits = credits - 1
WHERE id = 7 AND credits >= 1;
```

The identity high-water mark is durable and advances past explicitly imported
values. One-column foreign keys support `RESTRICT` and `CASCADE` and are
validated at optimistic commit. `INSERT IGNORE` and `ON CONFLICT DO NOTHING`
suppress uniqueness conflicts; global `COUNT(DISTINCT col)`, `NOW()` and
`CURRENT_TIMESTAMP` are also supported. Python exposes DB-API-style
`fetchone`, `fetchall` and `lastrowid`, while Rust, embedded Python and the
sidecar can run multi-statement SQL through an explicit transaction. See the
[SQL manual](manual.md) and [MySQL migration guide](mysql2elite.md) for limits
and retry rules.

Eligible arithmetic updates such as `credits = credits - 1` can reapply their
delta to a newer row at commit instead of failing immediately on contention.
The engine rechecks the `WHERE` guard and constraints; this optimization is
disabled when the transaction has returned that row to the caller or mixed
delta updates with ordinary writes. Applications must still handle conflicts.

An index can contain one or more ordered columns. `UNIQUE (a, b)` applies to
the complete tuple; tuples containing `NULL` remain indexable but do not
conflict with one another. The ordered-read path covers a non-NULL
equality prefix plus ascending `ORDER BY` over the remaining indexed columns
with `LIMIT`; text ordering requires `COLLATE binary`. Unicode collation,
descending order, historical snapshots and staged transaction reads retain the
normal sort path. `EXPLAIN` prints `INDEX ORDERED` only when that path is used.
Eligible indexed reads defer decoding projected records until filtering and
`LIMIT`/`OFFSET` select the rows that will be returned.

### Vector search (ANN) in Rust

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
filter.insert("workspace", Value::Text("acme".into()));
let hits = db.search_vector(
    "notes", "embedding", &query_embedding, 20,
    &VectorSearchOptions { filter: Some(filter), ..Default::default() },
)?;
for hit in hits {
    println!("{} (dist {:.3})", hit.id, hit.distance);
}
```

Vectors are ordinary typed columns, so replacing an embedding is a parameterized
SQL update; the vector index follows the committed record automatically:

```rust
db.query_params(
    "UPDATE notes SET embedding = %s WHERE workspace = %s",
    &[Value::Vector(query_embedding.clone()), Value::Text("acme".into())],
)?;
```

The replacement vector must have the column's declared dimension (768 above).

An `Async` mode lets commits proceed without waiting for indexing; search may
lag behind recent writes. The optional `quantized` (int8) representation uses
roughly 4× less space for index vector payloads; canonical row vectors remain
exact. Historical ANN latency, memory and recall measurements are in
[old_benchmark.md](old_benchmark.md). They are separate from the current
SQLite comparison, which has no equivalent native SQLite ANN workload.

### Full-text and hybrid in Rust

```rust
use elitesql_core::HybridQuery;

db.create_text_index("notes", "body")?;                    // BM25
let hits = db.search_text("notes", "body", "query", 10, None)?;
let hits = db.search_hybrid("notes", &HybridQuery {        // RRF: text + vector
    text: Some(("body", "query")),
    vector: Some(("embedding", &query_embedding)),
    top_k: 10,
    ..Default::default()
})?;
```

### SQL from Rust

The same engine exposes a deliberately small SQL dialect — full reference with examples in [manual.md](manual.md), and a migration guide for people arriving from MySQL in [mysql2elite.md](mysql2elite.md):

```rust
use elitesql_core::{QueryOutput, Record, Value};

db.query("CREATE TABLE users (id int AUTO_INCREMENT PRIMARY KEY, name text NOT NULL, email text, age int, since date)")?;
db.query("CREATE UNIQUE INDEX ON users (email)")?;
db.query("INSERT INTO users (name, email, age, since) VALUES ('ana', 'ana@x.com', 30, '2026-08-07')")?;
db.query("CREATE TABLE orders (user_id int NOT NULL REFERENCES users(id), amount float64)")?;
db.query("INSERT INTO orders (user_id, amount) VALUES (1, 19.99)")?;

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
params.insert("email", Value::Text("ana@x.com".into()));
params.insert("limit", Value::Int64(10));
db.query_named_params(
    "SELECT name FROM users WHERE email = %(email)s LIMIT %(limit)s",
    &params,
)?;
```


## License

Licensed under either of

- Apache License, Version 2.0 ([LICENSE-APACHE](LICENSE-APACHE) or http://www.apache.org/licenses/LICENSE-2.0)
- MIT license ([LICENSE-MIT](LICENSE-MIT) or http://opensource.org/licenses/MIT)

at your option.

Unless you explicitly state otherwise, any contribution intentionally submitted
for inclusion in the work by you, as defined in the Apache-2.0 license, shall be
dual licensed as above, without any additional terms or conditions.
