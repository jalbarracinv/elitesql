# ClawDB Specs

## Por que es necesario

Las aplicaciones modernas estan cargando mas estado local, mas datos semi-estructurados y mas capacidades de IA. Una app pequena hoy termina usando una torre innecesaria: SQLite para metadata, Redis para cache, un vector database para embeddings, archivos sueltos para blobs, y una capa propia de sync o snapshots.

Eso funciona, pero pesa demasiado para apps locales, edge, desktop, mobile, agentes y SaaS pequenos/medianos que necesitan moverse rapido sin operar infraestructura de base de datos.

ClawDB nace como un motor embebido, ligero y moderno: simple como los motores xBase de los 90s en filosofia operativa, ergonomico como SQLite, pero disenado desde el inicio para concurrencia moderna y busqueda vectorial nativa.

## Ventajas

- **Embebido y ligero**: sin servidor obligatorio, sin daemon, sin tuning ceremonial.
- **Formato autocontenido**: facil de copiar, respaldar, mover e inspeccionar.
- **Concurrencia moderna**: muchos lectores y multiples escritores preparando transacciones en paralelo.
- **Vectores nativos**: embeddings no como extension pegada, sino como tipo e indice de primera clase.
- **Fail-safe por diseno**: crash recovery, checksums, WAL replay y snapshots consistentes.
- **Superficie pequena**: CRUD, filtros, joins basicos, indices y ANN; no una recreacion completa de PostgreSQL.
- **Integracion universal**: core moderno con API C estable para bindings en varios lenguajes.
- **Ideal para AI/local-first**: metadata, documentos, blobs, embeddings, busqueda semantica y snapshots en un solo motor.

## Tesis del producto

**A tiny operational database for AI-native apps.**

ClawDB no compite directamente contra PostgreSQL. Compite contra la complejidad de armar y operar:

```text
SQLite + vector DB + cache + sync layer + files + embeddings metadata
```

La promesa:

```text
db.open("app.clawdb")
```

Y adentro tener registros, texto, blobs, JSON, indices normales, busqueda vectorial ANN, snapshots y concurrencia sana.

Pitch tecnico:

```text
SQLite-fast reads, better concurrent writes, native ANN.
```

## Alcance V1

ClawDB debe ser una base de datos operacional embebida para aplicaciones reales, no un motor SQL completo.

Incluye:

- Crear y abrir bases de datos.
- Crear tablas/colecciones con esquema simple.
- Insertar registros.
- Leer registros por id o indice.
- Actualizar registros.
- Eliminar registros.
- Escanear con filtros simples.
- Joins basicos.
- Indices normales.
- Almacenamiento de blobs.
- Tipo vectorial nativo.
- Busqueda ANN.
- Transacciones basicas.
- Snapshots.
- Compaction en background.

No incluye en V1:

- Triggers.
- Stored procedures.
- Views.
- Materialized views.
- Full outer join.
- Recursive queries.
- CTEs complejas.
- Subqueries avanzadas.
- Grants/roles.
- Planner SQL de gran complejidad.
- Replicacion distribuida compleja.

## Operaciones core

API conceptual minima:

```text
open(path)
create(path)
create_table(schema)
insert(table, record)
get(table, id)
update(table, id, patch)
delete(table, id)
scan(table, filter)
query(statement)
search_vector(table, column, vector, top_k, filter?)
search_hybrid(table, text?, vector?, filter?, top_k)
begin()
commit()
rollback()
snapshot()
compact()
```

## Tipos de datos V1

Lista final recomendada:

- `bool`
- `int64`
- `float64`
- `text`
- `blob`
- `timestamp`
- `json`
- `vector<float32, N>`

Opcionales para V1.1:

- `decimal`, para dinero exacto.
- `uuid`, aunque puede iniciar como `text` validado.
- `date`, si separar fecha de timestamp aporta valor real.
- `vector<int8, N>`, para embeddings cuantizados.

Tipos evitados en V1:

- `smallint`, `int32`, `bigint` separados. `int64` basta.
- `varchar(n)`. `text` basta.
- `char`, `nchar`, `nvarchar`.
- `time`, `interval`.
- `array`.
- `enum`.

## Modelo de queries

ClawDB puede tener un dialecto SQL pequeno o una API estructurada. Si existe SQL, debe mantenerse deliberadamente limitado.

Soportado:

- `SELECT`
- `INSERT`
- `UPDATE`
- `DELETE`
- `WHERE` con filtros simples.
- `ORDER BY` basico.
- `LIMIT`.
- `INNER JOIN`.
- `LEFT JOIN`.
- `RIGHT JOIN`.
- Busqueda vectorial con funcion explicita.

Joins soportados:

- Igualdad sobre campos indexables.
- Uno o varios joins simples.
- Filtros antes o despues del join cuando sean optimizables.

No soportado en V1:

- `FULL OUTER JOIN`.
- Joins recursivos.
- Subqueries complejas.
- Optimizador cost-based sofisticado.

`RIGHT JOIN` puede normalizarse internamente como `LEFT JOIN` con tablas invertidas.

## Concurrencia

La concurrencia se basa en:

- MVCC.
- WAL append-only.
- Commits optimistas.
- Snapshots por version.
- Compaction en background.

Principio:

```text
Readers never block writers. Writers only meet at commit.
```

Flujo:

1. Cada transaccion lee desde un snapshot estable.
2. Los writers preparan cambios sin modificar registros existentes.
3. Las escrituras se agregan como nuevas versiones en segmentos append-only.
4. En `commit`, el motor valida conflictos.
5. Si no hay conflicto, publica una nueva version visible.
6. Si hay conflicto, retorna `CONFLICT_RETRY`.

Conflictos:

- `insert` con id nuevo: normalmente no conflictua.
- `update` sobre el mismo registro: conflicto si el registro cambio desde el snapshot.
- `delete` sobre el mismo registro: conflicto si el registro cambio desde el snapshot.
- Indice unico: conflicto si otro commit publico el mismo valor.
- Vector index: actualizacion sincrona o asincrona segun modo.

## Storage

Estructura sugerida:

```text
app.clawdb/
  manifest
  manifest.prev
  wal/
  segments/
  indexes/
  vectors/
  blobs/
  snapshots/
  recovery/
```

Componentes:

- **Manifest**: puntero atomico al estado visible actual.
- **Manifest.prev**: copia anterior para rollback de metadata si el ultimo publish se corta.
- **WAL**: commits pendientes y durables.
- **Segments**: datos append-only con versiones de registros.
- **Indexes**: indices normales para busquedas por campos.
- **Vectors**: indices ANN por columna vectorial.
- **Blobs**: objetos grandes en chunks o segmentos separados.
- **Snapshots**: referencias a versiones estables.
- **Recovery**: metadatos temporales para reparacion, replay y compaction segura.

El formato puede ser carpeta autocontenida en V1 para simplificar compaction, indices y blobs. Mas adelante se puede explorar modo single-file.

## Fail-safe y recovery

ClawDB debe asumir que el proceso puede morir en cualquier momento: durante un insert, durante un commit, durante una actualizacion de indice, durante compaction o justo despues de escribir al disco.

Objetivo:

```text
After a crash, the database opens to the last fully committed state.
```

Garantias V1:

- Un commit es visible completo o no es visible.
- Nunca debe quedar una version parcialmente publicada.
- Readers solo ven manifests validos.
- WAL replay debe ser idempotente.
- Compaction nunca debe borrar datos aun referenciados por un snapshot.
- Indices derivados pueden reconstruirse desde datos canonicos.
- Blobs deben tener checksums y referencias validables.

Mecanismo de commit:

1. Escribir registros nuevos en segmentos append-only.
2. Escribir entrada de WAL con `txn_id`, lista de cambios y checksums.
3. Forzar durabilidad segun modo (`fsync` o equivalente).
4. Actualizar indices requeridos para modo `sync`.
5. Escribir nuevo manifest temporal.
6. Validar checksum del manifest temporal.
7. Renombrar manifest temporal a manifest activo de forma atomica.
8. Marcar WAL como aplicado.

Recovery al abrir:

1. Leer `manifest`.
2. Si falla checksum o version, probar `manifest.prev`.
3. Escanear WAL desde el ultimo commit aplicado.
4. Reaplicar commits completos y validos.
5. Ignorar commits incompletos o con checksum invalido.
6. Reconstruir indices marcados como dirty.
7. Reanudar o revertir compactions incompletas.
8. Dejar la DB en estado consistente antes de aceptar writes.

Checksums:

- Manifest: checksum obligatorio.
- WAL entries: checksum obligatorio por entrada.
- Segment blocks: checksum por bloque o pagina.
- Blob chunks: checksum por chunk.
- Vector index: checksum de metadata; el grafo ANN puede reconstruirse si se marca corrupto.

Modos de durabilidad:

- `safe`: fsync en commits criticos; mas lento, recomendado por defecto.
- `balanced`: batch fsync; buen equilibrio para apps.
- `fast`: menos fsync; acepta perdida de ultimos commits ante crash del sistema.

Comportamiento ante corrupcion:

- Abrir en modo read-only si hay corrupcion no recuperable.
- Exponer `clawdb_check`.
- Exponer `clawdb_repair`.
- Permitir exportar registros recuperables.
- Nunca "arreglar" silenciosamente descartando datos sin reportarlo.

Regla de oro:

```text
Data files are canonical. Indexes are disposable.
```

Si un indice se rompe, se reconstruye. Si el manifest se rompe, se usa el anterior. Si el WAL tiene una entrada incompleta, se ignora. Si un segmento canonico se rompe, se reporta y se recupera todo lo posible sin inventar estado.

## Indices

Indices V1:

- Primary key index.
- Secondary indexes por campo.
- Unique indexes.
- Vector ANN index.

Estructuras candidatas:

- B-tree para indices ordenados simples.
- LSM para escrituras intensas.
- HNSW para ANN inicial.
- IVF/PQ o cuantizacion para datasets grandes en versiones futuras.

## Vector Search

Tipo principal:

```text
vector<float32, N>
```

Metricas:

- cosine
- dot
- l2

API conceptual:

```text
search_vector(
  table: "documents",
  column: "embedding",
  vector: [...],
  top_k: 20,
  filter: { "workspace_id": "abc" },
  metric: "cosine"
)
```

ANN inicial:

- HNSW como default por ergonomia y calidad.
- Parametros simples: `top_k`, `metric`, `ef_search`.
- Parametros avanzados ocultos o con defaults razonables.

Modos de indexacion:

- `sync`: el vector queda searchable al confirmar el commit.
- `async`: el commit termina rapido y el vector entra al indice en background.

## Performance

ClawDB debe optimizar para:

- Lecturas por id muy rapidas.
- Inserts secuenciales rapidos.
- Updates baratos por versionado.
- Deletes baratos por tombstone.
- Snapshots baratos.
- Vector search rapido con HNSW.
- Buen rendimiento con multiples writers concurrentes.

Decisiones de performance:

- Append-only para evitar reescrituras caras.
- mmap para lecturas rapidas.
- Cache de paginas e indices calientes.
- Batch commits para cargas de escritura.
- Compaction en background.
- Checksums por bloques criticos con validacion barata.
- Recovery rapido usando manifest + WAL replay incremental.
- Blobs grandes fuera del camino critico transaccional.

Limites conocidos:

- Joins grandes sin indice seran caros.
- Updates masivos generan basura hasta compactar.
- Vector indexing sincrono puede hacer commits lentos.
- Blobs enormes deben manejarse por chunks.

## Lenguaje de implementacion

Decision recomendada:

```text
Rust inside, C outside.
```

Core:

- Rust para storage, transacciones, concurrencia, indices, ANN y CLI.

API publica:

- C ABI estable.

Bindings:

- Python.
- Node.js.
- Go.
- Swift/Kotlin posteriormente.
- WASM como target futuro.

Razonamiento:

- Rust ofrece performance tipo C/C++ sin garbage collector.
- Reduce riesgos de memoria en un motor complejo.
- Tiene buen ecosistema para mmap, parsers, serializacion, concurrencia, SIMD y FFI.
- C ABI permite integracion universal.

## API C conceptual

Ejemplo orientativo:

```c
clawdb_status clawdb_open(const char *path, clawdb_handle **db);
clawdb_status clawdb_close(clawdb_handle *db);

clawdb_status clawdb_exec(clawdb_handle *db, const char *statement);
clawdb_status clawdb_query(clawdb_handle *db, const char *statement, clawdb_result **result);

clawdb_status clawdb_begin(clawdb_handle *db, clawdb_txn **txn);
clawdb_status clawdb_commit(clawdb_txn *txn);
clawdb_status clawdb_rollback(clawdb_txn *txn);

clawdb_status clawdb_insert(clawdb_txn *txn, const char *table, const clawdb_record *record);
clawdb_status clawdb_get(clawdb_txn *txn, const char *table, const char *id, clawdb_record **record);
clawdb_status clawdb_update(clawdb_txn *txn, const char *table, const char *id, const clawdb_patch *patch);
clawdb_status clawdb_delete(clawdb_txn *txn, const char *table, const char *id);

clawdb_status clawdb_search_vector(
  clawdb_txn *txn,
  const char *table,
  const char *column,
  const float *vector,
  size_t dimensions,
  size_t top_k,
  clawdb_result **result
);
```

## Roadmap sugerido

### Phase 0: Prototype

- Rust crate basica.
- Formato append-only simple.
- Insert/get/update/delete.
- Snapshot por version.
- Indice primario en memoria.
- Benchmarks contra SQLite para inserts/reads basicos.

### Phase 1: MVP Storage

- WAL durable.
- Manifest atomico.
- `manifest.prev` para rollback seguro.
- MVCC real.
- Secondary indexes.
- Transactions con commit optimista.
- Crash recovery con WAL replay.
- Checksums para manifest, WAL y segmentos.
- `clawdb_check` basico.
- Compaction inicial.

### Phase 2: Query Layer

- Dialecto SQL pequeno o query builder estructurado.
- WHERE basico.
- ORDER BY/LIMIT.
- INNER/LEFT/RIGHT JOIN limitados.

### Phase 3: Vector Native

- `vector<float32, N>`.
- HNSW.
- `search_vector`.
- Filtros por metadata.
- Modo sync/async para indexing.

### Phase 4: Developer Experience

- C ABI.
- Python binding.
- Node binding.
- CLI.
- `clawdb check`.
- `clawdb repair`.
- Docs.
- Import/export.
- Benchmarks reproducibles.

### Phase 5: Advanced

- Full-text basico.
- Hybrid search.
- Blob chunking.
- Quantized vectors.
- WASM.
- Sync local-first opcional.

## Principios de diseno

- Mantener el motor pequeno.
- Preferir operaciones predecibles sobre magia.
- Hacer facil lo comun y explicito lo avanzado.
- No perseguir compatibilidad SQL completa.
- No competir con PostgreSQL.
- No depender de un servidor para funcionar.
- Preferir recovery explicito sobre reparaciones silenciosas.
- Optimizar para apps AI-native, local-first y edge.
- Cada feature debe justificar su peso.
