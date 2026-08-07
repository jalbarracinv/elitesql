# EliteSQL Specs

## Why it is needed

Modern applications carry more local state, more semi-structured data and more AI capabilities. A small app today ends up running an unnecessary tower: SQLite for metadata, Redis for caching, a vector database for embeddings, loose files for blobs, and a homemade sync or snapshot layer.

That works, but it weighs too much for local, edge, desktop and mobile apps, agents, and small/medium SaaS that need to move fast without operating database infrastructure.

EliteSQL is born as an embedded, lightweight, modern engine: simple like the xBase engines of the 90s in operational philosophy, ergonomic like SQLite, but designed from the start for modern concurrency and native vector search.

## Advantages

- **Embedded and lightweight**: no mandatory server, no daemon, no ceremonial tuning.
- **Self-contained format**: easy to copy, back up, move and inspect.
- **Modern concurrency**: many readers and multiple writers preparing transactions in parallel.
- **Native vectors**: embeddings not as a bolted-on extension but as a first-class type and index.
- **Fail-safe by design**: crash recovery, checksums, WAL replay and consistent snapshots.
- **Small surface**: CRUD, filters, basic joins, indexes and ANN; not a full PostgreSQL recreation.
- **Universal integration**: modern core with a stable C API for bindings in multiple languages.
- **Ideal for AI/local-first**: metadata, documents, blobs, embeddings, semantic search and snapshots in a single engine.

## Product thesis

**A tiny operational database for AI-native apps.**

EliteSQL does not compete head-on with PostgreSQL. It competes against the complexity of assembling and operating:

```text
SQLite + vector DB + cache + sync layer + files + embeddings metadata
```

The promise:

```text
db.open("app.esql")
```

And inside it: records, text, blobs, JSON, regular indexes, ANN vector search, snapshots and sane concurrency.

Technical pitch:

```text
SQLite-fast reads, better concurrent writes, native ANN.
```

## V1 scope

EliteSQL must be an embedded operational database for real applications, not a complete SQL engine.

Included:

- Create and open databases.
- Create tables/collections with a simple schema.
- Insert records.
- Read records by id or index.
- Update records.
- Delete records.
- Scan with simple filters.
- Basic joins.
- Regular indexes.
- Blob storage.
- Native vector type.
- ANN search.
- Basic transactions.
- Snapshots.
- Background compaction.

Not included in V1:

- Triggers.
- Stored procedures.
- Views.
- Materialized views.
- Full outer join.
- Recursive queries.
- Complex CTEs.
- Advanced subqueries.
- Grants/roles.
- A highly complex SQL planner.
- Complex distributed replication.

## Core operations

Minimal conceptual API:

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

## V1 data types

Final recommended list:

- `bool`
- `int64`
- `float64`
- `text`
- `blob`
- `timestamp`
- `json`
- `vector<float32, N>`

Optional for V1.1:

- `decimal`, for exact money.
- `uuid`, though it can start as validated `text`.
- `date`, days since epoch; separating date from timestamp avoids timezone bugs. (Promoted to required — see plan, Phase 2.5.)
- `time`, microseconds since midnight, for time-of-day without a date. (Promoted to required — see plan, Phase 2.5.)
- `vector<int8, N>`, for quantized embeddings.

Types avoided in V1:

- Separate `smallint`, `int32`, `bigint`. `int64` is enough.
- `varchar(n)`. `text` is enough.
- `char`, `nchar`, `nvarchar`.
- `interval`.
- `array`.
- `enum`.

## Query model

EliteSQL may have a small SQL dialect or a structured API. If SQL exists, it must stay deliberately limited.

Supported:

- `SELECT`
- `INSERT`
- `UPDATE`
- `DELETE`
- `WHERE` with simple filters.
- Basic `ORDER BY`.
- `LIMIT`.
- `INNER JOIN`.
- `LEFT JOIN`.
- `RIGHT JOIN`.
- Vector search through an explicit function.
- Basic aggregates (`COUNT`, `SUM`, `AVG`, `MIN`, `MAX`) with simple `GROUP BY`/`HAVING` (V1.1, plan Phase 2.5).

Supported joins:

- Equality over indexable fields.
- One or several simple joins.
- Filters before or after the join when they can be optimized.

Not supported in V1:

- `FULL OUTER JOIN`.
- Recursive joins.
- Complex subqueries.
- A sophisticated cost-based optimizer.

`RIGHT JOIN` may be normalized internally as a `LEFT JOIN` with tables swapped.

