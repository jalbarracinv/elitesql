# Recovery guide

Design principle: `After a crash, the database opens to the last fully
committed state.` A commit is either fully visible or not visible at all.
And the golden rule: `Data files are canonical. Indexes are disposable.`

## What happens on a crash

EliteSQL assumes the process can die on any instruction. A commit writes:
(1) large blob chunks (fsync), (2) the checksummed WAL record (fsync per the
durability mode), (3) the in-memory apply. The manifest is only ever replaced
through a temp file + atomic rename, keeping the previous one as
`manifest.prev`.

Opening after a crash:

1. `manifest` is read; if its checksum fails, `manifest.prev` is used (and
   the primary is re-established without ever clobbering the good copy).
2. The listed segments are loaded (per-entry CRC).
3. The WAL is replayed from the manifest's watermark — idempotent replay; a
   torn tail (a half-written commit) is truncated: that commit never existed.
   All or nothing.
4. Indexes (secondary, text, ANN) are rebuilt from canonical data; the ANN
   graph is loaded from its dump when valid and caught up, or rebuilt.

This is verified with real crash injection: processes killed with `kill -9`
at random points, thousands of rounds, zero acknowledged commits lost and
zero partial commits visible.

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
schema. Requires a readable `catalog.json`.

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
