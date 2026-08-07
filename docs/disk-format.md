# On-disk format

An EliteSQL database is a self-contained directory. All integers are
little-endian. Current `format_version`: 2.

```text
app.esql/
  ELITESQL        # marker: "elitesql format_version=2\n"
  LOCK            # flock: exclusive (writer) or shared (read-only)
  catalog.json    # tables, columns, indexes (atomic JSON via tmp+rename)
  manifest        # atomic pointer to the visible state
  manifest.prev   # previous manifest (recovery fallback)
  wal/NNNNNN.wal  # durable commits since the last checkpoint
  segments/NNNNNN.seg   # immutable data (created at checkpoint/compaction)
  vectors/XXXXXXXX.vidx # persisted ANN graphs (derived, disposable)
  blobs/<ulid>.blob     # large-blob chunks (out-of-line)
```

## manifest

`ESQLMANI` (8 bytes) + u32 crc32 of the body + u32 length + JSON body:
`{format_version, committed_version, segments: [{id, len}], wal_id}`.
Publication: write `manifest.tmp` (fsync), rotate `manifest` →
`manifest.prev`, rename `manifest.tmp` → `manifest`, fsync the directory.
A crash between the renames leaves a valid `manifest.prev`.

## Segments (`segments/`)

An append-only log of record versions. Each entry:

```text
u8   kind          1 = put, 2 = tombstone
u64  version       global commit sequence
u16  table_len     + table name (utf8)
u16  id_len        + id (utf8, ULID or user-provided)
u32  payload_len   + payload (encoded record; empty for tombstones)
u32  crc32         over every preceding byte of the entry
```

The manifest records the valid length (`len`); bytes past it are ignored.
Compaction rewrites segments keeping only the versions visible to the latest
state or to live snapshots.

## WAL (`wal/`)

One record per commit (multi-change, atomic):

```text
u64  commit_version
u32  change_count
per change: u8 kind, u16+table, u16+id, u32+payload
u32  crc32 of the whole record
```

Idempotent replay: records at or below the manifest's watermark are skipped;
a torn tail is truncated. At checkpoint the WAL rotates (id+1) and the old
file is deleted once the manifest is published.

## Record payload

Self-describing (survives schema evolution):

```text
u16 field_count
per field: u16+name, tagged value
```

Value tags: 0 null, 1 bool, 2 int64, 3 float64, 4 text(u32+utf8),
5 blob(u32+bytes), 6 timestamp(i64 µs), 7 json(u32+utf8), 8 vector(u32
count + f32*count), 9 date(i32 days), 10 time(i64 µs), 11 blobref
(u16+name, u64 size, u32 crc — a reference to `blobs/<name>.blob`).

## Blobs (`blobs/`)

Blob values >= `external_blob_threshold` (default 256 KiB) are written
out-of-line BEFORE the WAL commit that references them:
`ESQLBLOB` (8) + u32 crc + u64 len + content. Reads are fully validated.
GC at compaction: chunks not referenced by any surviving payload are deleted
(including orphans from torn commits).

## ANN graphs (`vectors/`)

`ESQLVIDX` + crc + length + body: the index identity (table, column, metric,
m, ef_construction, quantized), the dump's commit version, and the full
graph (f32 or int8+scale vectors, levels and neighbors, tombstones).
Written on database close and at compaction. On open: if it validates and
is not newer than the state, it is loaded and caught up with everything
committed afterwards; on any problem it is rebuilt from canonical data.
File name: crc32 of "table\0column" in hex.

## Versioning policy

`format_version` lives in the marker, the catalog and the manifest; a
different number rejects the open with a clear error. Derived indexes (vidx)
carry their own format number: an old version is simply discarded and
rebuilt.
