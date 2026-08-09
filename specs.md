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

## Resource philosophy

**"Lite" is a resource contract, not just a packaging or deployment property.**

EliteSQL must be prudent with memory, disk and background CPU. An embedded
database should not assume server-class resources, and it should not maximize
throughput by silently retaining every derived structure in memory. When the
canonical dataset or an index is larger than RAM, the engine should remain
usable with an explicit and understandable latency tradeoff rather than fail
because the whole structure must be materialized.

Resource principles:

- Prefer compact layouts, paging, read-only `mmap` and bounded caches before
  adding lossy transformations.
- Make resident memory follow the active working set where practical, not the
  total logical database size.
- Avoid duplicate full-size representations of canonical data unless a
  measured performance benefit justifies their cost.
- Treat steady-state memory and peak memory during open, recovery, index build
  and compaction as separate design constraints. A low steady RSS does not
  excuse a startup or maintenance spike that can exhaust an embedded device.
- Give indexes and background work explicit memory budgets or bounded modes as
  they mature. Under pressure, degrade predictably through paging, smaller
  caches or slower execution; do not depend on an eventual out-of-memory kill.
- Apply the same contract to traditional queries. Scans and projections run in
  batches; blocking operators such as `ORDER BY`, `GROUP BY`, `DISTINCT` and
  joins must use top-k, partitioning or temporary spill files when their
  working set exceeds the query budget.
- Do not require callers to materialize unbounded result sets. Provide cursors
  or batch APIs with backpressure; convenience APIs that return all rows may
  necessarily retain the caller-owned result, but must not also retain a full
  duplicate execution representation.
- Account for concurrent queries, indexes and maintenance under a database-wide
  budget. A per-query limit is the first enforcement boundary, not permission
  for every concurrent operator to consume that amount independently.
- Keep canonical values exact. Quantization, compression or other
  quality-changing techniques must be explicit, measurable and optional.
- Do not trade away crash safety, deterministic behavior or search quality
  silently in the name of a smaller footprint.

This does not mean every byte must stay on disk. Small and hot structures may
be fully resident when that is the best tradeoff. It means full residency is a
conscious policy choice, not an accidental consequence of the implementation.

Reference performance acceptance targets:

- On matched durability and data, traditional SQL operations should remain
  within 2x SQLite end-to-end; a result close to the boundary must be repeated
  and include deferred maintenance rather than hiding it after timing.
- EliteSQL should beat SQLite where its architecture is differentiated:
  sustained concurrent-writer throughput, bounded worst lock tails, native ANN
  search, and the specialized sorted bulk path.
- Vector search should maintain ANN recall@10 of at least 0.95 at the selected
  quality profile and report latency together with corpus size and dimension.
- Logical pools are hard admission boundaries. Engine-owned heap must not grow
  with total database size merely because a query, checkpoint or index is
  large; use paging, streaming, spill or a pre-publication `MemoryLimit`.
- Clean reclaimable mmap residency is reported separately from dirty/physical
  footprint. Neither RSS alone nor logical counters alone prove compliance.

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
query_params(statement, positional_values | named_values)
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
- `int` (signed 64-bit; `integer`, `bigint`, and `int64` are aliases)
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

- Separate `smallint` and `int32`. The single 64-bit integer type is enough; `bigint` is accepted only as an alias.
- `varchar(n)`. `text` is enough.
- `char`, `nchar`, `nvarchar`.
- `interval`.
- `array`.
- `enum`.

## Query model

EliteSQL has a small SQL dialect and a structured Rust API. The dialect stays
deliberately limited.

Supported:

- `SELECT`
- `INSERT`
- `UPDATE`
- `DELETE`
- `WHERE` with simple filters.
- Basic `ORDER BY`.
- `LIMIT` and `OFFSET`, as non-negative literals or bound parameters.
- `INNER JOIN`.
- `LEFT JOIN`.
- `RIGHT JOIN`.
- Vector search through an explicit function.
- Basic aggregates (`COUNT`, `SUM`, `AVG`, `MIN`, `MAX`) with simple `GROUP BY`/`HAVING` (V1.1, plan Phase 2.5).

