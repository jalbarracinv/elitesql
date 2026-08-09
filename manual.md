# EliteSQL SQL Manual

Reference for the EliteSQL SQL dialect (V1). It is a deliberately small subset: it covers what an operational app needs and rejects everything else with an explicit error that says what to use instead.

## Running SQL

```rust
use elitesql_core::{Db, QueryOutput, Record, Value};

let db = Db::open_or_create("app.esql")?;
match db.query("SELECT name FROM users WHERE age > 30")? {
    QueryOutput::Rows { columns, rows } => { /* SELECT */ }
    QueryOutput::Inserted { ids }        => { /* INSERT: generated ids */ }
    QueryOutput::Affected(n)             => { /* UPDATE/DELETE: affected rows */ }
    QueryOutput::None                    => { /* DDL */ }
}
```

The same SQL runs from the CLI. `Db::open_or_create` creates a database from
Rust, but the CLI never creates one just by opening a path — a single argument
that is not a subcommand is read as a path, so a mistyped subcommand would
otherwise leave a database named after the typo:

```bash
elitesql --create app.esql                          # once, to create it
elitesql query app.esql "SELECT name FROM users"    # afterwards, no flag
```

General rules:

- One statement per `query()` call.
- Case-insensitive keywords (`SELECT` = `select`).
- Line comments with `--` and block comments with `/* ... */`.
- SELECT reads the latest committed state (read committed). For snapshot-consistent reads use the Rust API (`db.snapshot()` + `scan_at`/`get_at`).
- UPDATE/DELETE run inside a transaction with automatic retries on optimistic conflict. A multi-row INSERT is a single atomic commit: all rows land or none do.

### Bound parameters

Applications should send SQL and values separately. EliteSQL supports
positional `?` and `%s`, plus Python DB-API-style named `%(name)s` placeholders:

```rust
db.query_params(
    "SELECT * FROM users WHERE email = %s LIMIT ?",
    &[Value::Text(email), Value::Int64(10)],
)?;

let mut params = Record::new();
params.insert("email".into(), Value::Text(email));
params.insert("limit".into(), Value::Int64(10));
db.query_named_params(
    "SELECT * FROM users WHERE email = %(email)s LIMIT %(limit)s",
    &params,
)?;
```

Binding happens after lexing/parsing and never rewrites the SQL string.
Placeholders are legal anywhere this dialect accepts a literal, including
`INSERT`, `UPDATE SET`, predicates, `IN`, defaults, `LIMIT` and `OFFSET`.
Positional counts and named keys must match exactly; missing, extra or
wrong-style parameters return `InvalidArgument`. `LIMIT`/`OFFSET` require a
non-negative `int64`.

Named parameters are case-sensitive and may be referenced more than once.
Calling plain `query()` with a statement that contains a placeholder is an
error because no parameter set was supplied. Parameterized cursors use
`query_cursor_params` or `query_cursor_named_params` and follow the same strict
rules.

Python exposes the same operation through `query(sql, params=None)`:

```python
db.query("INSERT INTO events (name, payload, happened) VALUES (%s, %s, %s)",
         [name, payload_bytes, datetime_value])
db.query("SELECT * FROM events WHERE name = %(name)s LIMIT %(limit)s",
         {"name": name, "limit": 20})
```

Python values retain their types: `None`, `bool`, signed 64-bit `int`, `float`,
`str`, bytes-like objects, `datetime`, `date`, `time`, dict/list JSON values and
numeric lists for vector columns. A parameter such as `"x' OR TRUE --"` is a
text value, not executable SQL.

The embedded C ABI uses `elitesql_query_params` with a JSON array (positional)
or object (named). The sidecar accepts the same value in the optional `params`
field, and both Python's `SidecarClient` and Node's `query(sql, params)` encode
it automatically. The tagged transport can preserve blobs, dates/times,
timestamps, JSON, vectors, non-finite floats and lossless `int64`. Python maps
its date/time classes directly; Node maps `Date` to a timestamp and `BigInt` to
an exact `int64`.

## Running SQL from another process, or another host

A database directory is owned by **one process**. When other processes need it,
or when the app runs on a different machine, that owner serves them:

```bash
elitesql serve app.esql /tmp/elitesql.sock     # same host
elitesql serve app.esql --tcp 127.0.0.1:7070   # another host (needs a token)
```

```python
db = SidecarClient("/tmp/elitesql.sock")                    # Unix socket
db = SidecarClient(host="db-host", port=7070, token=tok)    # TCP
db.query("SELECT name FROM users WHERE email = %s", ["ana@x.com"])
```

**The SQL behaves identically over either transport, and identically to the
embedded API.** It is the same engine in the same process: the same dialect, the
same MVCC and read-committed reads, the same automatic retry on UPDATE/DELETE,
the same atomic multi-row INSERT, the same error codes. Nothing in this manual
changes because a query arrived over a socket. Only three things differ, and
none of them is a semantic difference:

| | Unix socket | TCP |
|---|---|---|
| Authentication | filesystem permissions on the socket | a shared token, required |
| Encryption | not applicable (never leaves the host) | none — use an SSH tunnel or a VPN |
| Round trip | tens of microseconds | ~0.5 ms local, 10–50 ms across regions |

