# From MySQL to EliteSQL

A migration guide for people who know MySQL. It covers what changes, what breaks
silently, and what to rewrite — with the actual errors EliteSQL returns.

Full dialect reference: [manual.md](manual.md). Deployment and API:
[docs/getting-started.md](docs/getting-started.md).

## The shift in one paragraph

MySQL is a **server** you connect to, with a large SQL surface, pessimistic
locking and a schema layer that can express constraints (foreign keys, checks,
enums, exact decimals). EliteSQL is an **embedded engine**: a self-contained
directory your process opens, with a deliberately small SQL subset, optimistic
MVCC commits, and a schema layer that expresses types and uniqueness only.
Everything MySQL does that EliteSQL does not is not hidden — it is rejected at
parse time with a message that names the alternative. The migration work is
therefore mostly mechanical, and concentrated in three places: **money and
enums** (types), **transaction retries** (concurrency), and **whatever your ORM
generates** (SQL surface).

Before starting, be honest about fit. EliteSQL is the right target for an
operational app's working set: records, filters, joins on indexed keys,
aggregates, JSON, blobs, plus vector/text search. It is the wrong target if you
depend on foreign keys, stored procedures, triggers, window functions, or
analytical SQL with subqueries and CTEs.

## 1. Deployment: there is no server

| MySQL | EliteSQL |
|---|---|
| `mysqld` daemon, port 3306 | no daemon; the engine is a library in your process |
| connection string, user, password | a directory path: `Db::open_or_create("app.esql")` |
| `GRANT` / users / roles | filesystem permissions on the directory |
| connection pool | one `Db` handle, cloned and shared across threads |
| `mysqldump` | `elitesql backup` (snapshot-consistent + verified) or `elitesql export` (JSON lines) |
| replication | not available |

The consequence that surprises people most: **one process owns the database**.
Threads inside that process get real concurrency (readers never block writers;
writers meet only at commit), but a second process opening the same directory
read-write is not the deployment model.

For multi-worker setups (gunicorn, PHP-FPM, several services), run the sidecar:
one process owns the engine and workers speak a line-delimited JSON protocol
over a Unix socket.

```bash
elitesql serve app.esql /tmp/elitesql.sock
```

```python
from elitesql import SidecarClient
with SidecarClient("/tmp/elitesql.sock") as db:
    db.query("SELECT name FROM users WHERE email = %s", ["ana@x.com"])
```

If the app genuinely runs on a **different host** — the shape MySQL made normal
— the same sidecar listens on TCP, with a required token:

```bash
export ELITESQL_TOKEN=$(openssl rand -hex 32)
elitesql serve app.esql --tcp 127.0.0.1:7070
```

```python
db = SidecarClient(host="db-host", port=7070, token=os.environ["ELITESQL_TOKEN"])
```

Two differences from a MySQL connection worth pricing in. There is **no TLS**,
so put it behind an SSH tunnel or a VPN rather than exposing the port. And the
latency budget inverts: a point lookup is ~4 µs against ~0.5 ms of round trip,
so unlike MySQL — where the network was always in the path and query cost
dominated — here the network becomes the cost. Queries that were "free" locally
are no longer free, and chatty ORM patterns (N+1) hurt far more than they did.

Read-only consumers (analytics, exports) can open the directory with
`--read-only` without touching disk. What you must not do is put the database
directory on NFS/SMB and mount it from several machines: durability relies on
`fsync` plus atomic `rename` and index bases are read through `mmap`, neither of
which network filesystems provide reliably.

## 2. Schema: what your `CREATE TABLE` becomes

### Types

| MySQL | EliteSQL | Notes |
|---|---|---|
| `TINYINT(1)` / `BOOLEAN` | `bool` | a real boolean, not 0/1 |
| `INT`, `BIGINT`, `SMALLINT` | `int` | always signed 64-bit; `integer`/`bigint`/`int64` are aliases. There is no narrower integer |
| `DECIMAL(10,2)`, `NUMERIC` | **none** | see *Money* below — this is the one type change that can corrupt values silently |
| `FLOAT`, `DOUBLE` | `float64` | |
| `VARCHAR(n)`, `CHAR(n)`, `TEXT` | `text` | no length limit, no charset, no collation |
| `BLOB`, `VARBINARY` | `blob` | literal is hex: `X'DEADBEEF'` |
| `DATETIME`, `TIMESTAMP` | `timestamp` | UTC microseconds; no timezone offsets |
| `DATE` | `date` | |
| `TIME` | `time` | |
| `JSON` | `json` | |
| `ENUM('a','b')` | `text` | validate in the application, or use a lookup table |
| `SET(...)` | `json` | |
| `UUID`/`CHAR(36)` PK | the implicit `id` | see *Primary keys* |
| — | `vector(N)` | new: ANN search as a first-class type |