Parameter contract:

- SQL and parameter values travel separately through every public binding.
- Positional `?` and `%s`, and named `%(name)s`, are parsed as placeholder AST
  nodes and bound only after parsing. Placeholder-looking text inside literals
  or comments is never treated as a parameter.
- Values retain their logical types; bindings must not quote, escape or
  interpolate them into the SQL string.
- Positional counts and named keys match exactly. Missing, extra or mixed-style
  parameters are errors; a named placeholder may be reused.
- Parameters are accepted anywhere the dialect accepts a literal, including
  writes, predicates, `IN`, defaults, `LIMIT` and `OFFSET`.

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

Memory efficiency is part of performance, not a secondary optimization. Major
benchmarks and index changes must report, where relevant:

- steady resident/physical memory;
- peak memory during open, build, recovery and compaction;
- peak working memory per SQL operator and per concurrent query workload;
- spill bytes/files and the latency introduced by spilling;
- bytes per record or vector and total derived-index size;
- cold-cache and warm-cache latency;
- the quality metric associated with any memory tradeoff (for example ANN
  recall when quantization is enabled).

Optimizations should be evaluated on datasets both below and above the chosen
memory budget. A benchmark that fits entirely in RAM does not by itself prove
that an index is appropriate for a lightweight embedded database.

Current implementation of this contract:

- Each `Db` has a validated global envelope split into concurrent-query,
  mutable-index and maintenance pools plus an unallocatable emergency reserve.
  RAII permits apply backpressure across every clone/client; metrics expose
  current/peak use, waits and consolidations.
- Scans and cursor reads decode bounded batches. `ORDER BY` and
  high-cardinality `GROUP BY` create and merge bounded sorted runs. Temporary
  spill files are removed on success and error.
- The MVCC primary directory is a set of immutable, paged, checksummed `mmap`
  runs plus an active WAL delta. On primary-only workloads, an automatic
  checkpoint freezes that delta in O(1), immediately opens a new active
  generation, and writes the frozen generation on a dedicated background
  thread. Reads merge both generations until publication. The worker retains
  one maintenance-pool permit for the frozen heap and its bounded writer,
  preventing this overlap from becoming an unaccounted memory copy. It copies
  only WAL records after the freeze boundary into the next WAL before the
  manifest swap, so commits made during the flush remain recoverable. Explicit
  checkpoint, DDL, compaction and shutdown are barriers. Workloads with
  equality, BM25 or vector deltas currently keep the synchronous checkpoint
  path until those operations carry an equivalent MVCC freeze boundary. Groups of
  sixteen same-level primary runs are merged by a background worker; disjoint
  V2 ranges copy their checksummed pages without entry-by-entry reconstruction.
  The stable base is rewritten only by canonical data compaction. The atomic
  run manifest is tied to the exact segment set and catalog table epochs, not
  only the commit number. Paged format V2 stores one heap offset per page and
  leaves keys/file contents in mmap. Checkpoint snapshots intern table names,
  store IDs contiguously and build the primary run directly from captured
  segment offsets instead of mutating and rescanning the delta.
  Writable startup rebuilds stale/missing run state through bounded external
  runs rather than a database-sized resident map.
- Equality and BM25 indexes use immutable versioned operation runs plus bounded
  mutable deltas. Additions and tombstones are keyed by `(value,id)` or
  `(term,id)`; eight same-level runs merge in the background, retaining only
  the newest operation per key. Hot-key pagination uses one cursor per run and
  stops at the requested batch size. BM25 also persists exact document-count
  and total-token metadata and merges posting streams into an exact bounded
  top-k.