The token handshake is sent by the clients themselves, on connect and on
reconnect, so application code is the same for both transports.

The latency line is the one to design around. A point lookup is ~4 µs, so over
TCP the network costs about a hundred times more than the query. That does not
change what a query *returns*, but it changes which shapes are sensible:
prefer one query returning many rows over many queries returning one row each,
and be wary of ORM N+1 patterns that were free when the engine was in-process.

Current limits of the server mode, so you can tell what it is not:

- **One database per server process.** There is no `USE db`; `serve` opens the
  directory you name and serves that one. Several databases mean several
  processes on different sockets or ports, each with its own memory budget.
- **No TLS**, as above.
- **No session transactions.** There is no `BEGIN`/`COMMIT` over the wire, the
  same as in embedded SQL; multi-statement transactions use the Rust `Txn` API
  in the process that owns the database.
- **Connections are capped** (`--max-connections`, default 128) because each one
  costs a thread. Past the cap the server answers with a refusal rather than
  queueing.

Never share a database directory over NFS or SMB instead of using the sidecar.
Durability relies on `fsync` plus atomic `rename`, and immutable index bases are
read through `mmap`; network filesystems provide neither reliably, and two
machines writing the same directory is not the ownership model.

## Types and literals

| Type | SQL literal | Example |
|---|---|---|
| `bool` | `TRUE` / `FALSE` | `TRUE` |
| `int` | 64-bit integer | `42`, `-7` |
| `float64` | decimal or integer | `3.14`, `3` |
| `text` | single-quoted string | `'hello'`, `'it''s ok'` (escape `''`) |
| `blob` | hex literal | `X'DEADBEEF'` |
| `timestamp` | string `'YYYY-MM-DD HH:MM:SS[.ffffff]'` (UTC) or integer (Unix microseconds) | `'2026-08-07 09:30:00'` |
| `date` | string `'YYYY-MM-DD'` (days since epoch internally) | `'2026-08-07'` |
| `time` | string `'HH:MM:SS[.ffffff]'` (microseconds since midnight) | `'09:30:00'` |
| `json` | string containing valid JSON | `'{"tags": ["a"], "n": 3}'` |
| `vector(N)` | string containing a JSON array of N numbers | `'[0.12, -0.5, 0.33]'` |
| null | `NULL` | `NULL` |

`date`, `time` and `timestamp` literals are truly validated: `'2026-02-30'` or `'25:00:00'` fail with a clear error. In WHERE, a string coerces automatically against date/time/timestamp columns: `WHERE day >= '2026-01-01'` or `WHERE at < '2026-08-07 18:00:00'` work without any cast syntax (and through indexes too).