## Concurrency

Concurrency is built on:

- MVCC.
- Append-only WAL.
- Optimistic commits.
- Per-version snapshots.
- Background compaction.

Principle:

```text
Readers never block writers. Writers only meet at commit.
```

Flow:

1. Every transaction reads from a stable snapshot.
2. Writers prepare changes without modifying existing records.
3. Writes are appended as new versions in append-only segments.
4. At `commit`, the engine validates conflicts.
5. If there is no conflict, it publishes a new visible version.
6. If there is a conflict, it returns `CONFLICT_RETRY`.

Conflicts:

- `insert` with a new id: normally does not conflict.
- `update` on the same record: conflict if the record changed since the snapshot.
- `delete` on the same record: conflict if the record changed since the snapshot.
- Unique index: conflict if another commit published the same value.
- Vector index: synchronous or asynchronous update depending on mode.

## Storage

Suggested layout:

```text
app.esql/
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

Components:

- **Manifest**: atomic pointer to the currently visible state.
- **Manifest.prev**: previous copy for metadata rollback if the last publish is cut short.
- **WAL**: pending, durable commits.
- **Segments**: append-only data with record versions.
- **Indexes**: regular indexes for field lookups.
- **Vectors**: ANN indexes per vector column.
- **Blobs**: large objects in chunks or separate segments.
- **Snapshots**: references to stable versions.
- **Recovery**: temporary metadata for repair, replay and safe compaction.

The format can be a self-contained directory in V1 to simplify compaction, indexes and blobs. A single-file mode can be explored later.

## Fail-safe and recovery

EliteSQL must assume the process can die at any moment: during an insert, during a commit, during an index update, during compaction, or right after writing to disk.

Goal:

```text
After a crash, the database opens to the last fully committed state.
```

V1 guarantees:

- A commit is either fully visible or not visible at all.
- A partially published version must never remain.
- Readers only ever see valid manifests.
- WAL replay must be idempotent.
- Compaction must never delete data still referenced by a snapshot.
- Derived indexes can be rebuilt from canonical data.
- Blobs must have checksums and verifiable references.

Commit mechanism:

1. Write new records into append-only segments.
2. Write a WAL entry with `txn_id`, change list and checksums.
3. Force durability according to mode (`fsync` or equivalent).
4. Update the indexes required by `sync` mode.
5. Write a new temporary manifest.
6. Validate the temporary manifest's checksum.
7. Atomically rename the temporary manifest to the active manifest.
8. Mark the WAL as applied.

Recovery on open:

1. Read `manifest`.
2. If checksum or version fails, try `manifest.prev`.
3. Scan the WAL from the last applied commit.
4. Reapply complete, valid commits.
5. Ignore incomplete commits or those with invalid checksums.
6. Rebuild indexes marked dirty.
7. Resume or revert incomplete compactions.
8. Leave the DB in a consistent state before accepting writes.

Checksums:

- Manifest: checksum mandatory.
- WAL entries: checksum mandatory per entry.
- Segment blocks: checksum per block or page.
- Blob chunks: checksum per chunk.
- Vector index: metadata checksum; the ANN graph can be rebuilt if marked corrupt.

Durability modes:

- `safe`: fsync on critical commits; slower, recommended default.
- `balanced`: batched fsync; a good balance for apps.
- `fast`: fewer fsyncs; accepts losing the latest commits on a system crash.

Behavior under corruption:

- Open in read-only mode when corruption is unrecoverable.
- Expose `elitesql_check`.
- Expose `elitesql_repair`.
- Allow exporting recoverable records.
- Never "fix" silently by discarding data without reporting it.

Golden rule:

```text
Data files are canonical. Indexes are disposable.
```

If an index breaks, it is rebuilt. If the manifest breaks, the previous one is used. If the WAL has an incomplete entry, it is ignored. If a canonical segment breaks, it is reported and everything possible is recovered without inventing state.

## Indexes

V1 indexes:

- Primary key index.
- Secondary indexes per field.
- Unique indexes.
- Vector ANN index.

Candidate structures:

- B-tree for simple ordered indexes.
- LSM for write-heavy loads.
- HNSW for initial ANN.
- IVF/PQ or quantization for large datasets in future versions.

## Vector Search

Primary type:

```text
vector<float32, N>
```

Metrics:

- cosine
- dot
- l2

Conceptual API:

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

Initial ANN:

- HNSW as the default for ergonomics and quality.
- Simple parameters: `top_k`, `metric`, `ef_search`.
- Advanced parameters hidden or with sensible defaults.

Indexing modes:

- `sync`: the vector is searchable as soon as the commit confirms.
- `async`: the commit finishes fast and the vector enters the index in the background.

## Performance

EliteSQL must optimize for:

- Very fast reads by id.
- Fast sequential inserts.
- Cheap updates through versioning.
- Cheap deletes through tombstones.
- Cheap snapshots.
- Fast vector search with HNSW.
- Good performance with multiple concurrent writers.

Performance decisions:

- Append-only to avoid expensive rewrites.
- mmap for fast reads.
- Cache for hot pages and indexes.
- Batch commits for write-heavy loads.
- Background compaction.
- Checksums on critical blocks with cheap validation.
- Fast recovery using manifest + incremental WAL replay.
- Large blobs kept out of the transactional critical path.

Known limits:

- Large joins without an index will be expensive.
- Massive updates generate garbage until compaction.
- Synchronous vector indexing can slow down commits.
- Huge blobs must be handled through chunks.

## Implementation language

Recommended decision:

```text
Rust inside, C outside.
```

Core:

- Rust for storage, transactions, concurrency, indexes, ANN and the CLI.

Public API:

- Stable C ABI.

Bindings:

- Python.
- Node.js.
- Go.
- Swift/Kotlin later.
- WASM as a future target.

Reasoning:

- Rust offers C/C++-class performance without a garbage collector.
- It reduces memory risks in a complex engine.
- It has a good ecosystem for mmap, parsers, serialization, concurrency, SIMD and FFI.
- A C ABI enables universal integration.

## Conceptual C API

Illustrative example:

```c
elitesql_status elitesql_open(const char *path, elitesql_handle **db);
elitesql_status elitesql_close(elitesql_handle *db);