- HNSW V4 stores vectors, labels, and adjacency lists in a sectioned file
  queried directly through `mmap`; post-open graph deltas freeze into mmap
  overlays when the index pool is pressured. Vector candidate growth is capped
  by the admitted query budget.
- Vector dimensionality does not change this policy: 256-, 768- and
  1000+-dimensional canonical values stay exact. PCA or another lossy reduction
  is not required for residency; optional int8 quantization is a separately
  measurable quality/size choice.
- Transaction staging interns each touched table/schema, preserves insertion
  position and skips sorting for monotonic primary-key batches. A cached
  snapshot high-watermark avoids a shared-state lock per append while commit
  validation remains authoritative for concurrent conflicts.
- The default deployment profile is a bounded 384 MiB envelope (64 MiB query,
  128 MiB index-delta and maintenance pools; 64 MiB memtable target), sized to
  retain the measured 100K x 64-dimensional HNSW graph across restart.
- `DbOptions::ingest_performance()` is an explicit bounded 512 MiB deployment
  profile (64 MiB query, 192 MiB index-delta and maintenance pools; 128 MiB
  memtable target).
- Unindexed equality joins use recursive Grace partitioning and spill. A skewed
  partition uses bounded block probing, including outer-join match state on
  disk.
- Transactions are size-checked before durability, and primary compaction
  streams the replacement segment, paged directory and blob-reference set
  under the maintenance admission instead of materializing full copies.
- `bulk_insert_sorted` streams strictly increasing explicit IDs into one
  canonical segment and primary run, publishes the batch atomically, and
  requires derived indexes to be built afterward.

Known limits:

- Primary level promotion still adds variable drain latency and about 1.53 GiB
  of read-plus-write traffic for 533 MiB of checkpoint runs in the reference
  10M transactional load. Further write-amplification reduction remains useful.
- With four to eight writers, EliteSQL wins throughput and worst-tail latency
  but its p99 transaction latency remains above SQLite in the reference run.
- Large joins without an index trade RAM for partitioning and temporary I/O.
- Massive updates generate garbage until compaction.
- Synchronous vector indexing can slow down commits.
- A single HNSW graph build must fit the configured maintenance pool; EliteSQL
  returns `MemoryLimit` instead of exceeding it.
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
- SQL values cross every binding separately from statement text. The parser
  represents placeholders explicitly and the executor binds typed values only
  after parsing; bindings must never implement parameters with string
  substitution.

## Conceptual C API

Illustrative example:

```c
elitesql_status elitesql_open(const char *path, elitesql_handle **db);
elitesql_status elitesql_close(elitesql_handle *db);

elitesql_status elitesql_exec(elitesql_handle *db, const char *statement);
elitesql_status elitesql_query(elitesql_handle *db, const char *statement, elitesql_result **result);
elitesql_status elitesql_query_params(elitesql_handle *db, const char *statement,
                                      const char *params_json, elitesql_result **result);

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

### Cross-cutting: bounded resources and safe parameters

- One validated database-wide memory envelope with query, mutable-index and
  maintenance pools plus an emergency reserve.
- Paged read-only `mmap` runs with bounded deltas and leveled background
  promotion for primary, equality and BM25; immutable mmap graph overlays for
  HNSW.
- Batched scans/cursors and spill-capable sort, aggregation and equality joins.
- Typed positional and named SQL parameters in Rust, C, Python, Node and the
  sidecar protocol; no string substitution.

## Design principles

- Keep the engine small.
- Be conservative with memory; "embedded" must not mean "loads everything".
- Optimize for a bounded working set and make full residency an explicit
  tradeoff.
- Prefer predictable operations over magic.
- Make the common easy and the advanced explicit.
- Do not chase full SQL compatibility.
- Do not compete with PostgreSQL.
- Do not depend on a server to function.
- Prefer explicit recovery over silent repairs.
- Optimize for AI-native, local-first and edge apps.
- Every feature must justify its weight.
