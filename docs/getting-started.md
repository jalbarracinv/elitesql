# Getting started with EliteSQL

EliteSQL is an embedded operational database: no server, one self-contained
directory per database. This document takes you from zero to queries, vectors
and multi-worker deployment. Full SQL reference: [manual.md](../manual.md).

## Build

```bash
git clone <repo-url> elitesql && cd elitesql
cargo build --release                   # engine + CLI (target/release/elitesql)
cargo build --release -p elitesql-ffi   # libelitesql for bindings (Python/C)
cargo test                              # full suite
```

## First contact: the CLI

```bash
ESQL=target/release/elitesql
$ESQL --create query app.esql "CREATE TABLE notes (body text NOT NULL, score int, day date)"
$ESQL query app.esql "INSERT INTO notes (body, score, day) VALUES ('hello', 10, '2026-08-07')"
$ESQL query app.esql "SELECT body, score FROM notes WHERE day >= '2026-01-01' ORDER BY score DESC"
$ESQL app.esql                 # interactive shell; .help for shell commands
```

`--create` is needed once, to bring the database into existence; afterwards
opening it needs no flag. Opening never creates a database on its own, because
a single argument that is not a subcommand is read as a path — so a mistyped
subcommand (`elitesql versio`) would otherwise leave a database directory named
after the typo in the working directory.

Inside the shell, terminate SQL statements with `;`. Input may span multiple
lines; semicolons inside strings or comments do not end the statement.

## Rust (embedded)

```rust
use elitesql_core::{Db, QueryOutput};

let db = Db::open_or_create("app.esql")?;
db.query("CREATE TABLE notes (body text NOT NULL, emb vector(768))")?;
db.query("INSERT INTO notes (body) VALUES ('hello')")?;
if let QueryOutput::Rows { rows, .. } = db.query("SELECT body FROM notes")? { /* ... */ }
```

Transactions (MVCC, optimistic validation):

```rust
let mut txn = db.begin();
txn.insert("notes", record_a)?;
txn.insert("notes", record_b)?;   // both or neither
match txn.commit() {
    Ok(_) => {}
    Err(elitesql_core::Error::Conflict(_)) => { /* retry */ }
    Err(e) => return Err(e.into()),
}
```

Snapshots (stable reads while others write):

```rust
let snap = db.snapshot();
let rows = db.scan_at(&snap, "notes")?;
```

## Search: vector, text and hybrid

```rust
use elitesql_core::{HybridQuery, VectorIndexOptions, VectorSearchOptions};

db.create_vector_index("notes", "emb", VectorIndexOptions { quantized: true, ..Default::default() })?;
db.create_text_index("notes", "body")?;

// ANN
let hits = db.search_vector("notes", "emb", &embedding, 10, &VectorSearchOptions::default())?;
// BM25
let hits = db.search_text("notes", "body", "text query", 10, None)?;
// Hybrid (RRF)
let hits = db.search_hybrid("notes", &HybridQuery {
    text: Some(("body", "query")),
    vector: Some(("emb", &embedding)),
    top_k: 10,
    ..Default::default()
})?;
```

## Python

```python
import sys; sys.path.insert(0, "bindings/python")
from elitesql import EliteSQL

with EliteSQL("app.esql") as db:          # ctypes releases the GIL: real threads
    db.query("CREATE TABLE notes (body text NOT NULL, emb vector(768))")
    db.query("INSERT INTO notes (body, emb) VALUES (%s, %s)", ["hello", emb])
    rows = db.query("SELECT * FROM notes WHERE body = %(body)s LIMIT %(n)s",
                    {"body": "hello", "n": 10})
    db.create_text_index("notes", "body")
    db.create_vector_index("notes", "emb")
    hits = db.search_hybrid("notes", text=("body", "hello"), vector=("emb", emb))
    with db.snapshot() as snap:
        rows = snap.scan("notes")
```

## Multi-worker (gunicorn, PHP-FPM): the sidecar mode

One process owns the engine; the workers talk over a Unix socket:

```bash
target/release/elitesql serve app.esql /tmp/elitesql.sock
```

```python
from elitesql import SidecarClient
db = SidecarClient("/tmp/elitesql.sock")    # one per worker
db.query("SELECT count(*) AS n FROM notes")

with db.streaming_cursor("SELECT id, body FROM notes", batch_rows=512) as rows:
    for row in rows:
        consume(row)
```

```js
const { SidecarClient } = require('./bindings/node/elitesql');
const db = await SidecarClient.connect('/tmp/elitesql.sock');
await db.query('SELECT * FROM notes WHERE body = %s LIMIT %s', ['hello', 10]);
const rows = await db.stream('SELECT id, body FROM notes');
for await (const row of rows) consume(row);
```

Reproducible demo with gunicorn and 4 workers: `examples/gunicorn_demo/run_demo.sh`.

### From another host: TCP

Same protocol, different transport. A Unix socket is authenticated by
filesystem permissions; a TCP port is not, so it requires a shared token:

```bash
export ELITESQL_TOKEN=$(openssl rand -hex 32)      # or --token-file <path>
target/release/elitesql serve app.esql --tcp 127.0.0.1:7070
```

```python
db = SidecarClient(host="db-host", port=7070, token=os.environ["ELITESQL_TOKEN"])
```

```js
const db = await SidecarClient.connect({ host: 'db-host', port: 7070, token });
```

The clients send the handshake when a connection is created. They do not
automatically reconnect; after a disconnect, create a new client and retry only
operations whose outcome is known to be safe. The server refuses every request
until authentication succeeds, so nothing reaches the engine unauthenticated.

Traffic is **not encrypted**. Bind loopback and cross machines through a tunnel:

```bash
ssh -N -L 7070:127.0.0.1:7070 user@db-host
```

Expect the network to dominate: a point lookup is ~4 µs, a round trip is
~0.5 ms locally and 10–50 ms across regions. Separate app and database only when
you must, not to scale workers — several workers on one host is what the Unix
socket already solves.

Never put the database directory on NFS/SMB to share it: durability relies on
`fsync` plus atomic `rename` and index bases are read through `mmap`, and network
filesystems provide neither reliably.

## Durability

| Mode | fsync | Loses on OS crash |
|---|---|---|
| `safe` (default) | per concurrent commit group | nothing |
| `balanced` | every ~25ms, grouped | last few ms |
| `fast` | at checkpoints | recent commits |

Overlapping `safe` and due `balanced` commits can share one physical WAL sync;
each commit waits for that shared sync to complete before it returns.

`elitesql query app.esql --durability balanced "..."` or `DbOptions.durability`.

## When something goes wrong

See [recovery.md](recovery.md). The summary: the database always opens to the
last complete commit; `elitesql backup`/`elitesql restore` make and materialize
verified snapshot-consistent copies; `elitesql check` validates; `--read-only`
inspects even a damaged database; `elitesql repair` salvages into a fresh one,
reporting everything.
