# ClawDB

> **A tiny operational database for AI-native apps.**
> SQLite-fast reads, better concurrent writes, native ANN.

ClawDB es un motor de base de datos **embebido** (sin servidor, sin daemon, sin tuning ceremonial) escrito en Rust. Una base de datos es una carpeta autocontenida que puedes copiar, respaldar y mover.

No compite contra PostgreSQL: compite contra la complejidad de operar esta torre en una app moderna:

```text
SQLite + vector DB + cache + sync layer + files + embeddings metadata
```

La promesa es abrir un solo archivo y tener adentro registros, JSON, blobs, indices, busqueda vectorial ANN, snapshots y concurrencia sana:

```text
db.open("app.clawdb")
```

## Por que ClawDB

- **Escritura concurrente real.** SQLite serializa todos los escritores con un lock global. ClawDB usa MVCC con commits optimistas: los escritores preparan transacciones en paralelo y solo se encuentran en el commit (`Readers never block writers. Writers only meet at commit.`).
- **Vectores nativos** *(en desarrollo, Phase 3)*: `vector<float32, N>` e indice HNSW como tipo de primera clase, no como extension pegada.
- **Fail-safe por diseno.** WAL con checksums, manifest atomico con fallback (`manifest.prev`), replay idempotente y recovery automatico: tras un crash, la base abre al ultimo estado commiteado completo. Un commit es visible completo o no es visible.
- **Superficie pequena.** CRUD, filtros, indices, transacciones, snapshots. No es una recreacion de PostgreSQL.

## Estado actual

| Fase | Contenido | Estado |
|---|---|---|
| Phase 0 | Prototipo append-only + benchmarks vs SQLite | Completa |
| Phase 1 | WAL, manifest, MVCC, transacciones, indices, crash recovery | Completa |
| Phase 2 | Dialecto SQL minimo (ver [manual.md](manual.md)) | Completa |
| Phase 2.5 | Agregados (COUNT/SUM/AVG/MIN/MAX, GROUP BY, HAVING) y tipos date/time | Completa |
| Phase 3 | Tipo vectorial + busqueda ANN (HNSW propio, grafo persistido) | Completa |
| Phase 4 | C ABI (con snapshots), bindings Python/Node, CLI, repair, read-only, sidecar, docs | Completa |
| Phase 5 | Full-text BM25, hybrid search (RRF), vectores int8, blob chunking | Completa (WASM/sync diferidos con decision) |

Verificacion actual: 114 tests Rust (MVCC, recovery, compaction, salvage, modelo aleatorio, suite SQL, recall vectorial, BM25/hybrid, blobs, read-only), crash injection con `kill -9` de procesos reales, fuzzing de corrupcion y del parser SQL, mas e2e de CLI, FFI Python y sidecar Node. Docs de onboarding en [docs/](docs/). Detalles en [specs.md](specs.md) y [plan.md](plan.md).

## Quick installation

