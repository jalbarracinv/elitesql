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
$ESQL query app.esql "CREATE TABLE notes (body text NOT NULL, score int64, day date)"
$ESQL query app.esql "INSERT INTO notes (body, score, day) VALUES ('hello', 10, '2026-08-07')"
$ESQL query app.esql "SELECT body, score FROM notes WHERE day >= '2026-01-01' ORDER BY score DESC"
$ESQL repl app.esql            # interactive shell; .exit to quit
```

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
```

```js
const { SidecarClient } = require('./bindings/node/elitesql');
const db = await SidecarClient.connect('/tmp/elitesql.sock');
await db.query('SELECT * FROM notes LIMIT 10');
```

Reproducible demo with gunicorn and 4 workers: `examples/gunicorn_demo/run_demo.sh`.

## Durability

| Mode | fsync | Loses on OS crash |
|---|---|---|
| `safe` (default) | per commit | nothing |
| `balanced` | every ~25ms | last few ms |
| `fast` | at checkpoints | recent commits |

`elitesql query app.esql --durability balanced "..."` or `DbOptions.durability`.

## When something goes wrong

See [recovery.md](recovery.md). The summary: the database always opens to the
last complete commit; `elitesql check` validates; `--read-only` inspects even
a damaged database; `elitesql repair` salvages into a fresh one, reporting
everything.