About `timestamp` (EliteSQL's "datetime"): it represents an instant in UTC microseconds. The literal accepts a space or `T` separator, an optional `Z` suffix, and a date-only form (`'2026-08-08'` = midnight UTC). There are no timezone offsets (`+05:00`): the engine stores instants; timezone presentation belongs to the application.

**Date differences**: computed in the application, and the representation makes them trivial — `date` is days since epoch, so subtracting two `Value::Date` values gives the difference in days directly (the engine already resolved leap years when parsing); `time` and `timestamp` subtract in microseconds. Inside a query, express "last N days" or "between dates" as ranges, which also use indexes:

```sql
SELECT * FROM orders WHERE order_date >= '2026-07-08'                      -- bound computed by the app
SELECT * FROM events WHERE day >= '2026-08-01' AND day < '2026-09-01'      -- indexable range
```

Arithmetic inside SQL (`date2 - date1` in projections or HAVING) is left for a future minimal-expressions phase.

Automatic coercions: an integer is valid for `float64` and `timestamp` columns. A string is valid for `json` only if it parses as JSON.

## The implicit primary key

Every table has an `id` column of type `text` that is **not declared**. If you don't provide it on INSERT, the engine generates a [ULID](https://github.com/ulid/spec) (26 characters, time-sortable). You may provide it explicitly, and it cannot be changed with UPDATE.

## CREATE TABLE

```sql
CREATE TABLE users (
  name      text NOT NULL,
  email     text,
  age       int,
  plan      text NOT NULL DEFAULT 'free',
  prefs     json,
  embedding vector(768)
)
```

- Columns are nullable by default; `NOT NULL` to require a value.
- `DEFAULT <literal>` supplies the value when a write omits the column. A `NOT NULL` column with a `DEFAULT` may therefore be omitted on INSERT; without one it is required.
- There is no `PRIMARY KEY` (it's the implicit `id`), no `REFERENCES`, no inline `UNIQUE` (use `CREATE UNIQUE INDEX`).
- `int` is the recommended integer spelling. `integer`, `bigint`, and `int64` are aliases for the same signed 64-bit type; they have identical storage and behavior. `smallint` and `int32` are not separate types.
- Other type names are exact: for example, `varchar` fails with an error pointing to `text`.

## CREATE INDEX

```sql
CREATE INDEX ON orders (user_id)          -- equality index
CREATE UNIQUE INDEX ON users (email)      -- uniqueness validated at every commit
CREATE UNIQUE INDEX idx_email ON users (email)  -- the name is optional
```

- One index per column; multi-column is not supported in V1.
- `NULL`s do not participate in uniqueness (several rows may have `email NULL`).
- Creating a unique index over data with existing duplicates fails.
- The planner uses indexes automatically on WHERE equalities and in JOINs.

## ALTER TABLE

```sql
ALTER TABLE users ADD COLUMN nickname text                      -- instant; older records read NULL
ALTER TABLE users ADD COLUMN plan text NOT NULL DEFAULT 'free'   -- backfills the records already stored
ALTER TABLE users DROP COLUMN age
ALTER TABLE users DROP COLUMN IF EXISTS age
ALTER TABLE users RENAME COLUMN age TO years
ALTER TABLE users RENAME TO people
```

The keyword `COLUMN` is optional after `ADD`, `DROP` and `RENAME`.

What each one costs, because the difference is large and worth knowing before you run it on a big table:

| Statement | Cost | What happens |
|---|---|---|
| `ADD COLUMN c T` (no `DEFAULT`) | catalog only — instant whatever the size of the table | no record is rewritten; those written earlier read `c` as `NULL` |
| `ADD COLUMN c T DEFAULT v` | one pass of writes over the table | records written earlier are backfilled with `v`, in batches, so memory stays flat |
| `ADD COLUMN c T NOT NULL DEFAULT v` | the same pass, plus enforcement at the end | `NOT NULL` is published only once every record has a value |
| `DROP COLUMN c` | a compaction | the values leave the stored records; any index on `c` is dropped with it |
| `RENAME COLUMN a TO b` | a compaction | column names live inside every record payload; any index on `a` follows the rename |
| `RENAME TO t2` | a compaction | records are keyed by table name on disk |

- `ADD COLUMN ... NOT NULL` without a `DEFAULT` is refused on a table that already holds records — there would be no value to give them. Give it a `DEFAULT`, or add it nullable and fill it with `UPDATE`.
- A `DEFAULT` added this way also applies to later writes that omit the column.
- Renaming onto a name that already exists (a table or a column) is refused, and so is dropping the only column of a table — drop the table instead.
- Changing a column's **type or nullability** is not supported. Do it explicitly, so the conversion is yours: add the new column, copy the values with `UPDATE`, drop the old one.

## DROP TABLE and DROP INDEX

```sql
DROP TABLE users
DROP TABLE IF EXISTS users

DROP INDEX ON users (email)             -- an index is identified by its column
DROP INDEX idx_email ON users (email)   -- the name is accepted and ignored, as in CREATE INDEX
DROP INDEX IF EXISTS ON users (email)
```

- `DROP TABLE` costs the same on one record or a billion. It unlinks the table from the catalog, which is what makes the drop durable: the records become unreachable immediately — a `SELECT`, a reopen, or a re-`CREATE TABLE` of the same name will never see them again. The **disk space** they occupied in existing segments is returned by the next compaction (`elitesql compact` or `Db::compact`).
- `DROP INDEX` only removes the index. The column and its values stay; queries fall back to a scan, and a dropped `UNIQUE` index stops rejecting duplicates.
- Index names are not stored (`CREATE INDEX` accepts one and derives its own), so an index is dropped the same way it is created: by table and column.
- Vector (ANN) and full-text indexes are created through the Rust API and dropped there too: `Db::drop_vector_index` and `Db::drop_text_index`. Dropping a vector index also deletes its persisted graph.

### Schema changes and crashes

- Statements that only touch the catalog — `DROP TABLE`, `DROP INDEX`, `ADD COLUMN` without a default — are a single durable catalog write (temp file, fsync, rename, fsync of the directory). After a crash the change is either entirely there or entirely absent.
- Statements that must also rewrite data record what they are about to do in `ddl.json` before the first step. If the process dies halfway, the next read-write open replays the operation to completion before serving anything, and `elitesql check` reports it in the meantime as a warning. Every step is idempotent, so replaying one that already finished changes nothing. This is covered by a crash-injection test that SIGKILLs a process running schema changes in a loop and verifies the database always comes back with a consistent schema and every record intact.
- DDL is **not transactional and not versioned by MVCC**: a schema change applies to every reader at once, including open snapshots and transactions already in flight (whose commits then fail with `table not found`). As with any database, run schema changes when nothing depends on the old shape.
- A read-only handle rejects every DDL statement with `ReadOnly`, and does not replay a pending change.

The Rust API mirrors all of it: `drop_table`, `drop_index`, `drop_vector_index`, `drop_text_index`, `add_column`, `drop_column`, `rename_table`, `rename_column`.

## Maintenance and automatic compaction

Normal applications do not need to issue a compaction command. Automatic
compaction is enabled by default and is evaluated after checkpoints and when a
database is reopened. It runs on a single background worker and preserves every
version required by a live snapshot.

The default policy compacts when:

- at least 10,000 updates/deletes/replacements have accumulated **and** at
  least 25% of immutable segment bytes are estimated to be reclaimable;
- reclaimable segment space reaches 256 MiB, regardless of operation count; or
- 64 immutable segments have accumulated, including insert-only workloads.

Attempts are limited to one per minute to avoid maintenance thrashing. A
manual `elitesql compact app.esql` or `Db::compact()` ignores the automatic
thresholds and runs immediately. Advanced callers can tune
`DbOptions::auto_compaction`, disable it with
`AutoCompactionOptions::disabled()`, and observe it through
`Db::maintenance_stats()`.

## INSERT

```sql
INSERT INTO users (name, email, age) VALUES ('ana', 'ana@x.com', 30)

-- Multi-row: one atomic commit. Returns the ids in order.
INSERT INTO users (name, age) VALUES ('bob', 25), ('eva', 41)

-- With an explicit id:
INSERT INTO users (id, name) VALUES ('u-admin', 'root')

-- The column list may be omitted; values then follow declaration order.
INSERT INTO users VALUES ('ana', 'ana@x.com', 30, NULL, NULL)
```

- The column list is recommended in application code because it remains stable when a schema evolves. If omitted, values map to every declared column in declaration order; the implicit `id` is not included and is still generated automatically.
- Unlisted columns take their `DEFAULT`, or `NULL` when they have none (an error if they are `NOT NULL` without a default).
- Returns `QueryOutput::Inserted { ids }` with the generated ULIDs or the provided ids.
- `INSERT ... SELECT` and `RETURNING` are not supported.

## SELECT

```sql
SELECT * FROM users
SELECT name, age FROM users
SELECT name AS who, age AS years FROM users
```

### WHERE

Operators: `=`, `!=` (or `<>`), `<`, `<=`, `>`, `>=`, `AND`, `OR`, `NOT`, parentheses, `IS NULL`, `IS NOT NULL`, `IN (...)`, `NOT IN (...)`.

```sql
SELECT name FROM users WHERE age >= 25 AND email IS NOT NULL
SELECT name FROM users WHERE age IN (25, 30, 41)
SELECT name FROM users WHERE (age < 18 OR age > 65) AND NOT name = 'admin'
SELECT name FROM users WHERE id = 'u-admin'        -- direct point lookup
```

NULL semantics: standard SQL three-valued logic, matching PostgreSQL and MySQL. A comparison involving `NULL` is `UNKNOWN` (neither true nor false), and a row is kept only when the predicate is `TRUE`.

```sql
WHERE email = NULL       -- never matches; use IS NULL
WHERE NOT age = 30       -- drops rows where age IS NULL: NOT UNKNOWN is UNKNOWN
WHERE age NOT IN (1, NULL)  -- always empty: the NULL might have been the match
WHERE age = 30 OR age IS NULL   -- how to include NULLs deliberately
```

`IS NULL` / `IS NOT NULL` are the only tests that always return a definite answer. `AND`/`OR` follow the usual tables: `FALSE AND UNKNOWN` is `FALSE`, `TRUE OR UNKNOWN` is `TRUE`, and everything else touching `UNKNOWN` stays `UNKNOWN`. `HAVING` applies the same rule, so a group whose aggregate is `NULL` does not pass.

### ORDER BY, LIMIT, OFFSET

```sql
SELECT name, age FROM users ORDER BY age DESC, name ASC LIMIT 10 OFFSET 20
```

`ORDER BY` is memory bounded. Once its working set reaches
`DbOptions::memory.query_working_bytes`, EliteSQL writes sorted temporary runs
and merges them. With `LIMIT`/`OFFSET`, each run keeps only the potentially
useful top-k rows. Temporary runs are deleted on both success and error.

For large unordered result sets, use `Db::query_cursor` so rows are decoded and
returned in batches instead of building one `Vec` containing the whole result:

```rust
use elitesql_core::{Db, DbOptions, MemoryOptions};

let db = Db::open_with("app.esql", DbOptions {
    memory: MemoryOptions {
        total_memory_bytes: 128 * 1024 * 1024,
        query_pool_bytes: 64 * 1024 * 1024,
        query_working_bytes: 16 * 1024 * 1024,
        index_delta_pool_bytes: 24 * 1024 * 1024,
        maintenance_pool_bytes: 32 * 1024 * 1024,
        reserved_memory_bytes: 8 * 1024 * 1024,
        scan_batch_rows: 512,
        spill_directory: None,
    },
    ..DbOptions::default()
})?;

let mut rows = db.query_cursor(
    "SELECT id, name FROM users WHERE active = true LIMIT 10000"
)?;
while let Some(row) = rows.next() {
    consume(row?);
}
```

The cursor currently covers single-table `SELECT` with `WHERE`, projection,
`OFFSET` and `LIMIT`. `Db::query` remains compatible and uses bounded spill for
single-table sorting and high-cardinality aggregation. `query_memory_stats()`
reports cumulative spill files, spill bytes and the largest estimated operator
buffer for the open handle.

`NULL`s sort first. `LIMIT`/`OFFSET` apply after sorting and may be bound using
`?`, `%s` or `%(name)s`; their value must be a non-negative `int64`.

## Memory and resource limits

The three allocatable database-wide pools and the emergency reserve are shared
across all clones of `Db` and all sidecar clients. A query reserves
`query_working_bytes` from the query pool; if no permit is available it waits
with backpressure. Mutable primary, secondary, text and vector state is charged
to the index-delta pool and is published/remapped when the pool fills.
Checkpoints, index construction and compaction serialize through the
maintenance pool. The reserve is never lent to normal work. Inspect
current/peak usage, waits and consolidations with `Db::global_memory_stats()`.

One operation that cannot ever fit—such as an oversized transaction, vector
`top_k`, or vector-index build—returns `Error::MemoryLimit`. Split it into
batches or raise the corresponding pool. `Db::query()` results already handed
to the caller and clean reclaimable mmap pages are outside this accounting;
use `query_cursor()` to avoid owning an unbounded result vector.

For sustained ingestion, more memory helps only when assigned to the relevant
work: `memtable_max_bytes` delays automatic checkpoints,
`index_delta_pool_bytes` permits larger mutable index deltas, and
`maintenance_pool_bytes` gives construction/merge work a larger bounded
workspace. Increasing `query_pool_bytes` alone does not make inserts faster.
The conservative 128 MiB profile remains the default; larger profiles are an
explicit deployment choice, not a requirement for opening or querying an
index.

On a table without derived equality, text or vector indexes, crossing the
memtable threshold freezes the current primary delta and flushes it on a
dedicated worker while later commits use a fresh active delta. Both generations
remain queryable. The frozen generation consumes the maintenance pool rather
than duplicating memory outside the configured envelope; if the new active
delta fills before publication, writers wait with backpressure. `checkpoint()`,
DDL, compaction and database close are barriers. The WAL is cut at a complete
commit boundary and its later tail is copied into the newly published WAL, so
the overlap does not weaken crash recovery. Derived-index workloads currently
use the synchronous checkpoint path.

For sustained transactional ingest on machines where a 256 MiB envelope is
acceptable, use the measured preset:

```rust
let options = DbOptions::ingest_performance();
let db = Db::open_with("app.esql", options)?;
```

The preset retains the 64 MiB query pool, assigns 64 MiB each to mutable index
deltas and maintenance, and raises the memtable target to 64 MiB. The 10M-row
reference workload completed in 18.798 s with this profile versus 22.620 s at
the 128 MiB default; it is intentionally opt-in.

The current isolated 10M-row reference run reached 16/64 MiB in the query
pool, 22.81/24 MiB in the index-delta pool and 32/32 MiB in maintenance. macOS
reported a 65.56 MiB peak physical footprint. Max RSS was higher because clean
mapped pages are counted when touched even though the OS can reclaim them.

An index does not have to fit entirely in heap memory. The primary directory is
a generation-bound set of immutable checksummed paged runs mapped read-only,
plus a bounded recent delta. Primary runs use fanout 16 and copy checksummed V2
pages directly when the promoted key ranges do not overlap. Equality and BM25
use fanout 8 with versioned additions and tombstones, preventing an older value
or posting from reappearing after update/delete. A checkpoint publishes only
each current delta and the background worker performs promotions. Persisted HNSW
vector bases are also searched directly through `mmap`; new vectors remain in a bounded
mutable delta that is frozen into an immutable mapped run under pressure. This
applies equally to 256-, 768- and 1000+-dimensional vectors. EliteSQL does not
require PCA or another dimensional reduction; canonical vectors remain exact,
and int8 quantization is an explicit optional index setting.

Writable recovery recreates missing/stale primary and sorted derived indexes
with bounded external runs. Read-only recovery never modifies the database: it
uses a current mmap index when available and returns `Error::MemoryLimit` if a
required resident fallback cannot fit `index_delta_pool_bytes`.

## Sorted bulk import

For an initial or append-only import with explicit text IDs already in strict
ascending order, use the Rust API directly:

```rust
let inserted = db.bulk_insert_sorted("events", records)?;
```

Every `Record` must contain a non-empty text `id`; IDs must be strictly greater
than both the preceding input ID and any existing ID in the table. The table
must not have equality, BM25 or vector indexes yet—create those after loading.
The loader validates and streams records under the maintenance budget, writes
one canonical segment and one primary run, and publishes the entire batch at
one commit version. Bad ordering or schema data returns an error without
partially publishing the batch. This API is intentionally specialized; normal
transactions remain the correct path for unsorted or indexed writes.

## JOINs

Supported: `INNER JOIN` (or `JOIN`), `LEFT JOIN`, `RIGHT JOIN`. The `ON` condition is exactly one column equality; extra filters go in `WHERE`.

For a single `INNER`/`LEFT JOIN` whose new side has an index on the join key,
EliteSQL streams the left side and performs bounded index probes. A following
`ORDER BY` uses the same spill budget described above. Other equality joins use
a partitioned Grace Hash Join: both sides spill into bounded partitions,
oversized partitions are repartitioned recursively, and a highly skewed key
falls back to bounded block probing. `LEFT`/`RIGHT` match flags live in temporary
files instead of growing with the relation. As with every convenience API, the
final rows returned by `Db::query` remain caller-owned; use a cursor where the
query shape supports it when the result itself is unbounded.

```sql
-- A user's orders, using the orders.user_id index:
SELECT u.name, o.amount
FROM users u
INNER JOIN orders o ON o.user_id = u.id
WHERE u.email = 'ana@x.com'
ORDER BY o.amount DESC

-- LEFT JOIN: users without orders appear with NULL:
SELECT u.name, o.amount FROM users u LEFT JOIN orders o ON o.user_id = u.id

-- Chained joins:
SELECT u.name, o.amount, t.tag
FROM users u
JOIN orders o ON o.user_id = u.id
JOIN tags t   ON t.order_id = o.id
```

- Table aliases with or without `AS` (`users u` or `users AS u`).
- With more than one table, repeated columns (like `id`) must be qualified: `u.id`. `SELECT *` returns qualified headers (`u.id`, `o.amount`).
- `RIGHT JOIN` preserves the right table (internally a LEFT with roles swapped).
- `FULL OUTER JOIN` and `CROSS JOIN` are not supported.

## Aggregates, GROUP BY and HAVING

Functions: `COUNT(*)`, `COUNT(col)`, `SUM(col)`, `AVG(col)`, `MIN(col)`, `MAX(col)`.

```sql
-- Global aggregate: always returns exactly one row.
SELECT count(*), sum(amount), avg(amount) FROM sales

-- Per group, with group filtering and ordering by alias:
SELECT region, count(*) AS n, sum(amount) AS total
FROM sales
WHERE amount > 0
GROUP BY region
HAVING sum(amount) >= 300
ORDER BY total DESC
LIMIT 10

-- Composes with joins:
SELECT g.country, sum(s.amount) AS total
FROM sales s JOIN regions g ON g.name = s.region
GROUP BY g.country
```

NULL semantics (standard SQL):

- `COUNT(*)` counts rows; `COUNT(col)` ignores NULLs.
- `SUM`/`AVG`/`MIN`/`MAX` ignore NULLs; over an empty set they return NULL.
- NULLs group together in GROUP BY.
- An integer `SUM` overflows with an explicit error (no wrapping); mixing `int` and `float64` promotes to `float64`. `AVG` always returns `float64`.

Rules:

- Every non-aggregated column in the SELECT must appear in GROUP BY.
- `SUM`/`AVG` require `int` or `float64` columns.
- HAVING may only reference grouped columns and aggregates (the aggregate does not need to appear in the SELECT).
- In aggregate queries, ORDER BY references output names or aliases (`ORDER BY total DESC`), not function calls: give the aggregate an alias.
- Aggregates live only in SELECT and HAVING (in WHERE, use... HAVING).
- Not supported: `COUNT(DISTINCT ...)`, nested aggregates, expressions inside aggregates.

## UPDATE

```sql
UPDATE users SET age = 31 WHERE id = 'u-admin'
UPDATE users SET email = NULL, age = 0 WHERE age > 100
```

- `SET` accepts literals only (no `SET age = age + 1`; compute in the application).
- Without `WHERE` it affects every row.
- Returns `QueryOutput::Affected(n)`.

## DELETE

```sql
DELETE FROM orders WHERE amount < 100
DELETE FROM orders          -- every row
```

Returns `QueryOutput::Affected(n)`. Live snapshots keep seeing the deleted rows until they are released.

## Vectors: storing and searching embeddings

A `vector(N)` column holds an embedding of exactly N `float32` components. Storing them is SQL; **searching** them is the Rust API or a binding — there is no SQL function for it yet (see [Outside the V1 subset](#outside-the-v1-subset)).

### 1. Declare the column and index it

```sql
CREATE TABLE docs (
  title     text NOT NULL,
  lang      text,
  embedding vector(4)          -- in production: 768, 1024, 1536...
)
```

The column stores vectors on its own. To *search* them you need an ANN (HNSW) index, created through the API:

```rust
use elitesql_core::{Db, VectorIndexOptions};

let db = Db::open_or_create("app.esql")?;
db.create_vector_index("docs", "embedding", VectorIndexOptions::default())?;
```

It is built from the records already stored, so you may create it before or after loading data.

### 2. Insert a vector

In SQL the literal is a **string containing a JSON array** (the same shape as the type table above):

```sql
INSERT INTO docs (title, lang, embedding)
VALUES ('hello', 'en', '[0.1, 0.2, 0.3, 0.4]')

-- Multi-row, one atomic commit:
INSERT INTO docs (title, lang, embedding) VALUES
  ('hola',    'es', '[0.11, 0.19, 0.31, 0.39]'),
  ('bonjour', 'fr', '[-0.9, 0.05, 0.2, 0.1]')
```

The dimension is part of the type and is validated on every write:

```
INSERT INTO docs (title, embedding) VALUES ('bad', '[1.0, 2.0]')
  → schema violation: column 'embedding' expects vector<float32, 4>, got dimension 2

INSERT INTO docs (title, embedding) VALUES ('bad', 'nope')
  → sql error: invalid vector literal for 'embedding' (expected a JSON array of numbers)
```

In real code the embedding comes from a model as a `Vec<f32>`, so you insert it through the API and skip the string round-trip:

```rust
use elitesql_core::{Record, Value};

let embedding: Vec<f32> = model.embed("guten tag");   // your embedding model
let mut record = Record::new();
record.insert("title".into(), Value::Text("guten tag".into()));
record.insert("lang".into(), Value::Text("de".into()));
record.insert("embedding".into(), Value::Vector(embedding));
let id = db.insert("docs", record)?;
```

### 3. Search

```rust
use elitesql_core::VectorSearchOptions;

let query: Vec<f32> = model.embed("hi");
let hits = db.search_vector("docs", "embedding", &query, 3, &VectorSearchOptions::default())?;
for hit in &hits {
    println!("{:.4}  {:?}", hit.distance, hit.record["title"]);
}
```

Each hit carries `id`, `distance` and the full `record`, closest first:

```
0.0000  Text("hello")
0.0006  Text("guten tag")
0.0007  Text("hola")
```

**Lower distance = closer.** With `Cosine` (the default) the distance is `1 - cosine_similarity`: `0` is identical, `1` orthogonal, `2` opposite. `Dot` is `1 - dot_product` (use it with normalized vectors), and `L2` is the plain Euclidean distance.

Deleted records never appear in the results, and the search only sees committed data.

### 4. Filter by metadata

`filter` applies equality constraints on the record's other columns — the ANN search over-fetches until enough hits pass it, so you still get up to `top_k` results:

```rust
let mut filter = Record::new();
filter.insert("lang".into(), Value::Text("es".into()));

let hits = db.search_vector(
    "docs",
    "embedding",
    &query,
    5,
    &VectorSearchOptions {
        ef_search: Some(128),   // wider beam: better recall, slower
        filter: Some(filter),
    },
)?;
```

`ef_search` defaults to `max(64, 2 * top_k)`. Raise it when recall matters more than latency.

### 5. Index options

```rust
use elitesql_core::{IndexingMode, VectorIndexOptions, VectorMetric};

db.create_vector_index(
    "chunks",
    "embedding",
    VectorIndexOptions {
        metric: VectorMetric::Cosine,   // Cosine | Dot | L2
        mode: IndexingMode::Async,      // Sync (default) | Async
        m: 24,                          // HNSW connections per node (12–48)
        ef_construction: 200,           // build beam width (100–400)
        quantized: true,                // int8 in-index vectors: ~4x less memory
    },
)?;
```

- `Sync` makes a vector searchable as soon as the commit returns. `Async` lets the commit return before the vector enters the graph — faster bulk loading, at the cost of a short window where it is not yet searchable. Call `db.wait_vector_indexing()` to close that window (after a bulk load, before searching).
- `metric` is fixed when the index is created: to change it, drop the index and create it again.
- The graph is a **derived** structure: it is persisted for fast opens, but always rebuildable from the records — it is rebuilt on compaction, and after a crash if its dump is unusable. Losing it never loses data.
- `db.drop_vector_index("docs", "embedding")` removes the index and its persisted graph; the column and its vectors stay.

### From the bindings

Same operations, same names. Python:

```python
import elitesql

db = elitesql.EliteSQL("app.esql")
db.query("CREATE TABLE docs (title text NOT NULL, lang text, embedding vector(4))")
db.create_vector_index("docs", "embedding", metric="cosine")
db.query("INSERT INTO docs (title, lang, embedding) VALUES ('hello', 'en', '[0.1, 0.2, 0.3, 0.4]')")

for hit in db.search_vector("docs", "embedding", [0.1, 0.2, 0.3, 0.4], top_k=5):
    print(round(hit["distance"], 4), hit["record"]["title"])
# 0.0 hello
```

`search_vector(table, column, vector, top_k=10, ef_search=None, filter=None)` returns a list of `{"id", "distance", "record"}`; vectors come back as lists of floats. The Node binding and the `elitesql serve` sidecar expose the same `search_vector` / `create_vector_index` / `search_text` / `search_hybrid` operations.

### What SQL can and cannot do with a vector

```sql
SELECT title, embedding FROM docs WHERE title = 'hello'   -- ✅ returns the vector
UPDATE docs SET embedding = '[0.5, 0.5, 0.5, 0.5]' WHERE title = 'hello'  -- ✅
SELECT * FROM docs WHERE embedding = '[0.1, 0.2, 0.3, 0.4]'  -- ❌ vectors are not comparable
SELECT * FROM docs ORDER BY distance(embedding, '[...]')     -- ❌ no such function yet
```

Comparing or ordering by a vector fails with an explicit error. Nearest-neighbour ranking is `search_vector`; combining it with BM25 text ranking is `search_hybrid` (Reciprocal Rank Fusion). Both live in the API for now, and a SQL surface for them is a later phase.

## How the planner decides

Heuristic, no cost model:

1. `WHERE id = '...'` → direct point lookup.
2. Equality on an indexed column → index lookup.
3. Any other filter → full scan + filter.
4. Single-table filters are pushed **below** the join.
5. Joins: when the new side has a useful equality index (including `id`), bounded index probes; otherwise a partitioned Grace Hash Join using the query spill budget.

Practical rule: **index your join columns** (`orders.user_id`) and your frequent lookup columns.

## EXPLAIN

`EXPLAIN <select>` prints the plan and does not run the query:

```sql
EXPLAIN SELECT u.name, o.total FROM users u
JOIN orders o ON o.user_id = u.id
WHERE u.age > 30 AND o.total > 100;
```

```
JOIN INNER (index nested-loop)
  on: u.id = o.user_id
  streamed: no joined rows are materialized
  SCAN u
    filter: u.age > 30
  INDEX PROBE o.user_id = u.id
    filter: o.total > 100
```

Because planning is static and carries no estimates, the plan is not a prediction: the executor reads its access path and join strategy from the same functions EXPLAIN does. There are no row-count guesses to be wrong, and no `EXPLAIN ANALYZE` to contrast them with.

The line that matters most is the access path, because the gap between them is five orders of magnitude (see [Reference performance](#reference-performance)):

| Line | Meaning |
|---|---|
| `POINT LOOKUP t.id = '...'` | direct fetch by primary key |
| `INDEX LOOKUP t.col = v` | secondary index on `col` |
| `INDEX PROBE t.col = u.other` | per-row index probe, the inner side of an index nested-loop join |
| `SCAN t` | full scan |
| `SCAN t (equality col = v, no index)` | equality with **no index**: still a full scan — the one to go index |
| `NO ACCESS t` | the predicate cannot match (`col = NULL`), so nothing is read |

Operators above the access path — `LIMIT`, `SORT`, `GROUP BY`, `JOIN`, `filter:` — are listed outermost first, with their inputs indented underneath. `filter:` at the top level is a predicate spanning two tables, so it can only be evaluated after the join; `filter:` under an access path is pushed down to that table.

`EXPLAIN` accepts only `SELECT`, and it validates the query exactly as execution does: an unknown column or a missing `GROUP BY` fails instead of printing a plan.

Reading a plan you did not expect, the usual fixes are `CREATE INDEX` on the column in a `SCAN ... no index` line, and on the join column behind a `grace hash join`.

## Outside the V1 subset

All of this fails with a clear error, never with surprise behavior:

| Not supported | Alternative |
|---|---|
| `COUNT(DISTINCT ...)`, nested aggregates | deduplicate/compute in the application |
| Subqueries, CTEs (`WITH`), `UNION` | rewrite as separate queries |
| `FULL OUTER JOIN`, `CROSS JOIN` | two queries + merge in the app |
| Arithmetic (`age + 1`) and functions | compute in the application |
| `LIKE`, `BETWEEN` | `BETWEEN` → `>= AND <=`; text search: `db.create_text_index` + `db.search_text` (BM25, Rust API/bindings) |
| `DISTINCT` | deduplicate in the app |
| `ALTER COLUMN` / `MODIFY` (type or nullability changes) | add the new column, copy with `UPDATE`, drop the old one |
| `DROP TABLE ... CASCADE` / `RESTRICT` | there are no foreign keys in V1, so there is nothing to cascade |
| `BEGIN/COMMIT` in SQL | transactions via the Rust API: `db.begin()` |
| `RETURNING` | INSERT already reports the ids |
| `ON DUPLICATE KEY UPDATE`, `REPLACE INTO` | SELECT first, then INSERT or UPDATE inside a transaction |
| `TRUNCATE TABLE` | `DELETE FROM table`, or `DROP TABLE` and recreate it |
| `LIMIT offset, count` (MySQL order) | `LIMIT count OFFSET offset` — the operands swap |
| `FOR UPDATE` / `LOCK IN SHARE MODE` | commits are optimistic; retry on `Error::Conflict` instead of locking rows |
| Vector/text/hybrid search in SQL (`distance(...)`, `ORDER BY` a vector) | Rust API and bindings: `search_vector` (see [Vectors](#vectors-storing-and-searching-embeddings)), `search_text` (BM25) and `search_hybrid` (RRF); a SQL surface is left for a future phase |

## Reference performance

Over 1M orders + 10K users (Apple Silicon, `cargo bench --bench sql`):

| Query | Latency |
|---|---|
| Point lookup via unique index (full SQL path: parse + plan + exec) | ~4 µs |
| Indexed JOIN: a user → their ~100 orders out of 1M, ORDER BY + LIMIT | ~231 µs |
| Unindexed full scan with a filter over 1M rows | ~264 ms |

An index remains the fastest plan for a frequent join. Without one, execution
is memory-bounded but pays partitioning and temporary-I/O cost. These are
query-only Criterion intervals. In the current 10M-row scale harness,
transactional EliteSQL takes 22.620 s at the 128 MiB default versus SQLite's
13.663 s; the 256 MiB ingest profile takes 18.798 s, and sorted bulk EliteSQL
takes 9.968 s versus 13.822 s. Point reads and the measured unindexed equality
scan favor EliteSQL. Run-promotion drain, memory evidence, raw runs and
concurrent-writer tails are documented in [benchmark.md](benchmark.md).
