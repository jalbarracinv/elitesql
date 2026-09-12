# elitesql (Python)

Python binding for EliteSQL.

```bash
pip install elitesql
```

Wheels for Linux x86_64/aarch64 (manylinux_2_28) and macOS arm64 ship the
engine (`libelitesql`) inside the package; Python 3.9 or newer. Until the first
PyPI release, install the wheel attached to a
[GitHub Release](https://github.com/jalbarracinv/elitesql/releases).

- `EliteSQL(path)`: embedded in-process over the C ABI (`libelitesql`).
  ctypes releases the GIL on every foreign call, so threads truly
  parallelize. The library is loaded from the package itself; a source
  checkout also finds `target/release`, and `ELITESQL_LIB=/path` or
  `EliteSQL(path, lib_path=...)` point anywhere else.
- `SidecarClient(socket)`: client for the sidecar mode
  (`elitesql serve <db> <socket>`) for multi-worker deployments
  (gunicorn, uwsgi).

```python
from elitesql import EliteSQL

with EliteSQL("app.esql") as db:
    db.query("CREATE TABLE notes (body text NOT NULL, emb vector(768))")
    db.create_text_index("notes", "body")
    db.create_vector_index("notes", "emb", quantized=True)
    db.query("INSERT INTO notes (body, emb) VALUES (%s, %s)", ["hello world", embedding])
    rows = db.query(
        "SELECT * FROM notes WHERE body = %(body)s LIMIT %(limit)s",
        {"body": "hello world", "limit": 10},
    )

    hits = db.search_hybrid("notes", text=("body", "hello"), vector=("emb", embedding))
    with db.snapshot() as snap:
        rows = snap.scan("notes")   # stable read while others write
```

`query(sql, params=None)` binds parameters without string interpolation.
Sequences use `?` or `%s`; mappings use `%(name)s`. Supported Python values
include `None`, booleans, signed 64-bit integers, floats, strings, bytes,
`datetime`/`date`/`time`, JSON dicts/lists and numeric lists for vector columns.

## Building and releasing wheels

`bash bindings/python/build_wheel.sh` builds `libelitesql` with Cargo, copies
it into the package and produces `dist/elitesql-<version>-py3-none-<platform>.whl`
(`pip install build wheel` first). `ELITESQL_LIB=` reuses an existing library
and `ELITESQL_WHEEL_PLATFORM=` names the platform tag; Linux wheels meant for
distribution are built inside `quay.io/pypa/manylinux_2_28_<arch>` so they run
on glibc 2.28 or newer.

The `Wheels` GitHub workflow does this for Linux x86_64, Linux aarch64 and
macOS arm64 on every `v*` tag, smoke-tests each wheel from a clean virtualenv,
attaches them to the GitHub Release, and uploads them to PyPI through
[trusted publishing](https://docs.pypi.org/trusted-publishers/) when the
repository variable `PYPI_PUBLISH` is `true`. One-time setup on PyPI: add a
pending publisher for project `elitesql` with owner `jalbarracinv`, repository
`elitesql`, workflow `wheels.yml`, environment `pypi`; then create the `pypi`
environment in the repository settings and set the variable. The package
version comes from `pyproject.toml` and should match the tag.

## Error codes and retries

Every failure raises `EliteSQLError` with a stable `code` (also exposed as
constants on the class). Two questions decide what to do next:

| Code | Constant | Meaning | Retry? |
|---|---|---|---|
| 9 | `CONFLICT_RETRY` | Optimistic commit conflict; nothing was published | Yes, the whole transaction (`run_transaction` does this) |
| 21 | `TRANSACTION_EXPIRED` | Sidecar rolled the transaction back at its 30 s deadline | Yes, the whole transaction |
| 11 | `UNIQUE_VIOLATION` | Constraint refused the write | No: fix the data |
| 17 | `COMMIT_UNKNOWN` | **The write is published and visible.** Only its durability across a power loss is unknown (a sync failed). Further writes are fenced until the database is reopened | **Never blindly.** Read back, then decide |
| 1 | `IO` | I/O error. Over the sidecar this is also a lost connection or timeout: a `commit` or autocommit statement that was already sent **may have been executed** | **Never blindly.** Reconnect, read back, then decide |
| 2 | `CORRUPT` | On-disk validation failed; see `elitesql check` and `elitesql repair` | No |
| 10 | `DATABASE_LOCKED` | Another process holds the directory | Later |
| 13 | `READ_ONLY` | Handle opened read-only | No |
| 16 | `MEMORY_LIMIT` | Operation exceeds its memory pool; split it | With smaller work |
| 20 | `AUTH` | Sidecar token missing or wrong | No |

`error.retry_safe` is `True` for 9 and 21; `error.maybe_published` is `True`
for 17 and 1. A catch-all `except EliteSQLError: retry` duplicates rows on
17 and 1: make inserts idempotent (explicit `id` or a unique key) if you retry
those.

### Sidecar timeouts

`SidecarClient(..., timeout=seconds)` bounds every request. When a request
times out the client closes the connection and raises code 1: the late
response cannot be matched to a later request (every request carries an id the
server echoes, and a mismatch also closes the connection). Create a new
client to continue; an in-progress transaction on the old connection is rolled
back by the server when it notices the disconnect.

### Durability notes

- `EliteSQL(path, durability="balanced"|"fast")` acknowledges commits before
  they reach the disk. A clean `close()` syncs everything acknowledged; an OS
  crash before that may lose recent commits, never corrupt the database.
- On macOS `fsync` does not flush the drive's cache. Pass
  `full_fsync=True` in the options (see `EliteSQL.__init__`) for `safe` to
  survive a power loss there; it is much slower. Linux needs nothing.