elitesql_status elitesql_exec(elitesql_handle *db, const char *statement);
elitesql_status elitesql_query(elitesql_handle *db, const char *statement, elitesql_result **result);

elitesql_status elitesql_begin(elitesql_handle *db, elitesql_txn **txn);
elitesql_status elitesql_commit(elitesql_txn *txn);
elitesql_status elitesql_rollback(elitesql_txn *txn);

elitesql_status elitesql_insert(elitesql_txn *txn, const char *table, const elitesql_record *record);
elitesql_status elitesql_get(elitesql_txn *txn, const char *table, const char *id, elitesql_record **record);
elitesql_status elitesql_update(elitesql_txn *txn, const char *table, const char *id, const elitesql_patch *patch);
elitesql_status elitesql_delete(elitesql_txn *txn, const char *table, const char *id);

elitesql_status elitesql_search_vector(
  elitesql_txn *txn,
  const char *table,
  const char *column,
  const float *vector,
  size_t dimensions,
  size_t top_k,
  elitesql_result **result
);
```

## Suggested roadmap

### Phase 0: Prototype

- Basic Rust crate.
- Simple append-only format.
- Insert/get/update/delete.
- Per-version snapshot.
- In-memory primary index.
- Benchmarks against SQLite for basic inserts/reads.

### Phase 1: MVP Storage

- Durable WAL.
- Atomic manifest.
- `manifest.prev` for safe rollback.
- Real MVCC.
- Secondary indexes.
- Transactions with optimistic commit.
- Crash recovery with WAL replay.
- Checksums for manifest, WAL and segments.
- Basic `elitesql_check`.
- Initial compaction.

### Phase 2: Query Layer

- Small SQL dialect or structured query builder.
- Basic WHERE.
- ORDER BY/LIMIT.
- Limited INNER/LEFT/RIGHT JOIN.

### Phase 3: Vector Native

- `vector<float32, N>`.
- HNSW.
- `search_vector`.
- Metadata filters.
- Sync/async indexing mode.

### Phase 4: Developer Experience

- C ABI.
- Python binding.
- Node binding.
- CLI.
- `elitesql check`.
- `elitesql repair`.
- Docs.
- Import/export.
- Reproducible benchmarks.

### Phase 5: Advanced

- Basic full-text.
- Hybrid search.
- Blob chunking.
- Quantized vectors.
- WASM.
- Optional local-first sync.

## Design principles

- Keep the engine small.
- Prefer predictable operations over magic.
- Make the common easy and the advanced explicit.
- Do not chase full SQL compatibility.
- Do not compete with PostgreSQL.
- Do not depend on a server to function.
- Prefer explicit recovery over silent repairs.
- Optimize for AI-native, local-first and edge apps.
- Every feature must justify its weight.