Unknown types fail loudly rather than being coerced:

```
CREATE TABLE u (s varchar(50))
  -> unknown type 'varchar': use text
CREATE TABLE u (price decimal(10,2))
  -> unknown type 'decimal': V1 types are bool, int (int64), float64, text,
     blob, timestamp, date, time, json, vector(N)
```

### Money — read this before migrating a billing table

There is no exact decimal type. Do **not** map `DECIMAL(10,2)` to `float64`:
binary floating point cannot represent `0.10` exactly, and sums drift.

Store minor units in `int`:

```sql
-- MySQL:    price DECIMAL(10,2)   -- 19.99
-- EliteSQL: price_cents int       -- 1999
```

Format and divide in the application. Keep the column name honest
(`price_cents`, `amount_micros`) so nobody later reads it as euros. If you must
keep a decimal string for display, store `text` alongside the integer — but make
the integer the source of truth for arithmetic and comparisons.

### Primary keys and AUTO_INCREMENT

Every table has an implicit `id text` column you never declare. Omit it on
INSERT and the engine generates a ULID (26 chars, time-sortable); supply it and
it is used as given. It cannot be changed by UPDATE.

```sql
-- MySQL
CREATE TABLE users (
  id INT AUTO_INCREMENT PRIMARY KEY,
  email VARCHAR(255) NOT NULL UNIQUE,
  created_at DATETIME NOT NULL DEFAULT CURRENT_TIMESTAMP
);

-- EliteSQL
CREATE TABLE users (
  email      text NOT NULL,
  created_at timestamp NOT NULL          -- the app supplies the value
);
CREATE UNIQUE INDEX ON users (email);
```

What this means in practice:

- **Numeric ids become text.** If other systems, URLs or exported reports depend
  on integer ids, keep the old value in its own column (`legacy_id int`) and
  index it. Do not try to make `id` numeric — it is text by construction.
- **ULIDs are time-sortable**, so `ORDER BY id` approximates insertion order,
  which covers many uses of `ORDER BY id` on an auto-increment key.
- **There is no `LAST_INSERT_ID()`**. INSERT returns the ids directly:
  `QueryOutput::Inserted { ids }` in Rust, `{"inserted": [...]}` from `query()`
  in Python and the sidecar.
- `PRIMARY KEY`, `REFERENCES` and inline `UNIQUE` are not accepted in
  `CREATE TABLE`; uniqueness is `CREATE UNIQUE INDEX`.

### Constraints that do not exist

| MySQL | What to do |
|---|---|
| `FOREIGN KEY ... ON DELETE CASCADE` | enforce in the application; delete children explicitly, ideally in one `Txn` |
| `CHECK (...)` | validate in the application |
| `ENUM` | `text` + application validation |
| `NOT NULL` | supported |
| `UNIQUE` | `CREATE UNIQUE INDEX` — validated at every commit, NULLs excluded |
| `DEFAULT CURRENT_TIMESTAMP` | not available: `DEFAULT` takes a literal only, so the app supplies timestamps |

### Indexes

- **One column per index.** There are no composite indexes in V1, so a MySQL
  `INDEX (tenant_id, created_at)` becomes an index on the more selective column
  plus a filter on the rest. Check the result with `EXPLAIN` (section 6).
- No prefix indexes, no `FULLTEXT` in SQL (see section 8), no descending
  indexes, no hash/BTREE choice.
- Index names are accepted and ignored; an index is identified by table +
  column, and dropped the same way: `DROP INDEX ON users (email)`.

### Identifiers are case-sensitive

MySQL folds column names case-insensitively (and table names too, on macOS and
Windows). EliteSQL does not:

```
SELECT NAME FROM USERS   -> table not found: USERS
SELECT NAME FROM users   -> unknown column 'NAME'
```

Keywords remain case-insensitive (`SELECT` = `select`). If your codebase mixes
`Users` and `users`, normalize before migrating — this one produces runtime
errors rather than wrong results, which is the good failure mode, but it will
find every inconsistency you have.

## 3. Queries: what to rewrite

Everything in this table fails at parse time, not at runtime:

| MySQL | EliteSQL | Rewrite |
|---|---|---|
| `LIKE 'a%'` | `LIKE is not supported in V1` | BM25 text search (section 8), or filter in the app |
| `BETWEEN 1 AND 5` | `BETWEEN is not supported in V1; use >= AND <=` | `n >= 1 AND n <= 5` |
| `DISTINCT` | `DISTINCT is not supported in V1` | deduplicate in the app |
| `COUNT(DISTINCT x)` | `DISTINCT inside aggregates is not supported in V1` | collect and count in the app |
| subqueries | `subqueries are not supported in V1` | run two queries |
| `WITH` / CTEs | `CTEs (WITH) are not supported in V1` | run two queries |
| `UNION` | `UNION is not supported in V1` | concatenate in the app |
| `INSERT ... SELECT` | `INSERT ... SELECT is not supported in V1` | SELECT, then batch INSERT |
| `NOW()`, `DATE()`, `CONCAT()`, `GROUP_CONCAT()` | `functions are not supported in V1 (aggregates: COUNT, SUM, AVG, MIN, MAX)` | compute in the app |
| `age + 1` in a projection | arithmetic is not supported | compute in the app |
| `INDEX (a, b)` | `multi-column indexes are not supported in V1` | index the more selective column |
| `CROSS JOIN` | `CROSS JOIN is not supported in V1` | two queries + merge |
| `FULL OUTER JOIN` | not supported | two queries + merge |
| `RETURNING` | `RETURNING is not supported in V1` | INSERT already returns the ids |
| `ON DUPLICATE KEY UPDATE` | `ON DUPLICATE KEY UPDATE is not supported in V1` | SELECT then INSERT/UPDATE inside a `Txn` |
| `REPLACE INTO` | `REPLACE INTO is not supported` | same |
| `TRUNCATE TABLE` | `TRUNCATE is not supported` | `DELETE FROM table`, or drop and recreate |
| `LIMIT 10, 20` | `LIMIT offset, count is not supported` | `LIMIT 20 OFFSET 10` — **the operands swap** |
| `FOR UPDATE`, `LOCK IN SHARE MODE` | `row locking ... is not supported` | optimistic commit + retry (section 4) |

What does carry over unchanged: `SELECT`/`INSERT`/`UPDATE`/`DELETE`, `WHERE`
with `= != <> < <= > >= AND OR NOT` and parentheses, `IS NULL` / `IS NOT NULL`,
`IN (...)` / `NOT IN (...)`, `INNER`/`LEFT`/`RIGHT JOIN` on a single column
equality, `GROUP BY`/`HAVING`, `COUNT`/`SUM`/`AVG`/`MIN`/`MAX`,
`ORDER BY`/`LIMIT`/`OFFSET`, `AS` aliases, and `--` / `/* */` comments.

### Parameters already match

If you use `mysqlclient`, `PyMySQL` or anything DB-API, your placeholder style
works as-is — `%s` and `%(name)s` are supported, as is `?`:

```python
db.query("SELECT * FROM users WHERE email = %s LIMIT %s", [email, 10])
db.query("SELECT * FROM users WHERE email = %(email)s", {"email": email})
```

Binding happens after parsing and never rewrites the SQL string, so a parameter
like `"x' OR TRUE --"` is a text value and nothing else. Counts and names must
match exactly; a mismatch is an error rather than a silent `NULL`.

## 4. Transactions and concurrency: the real semantic change

This is where a mechanical port produces bugs.

InnoDB is **pessimistic**: `BEGIN`, then rows are locked as you touch them, and
`SELECT ... FOR UPDATE` reserves them. EliteSQL is **optimistic MVCC**: a
transaction stages its writes and validates at commit. If another transaction
touched the same records first, the commit returns `Error::Conflict` and **your
code must retry**.

```rust
// There is no BEGIN/COMMIT in SQL:
//   BEGIN -> SQL transactions are not supported in V1; use the Txn API
let mut txn = db.begin();
txn.insert("orders", order)?;
txn.update("stock", &sku, updated)?;
match txn.commit() {
    Ok(_) => {}
    Err(elitesql_core::Error::Conflict(_)) => { /* rebuild and retry */ }
    Err(e) => return Err(e.into()),
}
```

Practical rules for the port:

- **Every multi-statement transaction needs a retry loop.** Write it once as a
  helper and route all transactional work through it. Under contention, a
  missing retry shows up as intermittent failures under load, not in tests.
- **Retries must be idempotent.** Recompute the new values from a fresh read
  inside the retry rather than reusing values read before the failed attempt.
