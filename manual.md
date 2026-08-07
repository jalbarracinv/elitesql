# EliteSQL SQL Manual

Reference for the EliteSQL SQL dialect (V1). It is a deliberately small subset: it covers what an operational app needs and rejects everything else with an explicit error that says what to use instead.

## Running SQL

```rust
use elitesql_core::{Db, QueryOutput};

let db = Db::open_or_create("app.esql")?;
match db.query("SELECT name FROM users WHERE age > 30")? {
    QueryOutput::Rows { columns, rows } => { /* SELECT */ }
    QueryOutput::Inserted { ids }        => { /* INSERT: generated ids */ }
    QueryOutput::Affected(n)             => { /* UPDATE/DELETE: affected rows */ }
    QueryOutput::None                    => { /* DDL */ }
}
```

General rules:

- One statement per `query()` call.
- Case-insensitive keywords (`SELECT` = `select`).
- Line comments with `--`.
- SELECT reads the latest committed state (read committed). For snapshot-consistent reads use the Rust API (`db.snapshot()` + `scan_at`/`get_at`).
- UPDATE/DELETE run inside a transaction with automatic retries on optimistic conflict. A multi-row INSERT is a single atomic commit: all rows land or none do.

## Types and literals

| Type | SQL literal | Example |
|---|---|---|
| `bool` | `TRUE` / `FALSE` | `TRUE` |
| `int64` | integer | `42`, `-7` |
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
  age       int64,
  prefs     json,
  embedding vector(768)
)
```

- Columns are nullable by default; `NOT NULL` to require a value.
- There is no `PRIMARY KEY` (it's the implicit `id`), no `DEFAULT`, no `REFERENCES`, no inline `UNIQUE` (use `CREATE UNIQUE INDEX`).
- Type names are exact: `int` or `varchar` fail with an error pointing to the right type (`use int64`, `use text`).

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

## INSERT

```sql
INSERT INTO users (name, email, age) VALUES ('ana', 'ana@x.com', 30)

-- Multi-row: one atomic commit. Returns the ids in order.
INSERT INTO users (name, age) VALUES ('bob', 25), ('eva', 41)

-- With an explicit id:
INSERT INTO users (id, name) VALUES ('u-admin', 'root')
```

- The column list is **required**.
- Unlisted columns become `NULL` (an error if they are `NOT NULL`).
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

NULL semantics (simplified two-valued logic): any comparison involving `NULL` is false. `email = NULL` never matches — use `email IS NULL`.

### ORDER BY, LIMIT, OFFSET

```sql
SELECT name, age FROM users ORDER BY age DESC, name ASC LIMIT 10 OFFSET 20
```

`NULL`s sort first. `LIMIT`/`OFFSET` apply after sorting.

## JOINs

Supported: `INNER JOIN` (or `JOIN`), `LEFT JOIN`, `RIGHT JOIN`. The `ON` condition is exactly one column equality; extra filters go in `WHERE`.

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
- An int64 `SUM` overflows with an explicit error (no wrapping); mixing int64 and float64 promotes to float64. `AVG` always returns float64.

Rules:

- Every non-aggregated column in the SELECT must appear in GROUP BY.
- `SUM`/`AVG` require int64 or float64 columns.
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

## How the planner decides

Heuristic, no cost model:

1. `WHERE id = '...'` → direct point lookup.
2. Equality on an indexed column → index lookup.
3. Any other filter → full scan + filter.
4. Single-table filters are pushed **below** the join.
5. Joins: when the probe side is small (≤1024 rows) and the join column is indexed (or is `id`), per-row index lookups (index nested-loop); otherwise a hash join.

Practical rule: **index your join columns** (`orders.user_id`) and your frequent lookup columns.

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
| `DROP`, `ALTER` | pending on the roadmap |
| `BEGIN/COMMIT` in SQL | transactions via the Rust API: `db.begin()` |
| `RETURNING` | INSERT already reports the ids |
| Vector/text/hybrid search in SQL | Rust API and bindings: `search_vector`, `search_text` (BM25) and `search_hybrid` (RRF); an explicit SQL function is left for a future phase |

## Reference performance

Over 1M orders + 10K users (Apple Silicon, `cargo bench --bench sql`):

| Query | Latency |
|---|---|
| Point lookup via unique index (full SQL path: parse + plan + exec) | ~4 µs |
| Indexed JOIN: a user → their ~100 orders out of 1M, ORDER BY + LIMIT | ~360 µs |
| Unindexed full scan with a filter over 1M rows | ~1.1 s |

The last row is the spec's known limit ("large joins without an index will be expensive"): if a query is frequent, index it.