Requisitos: [Rust](https://rustup.rs) 1.89 o superior.

```bash
git clone https://192.168.1.67:3100/jalbarracin/sqlcola.git clawdb
cd clawdb
cargo build --release
cargo test          # suite completa
cargo bench         # benchmarks vs SQLite
```

Para usarlo como dependencia en otro proyecto Rust:

```toml
[dependencies]
clawdb-core = { path = "../clawdb/crates/clawdb-core" }
# o directamente desde git:
# clawdb-core = { git = "https://192.168.1.67:3100/jalbarracin/sqlcola.git" }
```

## Quick start

```rust
use clawdb_core::{Column, ColumnType, Db, Record, TableSchema, Value};

fn main() -> clawdb_core::Result<()> {
    let db = Db::open_or_create("app.clawdb")?;

    db.create_table(TableSchema::new(
        "docs",
        vec![
            Column::new("title", ColumnType::Text).not_null(),
            Column::new("score", ColumnType::Int64),
            Column::new("meta", ColumnType::Json),
        ],
    ))?;
    db.create_index("docs", "title", false)?;

    // Escritura simple (auto-commit). El id es un ULID generado por el motor.
    let mut rec = Record::new();
    rec.insert("title".into(), Value::Text("hola".into()));
    rec.insert("score".into(), Value::Int64(10));
    let id = db.insert("docs", rec)?;

    // Transaccion multi-operacion: atomica, aislada, con validacion
    // optimista en el commit (Error::Conflict => reintentar).
    let mut txn = db.begin();
    let mut patch = Record::new();
    patch.insert("score".into(), Value::Int64(99));
    txn.update("docs", &id, patch)?;
    txn.commit()?;

    // Snapshots: lecturas estables mientras otros escriben.
    let snap = db.snapshot();
    let actual = db.get("docs", &id)?.unwrap();
    let en_snapshot = db.get_at(&snap, "docs", &id)?.unwrap();
    assert_eq!(actual["score"], en_snapshot["score"]);

    // Busqueda por igualdad (usa el indice secundario si existe).
    let hits = db.find_eq("docs", "title", &Value::Text("hola".into()))?;
    assert_eq!(hits.len(), 1);
    Ok(())
}
```

### Busqueda vectorial (ANN)

Embeddings como tipo de primera clase, con HNSW propio y filtros por metadata:

```rust
use clawdb_core::{Column, ColumnType, TableSchema, VectorIndexOptions, VectorSearchOptions};

db.create_table(TableSchema::new(
    "notes",
    vec![
        Column::new("body", ColumnType::Text).not_null(),
        Column::new("workspace", ColumnType::Text),
        Column::vector("embedding", 768),
    ],
))?;
db.create_vector_index("notes", "embedding", VectorIndexOptions::default())?; // cosine, sync

// ... insertar registros con Value::Vector(...) ...

let mut filter = clawdb_core::Record::new();
filter.insert("workspace".into(), Value::Text("acme".into()));
let hits = db.search_vector(
    "notes", "embedding", &query_embedding, 20,
    &VectorSearchOptions { filter: Some(filter), ..Default::default() },
)?;
for hit in hits {
    println!("{} (dist {:.3})", hit.id, hit.distance);
}
```

Sobre 100K vectores (dim 64): recall@10 de 0.88 con `ef_search=128` (0.97 con 256) y busquedas de ~95-156 µs. Modo `Async` disponible para que el commit no espere la indexacion, y opcion `quantized` (int8) para ~4x menos memoria.

### Full-text e hibrida

```rust
db.create_text_index("notes", "body")?;                    // BM25
let hits = db.search_text("notes", "body", "consulta", 10, None)?;
let hits = db.search_hybrid("notes", &HybridQuery {        // RRF: texto + vector
    text: Some(("body", "consulta")),
    vector: Some(("emb", &embedding)),
    top_k: 10,
    ..Default::default()
})?;
```

### SQL

El mismo motor expone un dialecto SQL deliberadamente pequeno — referencia completa con ejemplos en [manual.md](manual.md):

```rust
use clawdb_core::QueryOutput;

db.query("CREATE TABLE users (name text NOT NULL, email text, age int64, since date)")?;
db.query("CREATE UNIQUE INDEX ON users (email)")?;
db.query("INSERT INTO users (name, email, age, since) VALUES ('ana', 'ana@x.com', 30, '2026-08-07')")?;

if let QueryOutput::Rows { columns, rows } = db.query(
    "SELECT u.name, o.amount FROM users u \
     JOIN orders o ON o.user_id = u.id \
     WHERE u.email = 'ana@x.com' ORDER BY o.amount DESC LIMIT 10",
)? {
    // ...
}

// Agregados con GROUP BY/HAVING y filtros por rango de fechas:
db.query(
    "SELECT age, count(*) AS n FROM users \
     WHERE since >= '2026-01-01' GROUP BY age HAVING count(*) > 1 ORDER BY n DESC",
)?;
```

## CLI

```bash
cargo build --release -p clawdb-cli     # produce target/release/clawdb

clawdb query app.clawdb "SELECT count(*) AS n FROM docs"
clawdb repl app.clawdb                  # shell interactivo (.exit para salir)
clawdb tables app.clawdb                # esquemas en JSON
clawdb check app.clawdb                 # verificacion de integridad offline
clawdb compact app.clawdb
clawdb export app.clawdb docs > docs.jsonl
clawdb import app.clawdb docs < docs.jsonl
clawdb repair danada.clawdb rescatada.clawdb   # salvage, nunca silencioso
clawdb serve app.clawdb /tmp/clawdb.sock       # modo sidecar
```

## Multi-worker: el modo sidecar

Para despliegues con varios procesos (gunicorn, PHP-FPM), un solo proceso es dueno del motor y los workers se conectan por Unix socket:

```bash
clawdb serve app.clawdb /tmp/clawdb.sock
```

```python
# cada worker de gunicorn:
from clawdb import SidecarClient
db = SidecarClient("/tmp/clawdb.sock")
db.query("INSERT INTO visits (who) VALUES ('ana')")
db.query("SELECT count(*) AS n FROM visits")
```

Demo reproducible con gunicorn real (4 workers, visitantes concurrentes leyendo y escribiendo sin bloquearse): `examples/gunicorn_demo/run_demo.sh`.

## Bindings

**Python** ([bindings/python/clawdb.py](bindings/python/clawdb.py)) — embebido via C ABI (ctypes libera el GIL en cada llamada: los threads paralelizan de verdad) o via sidecar:

```python
from clawdb import ClawDB

with ClawDB("app.clawdb") as db:
    db.query("CREATE TABLE notes (body text NOT NULL, emb vector(768))")
    db.create_vector_index("notes", "emb", metric="cosine")
    db.query("INSERT INTO notes (body, emb) VALUES ('hola', '[...]')")
    hits = db.search_vector("notes", "emb", embedding, top_k=10, filter={"ws": "acme"})
```

**Node** ([bindings/node/clawdb.js](bindings/node/clawdb.js)) — cliente sidecar sin dependencias:

```js
const { SidecarClient } = require('./clawdb');
const db = await SidecarClient.connect('/tmp/clawdb.sock');
const { rows } = await db.query('SELECT * FROM notes LIMIT 10');
const hits = await db.searchVector('notes', 'emb', embedding, { topK: 10 });
```

**C** — header en [crates/clawdb-ffi/include/clawdb.h](crates/clawdb-ffi/include/clawdb.h); `cargo build --release -p clawdb-ffi` produce `libclawdb`.

## Durabilidad

| Modo | fsync | Ante crash del proceso | Ante crash del SO |
|---|---|---|---|
| `Safe` (default) | En cada commit | No pierde nada | No pierde nada |
| `Balanced` | Cada ~25 ms | No pierde nada | Puede perder los ultimos ms |
| `Fast` | Solo en checkpoints | No pierde nada | Puede perder commits recientes |

```rust
use clawdb_core::{Db, DbOptions, Durability};
let opts = DbOptions { durability: Durability::Balanced, ..Default::default() };
let db = Db::open_or_create_with("app.clawdb", opts)?;
```

## Formato en disco

```text
app.clawdb/
  CLAWDB          # marker + format_version
  LOCK            # exclusion de proceso (flock)
  catalog.json    # tablas, columnas, indices
  manifest        # puntero atomico al estado visible
  manifest.prev   # fallback de recovery
  wal/            # commits durables (CRC por registro)
  segments/       # datos inmutables (CRC por entrada)
  vectors/        # grafos ANN persistidos (CRC; desechables y reconstruibles)
```

Regla de oro del motor: `Data files are canonical. Indexes are disposable.` Si un indice se rompe, se reconstruye desde los datos; si el manifest se rompe, se usa el anterior; si el WAL tiene una entrada incompleta, se descarta completa (nunca medio commit).

## Verificacion de integridad

```rust
let report = clawdb_core::check("app.clawdb")?;
assert!(report.is_ok());
```

Para correr la suite de crash injection y el fuzzing con mas iteraciones:

```bash
CLAWDB_CRASH_ITERS=500 cargo test --release --test crash_kill
CLAWDB_FUZZ_ITERS=5000 cargo test --release --test corruption
```

## Performance

Benchmarks en Apple Silicon (`cargo bench`), ClawDB en modo `Fast` vs SQLite con WAL + `synchronous=OFF` (misma clase de durabilidad):

| Operacion | ClawDB | SQLite | |
|---|---|---|---|
| Lectura por id (10K filas) | 0.62 µs | 2.72 µs | 4.4x mas rapido |
| 1K inserts, commit por operacion | 4.6 ms | 15.9 ms | 3.4x mas rapido |
| 1K inserts, una transaccion | 3.3 ms | 1.2 ms | SQLite gana en batch* |

\* El caso de uso que ClawDB optimiza es el operacional (commits chicos concurrentes), donde gana 3-4x. El bulk-load en una transaccion paga el costo por registro del staging MVCC (~3 µs/registro); las palancas identificadas para cerrarlo (arena de payloads por commit, aplicacion al indice agrupada por tabla) estan anotadas en el plan.

## Licencia

MIT OR Apache-2.0
