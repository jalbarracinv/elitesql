# Getting started con ClawDB

ClawDB es una base de datos operacional embebida: sin servidor, una carpeta
autocontenida por base. Este documento te lleva de cero a consultas, vectores
y despliegue multi-worker. Referencia SQL completa: [manual.md](../manual.md).

## Construir

```bash
git clone <repo> clawdb && cd clawdb
cargo build --release            # motor + CLI (target/release/clawdb)
cargo build --release -p clawdb-ffi   # libclawdb para bindings (Python/C)
cargo test                       # suite completa
```

## Primer contacto: el CLI

```bash
CLAW=target/release/clawdb
$CLAW query app.clawdb "CREATE TABLE notes (body text NOT NULL, score int64, day date)"
$CLAW query app.clawdb "INSERT INTO notes (body, score, day) VALUES ('hola', 10, '2026-08-07')"
$CLAW query app.clawdb "SELECT body, score FROM notes WHERE day >= '2026-01-01' ORDER BY score DESC"
$CLAW repl app.clawdb            # shell interactivo; .exit para salir
```

## Rust (embebido)

```rust
use clawdb_core::{Db, QueryOutput};

let db = Db::open_or_create("app.clawdb")?;
db.query("CREATE TABLE notes (body text NOT NULL, emb vector(768))")?;
db.query("INSERT INTO notes (body) VALUES ('hola')")?;
if let QueryOutput::Rows { rows, .. } = db.query("SELECT body FROM notes")? { /* ... */ }
```

Transacciones (MVCC, validacion optimista):

```rust
let mut txn = db.begin();
txn.insert("notes", record_a)?;
txn.insert("notes", record_b)?;   // ambas o ninguna
match txn.commit() {
    Ok(_) => {}
    Err(clawdb_core::Error::Conflict(_)) => { /* reintentar */ }
    Err(e) => return Err(e.into()),
}
```

Snapshots (lecturas estables mientras otros escriben):

```rust
let snap = db.snapshot();
let rows = db.scan_at(&snap, "notes")?;
```

## Busqueda: vectorial, texto e hibrida

```rust
use clawdb_core::{HybridQuery, VectorIndexOptions, VectorSearchOptions};

db.create_vector_index("notes", "emb", VectorIndexOptions { quantized: true, ..Default::default() })?;
db.create_text_index("notes", "body")?;

// ANN
let hits = db.search_vector("notes", "emb", &embedding, 10, &VectorSearchOptions::default())?;
// BM25
let hits = db.search_text("notes", "body", "consulta de texto", 10, None)?;
// Hibrida (RRF)
let hits = db.search_hybrid("notes", &HybridQuery {
    text: Some(("body", "consulta")),
    vector: Some(("emb", &embedding)),
    top_k: 10,
    ..Default::default()
})?;
```

## Python

```python
import sys; sys.path.insert(0, "bindings/python")
from clawdb import ClawDB

with ClawDB("app.clawdb") as db:          # ctypes libera el GIL: threads reales
    db.query("CREATE TABLE notes (body text NOT NULL, emb vector(768))")
    db.create_text_index("notes", "body")
    db.create_vector_index("notes", "emb")
    hits = db.search_hybrid("notes", text=("body", "hola"), vector=("emb", emb))
    with db.snapshot() as snap:
        rows = snap.scan("notes")
```

## Multi-worker (gunicorn, PHP-FPM): modo sidecar

Un proceso es dueno del motor; los workers hablan por Unix socket:

```bash
target/release/clawdb serve app.clawdb /tmp/clawdb.sock
```

```python
from clawdb import SidecarClient
db = SidecarClient("/tmp/clawdb.sock")    # uno por worker
db.query("SELECT count(*) AS n FROM notes")
```

```js
const { SidecarClient } = require('./bindings/node/clawdb');
const db = await SidecarClient.connect('/tmp/clawdb.sock');
await db.query('SELECT * FROM notes LIMIT 10');
```

Demo reproducible con gunicorn y 4 workers: `examples/gunicorn_demo/run_demo.sh`.

## Durabilidad

| Modo | fsync | Pierde ante crash del SO |
|---|---|---|
| `safe` (default) | por commit | nada |
| `balanced` | cada ~25ms | ultimos ms |
| `fast` | en checkpoints | commits recientes |

`clawdb query app.clawdb --durability balanced "..."` o `DbOptions.durability`.

## Cuando algo sale mal

Ver [recovery.md](recovery.md). El resumen: la base siempre abre al ultimo
commit completo; `clawdb check` valida; `--read-only` inspecciona incluso una
base danada; `clawdb repair` rescata a una base nueva reportando todo.
