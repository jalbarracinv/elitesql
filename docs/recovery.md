# Recovery guide

Design principle: `After a crash, the database opens to the last fully
committed state.` A commit is either fully visible or not visible at all.
And the golden rule: `Data files are canonical. Indexes are disposable.`

## What happens on a crash

EliteSQL assumes the process can die on any instruction. A commit writes:
(1) large blob chunks (fsync), (2) the checksummed WAL record (fsync per the
durability mode), (3) the in-memory apply. The manifest is only ever replaced
through a temp file + atomic rename. `manifest.prev` remains a usable old
generation during replacement and is refreshed to a redundant copy of the
new data-and-schema generation before success is reported.

Opening after a crash:

1. `manifest` is read; if its checksum fails, `manifest.prev` is used (and
   the primary is re-established without ever clobbering the good copy).
2. The listed segments are loaded (per-entry CRC).
3. The required active WAL is replayed from the manifest's watermark with
   contiguous commit versions — idempotent replay; a
   torn tail (a half-written commit) is truncated: that commit never existed.
   All or nothing.
4. Indexes (secondary, text, ANN) are rebuilt from canonical data; the ANN
   graph is loaded from its dump when valid and caught up, or rebuilt.

This is verified with real crash injection: processes killed with `kill -9`
at random points, thousands of rounds, zero acknowledged commits lost and
zero partial commits visible.

## Backup and restore

The first line of defense is a backup taken BEFORE anything goes wrong:

```bash
elitesql backup app.esql backup.esql     # snapshot-consistent copy, verified
elitesql restore backup.esql app.esql    # validate a backup, materialize it
```

`backup` is a logical copy under a snapshot: it contains exactly the state
committed when it started, preserves ids, schemas and index definitions, and
is born compacted. From the CLI it needs the lock (no other process may hold
the database open); embedded apps can call `db.backup(dst)` directly and
back up WHILE other threads keep committing — writers are never blocked.
The copy is written to a unique `<dst>.<ulid>.partial` sibling and renamed at
the end (an interrupted backup never leaves a partial directory under the
final name or reuses another process's temporary path), then verified with
`check` before reporting success.

`restore` refuses to touch an existing destination, runs `check` on the
backup first (a damaged backup is reported, never restored), copies the
canonical files, and opens the result once to replay the WAL and rebuild
derived indexes.

A cold copy (`cp -R`) of a CLOSED database is also a valid backup — the
directory is self-contained. Never file-copy a database that a process has
open for writing: a multi-file copy is not atomic and can miss a WAL rotated
away by a checkpoint mid-copy.

## Tools, in escalation order

### 1. `elitesql check <db>` — diagnosis

Offline validation of checksums and structure: manifest and its fallback,
segment entries, WAL records, referenced blob chunks, orphan files. Modifies
nothing. Non-zero exit code when errors exist.

### 2. `--read-only` — inspecting a damaged database

If a normal `open` refuses the database (corruption inside a listed segment),
read-only mode opens anyway: it exposes the valid prefix of every file,
writes not a single byte (shared lock, no WAL truncation, no healing, no
dumps), and rejects every write with `ReadOnly` (code 13). Useful for
inspecting and exporting before repairing:

```bash
elitesql export damaged.esql table --read-only > rescue.jsonl
```

### 3. `elitesql repair <src> <dst>` — salvage

Builds a NEW database at `dst` with everything recoverable from `src`: it
walks segments and WAL directly (valid prefix of each file), takes the latest
version of every record, honors tombstones and re-inserts using the catalog's
schema. It prefers the catalog embedded in a valid manifest and falls back to
legacy `catalog.json` only for older databases. The source is held under a
shared process lock, so salvage refuses to race a live writer.

It is never silent — the report counts recovered records, legitimate
deletions, skipped entries, and one note per damage found. Whatever sat after
a corruption point within a file is lost (and the report says how much).

## Semantics per durability mode

- `safe`: an acknowledged commit survives both process AND OS crashes.
- `balanced`: survives process crashes; an OS crash may lose the last ~25ms
  of commits.
- `fast`: survives process crashes (the WAL was written; the page cache has
  it); an OS crash may lose commits since the last checkpoint.

In all three modes atomicity holds: never half a commit.

## What does NOT self-repair

- A corrupt `catalog.json`: salvage needs the schema to re-type records;
  restore it from a backup (it is a small, stable JSON — back it up).
- Damaged blob chunks: the record referencing them fails its read with an
  explicit error and check() reports it; the rest of the database keeps
  working.
- Bit rot inside old segment data: open fails; read-only and repair apply
  (valid prefix).
