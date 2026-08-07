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
| Phase 3 | Tipo vectorial + busqueda ANN (HNSW) | Pendiente |
| Phase 4 | C ABI, bindings Python/Node, CLI, modo sidecar | Pendiente |

Verificacion actual: 64 tests (MVCC, recovery, compaction, modelo aleatorio, suite SQL), crash injection con `kill -9` de procesos reales, y fuzzing de corrupcion de archivos y del parser SQL (la base nunca entra en panico ni acepta estado invalido). Detalles en [specs.md](specs.md) y [plan.md](plan.md).

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

### SQL

El mismo motor expone un dialecto SQL deliberadamente pequeno — referencia completa con ejemplos en [manual.md](manual.md):

```rust
use clawdb_core::QueryOutput;

db.query("CREATE TABLE users (name text NOT NULL, email text, age int64)")?;
db.query("CREATE UNIQUE INDEX ON users (email)")?;
db.query("INSERT INTO users (name, email, age) VALUES ('ana', 'ana@x.com', 30)")?;

if let QueryOutput::Rows { columns, rows } = db.query(
    "SELECT u.name, o.amount FROM users u \
     JOIN orders o ON o.user_id = u.id \
     WHERE u.email = 'ana@x.com' ORDER BY o.amount DESC LIMIT 10",
)? {
    // ...
}
```

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
| 1K inserts, commit por operacion | 5.3 ms | 15.9 ms | 3.0x mas rapido |
| 1K inserts, una transaccion | 3.2 ms | 1.2 ms | SQLite gana en batch (pendiente de optimizar) |

## Licencia

MIT OR Apache-2.0