- **Single statements are already atomic.** `UPDATE`/`DELETE` run in a
  transaction with automatic retry, and a multi-row `INSERT` is one commit — all
  rows land or none do. Only your own multi-step `Txn` needs handling.
- **`SELECT` is read-committed**, matching MySQL's `READ COMMITTED` rather than
  InnoDB's default `REPEATABLE READ`. For a stable multi-query view, take a
  snapshot: `db.snapshot()` + `scan_at`/`get_at`.
- **No deadlocks** — there are no lock waits to deadlock on. Conflicts fail fast
  instead.

Schema changes are the exception to MVCC: DDL is not versioned and applies to
everyone at once, including in-flight transactions (whose commits then fail).
Run migrations when nothing depends on the old shape, exactly as you would in
MySQL.

## 5. NULL

EliteSQL uses standard three-valued logic, the same as MySQL: a comparison with
`NULL` yields `UNKNOWN`, `NOT UNKNOWN` is still `UNKNOWN`, and a row is kept only
when the predicate is `TRUE`. These all behave identically in both engines:

```sql
SELECT name FROM t WHERE name = NULL      -- 0 rows; use IS NULL
SELECT n    FROM t WHERE n <> 1           -- NULL rows dropped
SELECT name FROM t WHERE NOT name = 'a'   -- NULL rows dropped
SELECT n    FROM t WHERE n NOT IN (1, NULL)  -- always empty
SELECT n    FROM t WHERE n = 1 OR n IS NULL  -- how to include NULLs
```

`HAVING` follows the same rule, so a group whose aggregate is `NULL` does not
pass. Ordering matches too: `NULL`s sort first with `ASC` and last with `DESC`,
exactly as in MySQL.

Nothing to port in this section — it is here because NULL handling is where
engines usually diverge, and this one does not.

## 6. EXPLAIN

`EXPLAIN SELECT ...` exists and is familiar in spirit, but the output is a plain
indented tree rather than MySQL's row-per-table table, and it carries **no
estimates** — planning is static, so the plan is a statement of what will run,
not a prediction. There is no `EXPLAIN ANALYZE`, because there are no estimated
row counts to contrast with real ones.

```
EXPLAIN SELECT u.name, o.total FROM users u
JOIN orders o ON o.user_id = u.id WHERE u.age > 30;

JOIN INNER (index nested-loop)
  on: u.id = o.user_id
  streamed: no joined rows are materialized
  SCAN u
    filter: u.age > 30
  INDEX PROBE o.user_id = u.id
```

Reading it, in MySQL terms:

| EliteSQL | MySQL `type` column |
|---|---|
| `POINT LOOKUP t.id = '...'` | `const` / `eq_ref` on the primary key |
| `INDEX LOOKUP t.col = v` | `ref` |
| `INDEX PROBE t.col = u.other` | `ref` as the inner side of a nested loop |
| `SCAN t` | `ALL` |
| `SCAN t (equality col = v, no index)` | `ALL` — **the line to act on**: an equality with no index |
| `NO ACCESS t` | `Impossible WHERE` |

Since composite indexes do not exist, `EXPLAIN` is the fastest way to check that
a query that used to ride a multi-column index still finds a useful single-column
one. Full details in [manual.md](manual.md#explain).

## 7. Dates and times

`timestamp` is an instant in UTC microseconds. There are no timezone offsets in
literals (`+05:00` is not accepted) and no session timezone: the engine stores
instants, and presentation belongs to the application.

- No `NOW()`, `CURDATE()`, `DATE_ADD()`, `DATEDIFF()`. The app computes the
  values and passes them as parameters.
- No `DEFAULT CURRENT_TIMESTAMP`; set `created_at` explicitly on INSERT.
- Date arithmetic is trivial outside SQL because of the representation: `date`
  is days since epoch (subtract for a day count), `time` and `timestamp` are
  microseconds.
- Ranges work and use indexes, so "last 30 days" ports directly once the bound
  is computed by the app:

```sql
SELECT * FROM events WHERE day >= '2026-07-08' AND day < '2026-08-08'
```

Strings coerce automatically against date/time/timestamp columns, and invalid
literals (`'2026-02-30'`) are rejected rather than zeroed — unlike MySQL's
historical `'0000-00-00'` behavior.

## 8. Full-text search

`FULLTEXT` indexes and `MATCH ... AGAINST` do not exist in SQL. The replacement
is BM25 through the API, which is generally better ranked than MySQL's natural
language mode:

```python
db.create_text_index("docs", "body")
hits = db.search_text("docs", "body", "quarterly report", top_k=10)
```

`LIKE '%term%'` scans in MySQL too, so if you were using it for search, BM25 is
an upgrade. If you were using `LIKE 'prefix%'` as a cheap index probe, that is
the case with no direct replacement — restructure the data (store the prefix as
its own indexed column) or filter in the app.

Vector (`search_vector`) and hybrid (`search_hybrid`, reciprocal rank fusion)
search are available the same way, and have no MySQL equivalent at all.

## 9. Operations

| MySQL | EliteSQL |
|---|---|
| `mysqldump` | `elitesql backup app.esql dst` — snapshot-consistent, then verified |
| restore a dump | `elitesql restore backup dst` — validates before materializing |
| `SELECT ... INTO OUTFILE` | `elitesql export app.esql table` (JSON lines) |
| `LOAD DATA INFILE` | `elitesql import app.esql table`, or `bulk_insert_sorted` for a sorted initial load |
| `OPTIMIZE TABLE` | automatic compaction; `elitesql compact` to force it |
| `CHECK TABLE` | `elitesql check` (offline integrity check) |
| `innodb_flush_log_at_trx_commit` | `--durability safe\|balanced\|fast` |
| `SHOW TABLES` / `DESCRIBE` | `elitesql tables` |
| crash recovery | automatic on open: checksummed WAL, atomic manifest with fallback, idempotent replay |

`--durability safe` is the default and the closest analogue to
`innodb_flush_log_at_trx_commit=1`. Do not lower it to make an import faster —
use `bulk_insert_sorted` or larger batches instead.

## 10. A migration recipe

1. **Inventory the schema.** For each table, list the columns MySQL types that
   have no direct target: `DECIMAL`, `ENUM`, `SET`, foreign keys, composite
   indexes, `AUTO_INCREMENT` ids consumed outside the database. Decide each one
   before writing any code; these are the only genuinely irreversible choices.
2. **Write the new DDL.** Drop `PRIMARY KEY`/`REFERENCES`/inline `UNIQUE`,
   convert money to integer minor units, turn each `UNIQUE` into
   `CREATE UNIQUE INDEX`, and keep one index per column you filter or join on.
3. **Export from MySQL** as JSON lines, converting values as you go: decimals to
   integer cents, enums to text, datetimes to `'YYYY-MM-DD HH:MM:SS'` in UTC,
   and the old primary key into `legacy_id` if anything outside the database
   references it.
4. **Load.** `elitesql import app.esql table` reads JSON lines from stdin, so
   `mysql -e '...' | convert.py | elitesql import app.esql users` works as a
   pipeline. Create the indexes *after* loading. For a large append-only table
   with explicit sorted ids, `bulk_insert_sorted` is much faster and must run
   before any index exists.
5. **Port the queries.** Work through section 3. Because unsupported syntax
   fails at parse time, a test run over your query set finds them all quickly —
   there is no silently-different behavior to discover in production.
6. **Port the transactions.** Add the retry loop (section 4) and audit every
   place that relied on `SELECT ... FOR UPDATE` or on read-your-writes inside a
   long transaction.
7. **Run `EXPLAIN`** on your top queries and confirm none of them says
   `SCAN ... no index` or falls back to `grace hash join` on a hot path. This is
   the step that catches what the lack of composite indexes cost you.
8. **Choose the deployment shape**: single process, or the sidecar for multiple
   workers (section 1).

Note what is *not* on this list: NULL handling and predicate semantics need no
audit, because they match MySQL (section 5). The changes that alter results
silently are the type conversions in step 1 — money above all.

## Checklist

- [ ] No `DECIMAL` mapped to `float64`; money is integer minor units
- [ ] Every `UNIQUE` became a `CREATE UNIQUE INDEX`
- [ ] Foreign-key cascades reimplemented in application code
- [ ] Composite indexes reconsidered, and verified with `EXPLAIN`
- [ ] Identifier casing normalized (case-sensitive now)
- [ ] Every multi-step transaction wrapped in a conflict-retry loop
- [ ] `SELECT ... FOR UPDATE` call sites redesigned
- [ ] `NOW()`/`CURRENT_TIMESTAMP` moved into the application
- [ ] `LIKE` search replaced with BM25, or explicitly accepted as an app-side filter
- [ ] `LIMIT a, b` rewritten as `LIMIT b OFFSET a`
- [ ] Backup/restore switched to `elitesql backup`/`restore`
- [ ] Deployment shape decided: single process or sidecar
