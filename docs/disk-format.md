# On-disk format

An EliteSQL database is a self-contained directory. All integers are
little-endian. Current `format_version`: 2.

```text
app.esql/
  ELITESQL        # marker: "elitesql format_version=2\n"
  LOCK            # flock: exclusive (writer) or shared (read-only)
  catalog.json    # compatibility/tooling mirror of the catalog
  manifest        # atomic data + schema generation
  manifest.prev   # redundant recovery copy after successful publication
  wal/NNNNNN.wal  # durable commits since the last checkpoint
  segments/NNNNNN.seg   # immutable data (created at checkpoint/compaction)
  vectors/XXXXXXXX.vidx # persisted ANN graphs (derived, disposable)
  vectors/XXXXXXXX-<ulid>.vidx.run  # immutable HNSW runs published while running
  vectors/XXXXXXXX.vidx.runs        # run manifest: which runs cover which generation
  blobs/<ulid>.blob     # large-blob chunks (out-of-line)
```

## manifest

`ESQLMANI` (8 bytes) + u32 crc32 of the body + u32 length + JSON body:
`{format_version, committed_version, segments: [{id, len}], wal_id,
required_wal_id, identity_high_water, catalog}`. Embedding the catalog makes every canonical
generation self-contained: schema cannot advance independently of its data.
Publication: write `manifest.tmp` (fsync), rotate `manifest` →
`manifest.prev`, rename `manifest.tmp` → `manifest`, fsync the directory.
A crash between the renames leaves a valid old `manifest.prev`. Once the new
primary is durable, the fallback is atomically refreshed to the same generation
and synced before success is reported.

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

Idempotent replay: records at or below the manifest's watermark are skipped.
Recovery checks the entire WAL chain before modifying canonical files. The
tail a crash leaves (a strict prefix of a record, a zero-filled extent, a
record with a bad CRC or unknown kind) is an incomplete final record and is
truncated **only when no complete, checksummed record follows it**; the same
damage before a later valid record, or a version gap, is corruption. Normal
open refuses corruption and preserves the WAL for explicit salvage. Bit rot
confined to the last record is therefore indistinguishable from a torn write
and is truncated; every earlier record keeps its CRC protection.

Background checkpoint reserves a bridge WAL (id+1) and a new writer (id+2).
Before switching writers, it persists `required_wal_id` in both manifest copies.
The final manifest anchors the bridge and still requires the new writer.
Thus deleting even an empty required successor is detectable. Older manifests
default this field to zero and discover consecutive successors by enumeration;
they cannot prove that an absent last successor existed before this upgrade.
When recovery resumes writing in a successor beyond `required_wal_id` (the
checkpoint that reserved it never published), open republishes the manifest
with the real extent before accepting commits. An incomplete tail before
successors is accepted only while those successors hold no records.
Obsolete WALs are removed only after canonical publication is durable.

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

## Sorted index runs (`indexes/`)

Primary, secondary and text indexes share the disposable `ESQLPAGE` format.
New runs use version 3, independently of the canonical database format version.
The 48-byte header contains magic, version, page size, generation, entry count,
directory offset/count and a CRC over its first 44 bytes.

Each V3 page contains `u32 payload_len`, `u32 payload_crc`, `u32 first_key_len`,
`u32 last_key_len`, the first/last keys, then sorted key/value entries. The
directory is `DIR3` followed by one `u64 page_offset` per page and a final
`u32 navigation_crc`. That checksum covers, in page order, every page header
and its boundary keys, followed by the directory magic and offsets. Open
validates it and contiguous page extents before consulting any key range.
Payload CRCs are checked when a page is read.

V1/V2 runs remain readable, but their unprotected boundaries are checked against
the actual checksummed entries, their ordering and total count during open.
A bad primary run is rebuilt from canonical segments before serving writes.
V3 disjoint-range merges can copy whole pages while updating the navigation
checksum; the canonical segment and WAL layouts are unchanged by this upgrade.

## ANN graphs (`vectors/`)

`ESQLVIDX` + crc + length + body: the index identity (table, column, metric,
m, ef_construction, quantized), the dump's commit version, and the full
graph (f32 or int8+scale vectors, levels and neighbors, tombstones).
File name: crc32 of "table\0column" in hex.

An index is a set of these immutable graphs. The first is written when the
index is created over existing rows, on compaction, or when a database with
no usable graph is opened; every later flush of the mutable HNSW overlay (a
background publication while the mutable index pool fills, and the clean
close) writes another graph as `<stem>-<ulid>.vidx.run`. Searches merge all
of them; an id present in a newer run supersedes its copies in older ones.
`<stem>.vidx.runs` (`ESQLDRN1` envelope, kind `Vector`) lists the runs in
generation order with the commit version the set covers completely. It is
published under the commit mutex, so a crash leaves either the previous
manifest or the new one, never a partial set.

On open: if the manifest validates and is not newer than the state, every
run is mapped (a 100K x 64 graph opens in tens of milliseconds) and the
index is caught up with the commits after the manifest generation, including
records whose vector was removed while the runs were being written. Without
a manifest the single base file is loaded and caught up as before. Any
missing or corrupt file falls back to a rebuild from canonical data. Runs no
longer listed by a valid manifest are removed at open, and dropping the index
removes all of its files.

The maintenance worker bounds how many runs accumulate. Runs are size-tiered
by live vectors: whenever four or more runs lie within a factor of two of
each other (runs with no live vector always qualify) and the rebuilt graph
fits the maintenance pool, one background merge re-inserts their live
vectors into a fresh graph, written as another `.vidx.run`. The merge plans
under the state lock (which ids each run currently owns), rebuilds from the
run files without any lock, and publishes under the commit mutex: ids that
stopped being live meanwhile are dropped from the new run, the old runs are
unmapped and unlinked, and the manifest is republished at its previous
generation, which the new set still covers. A merged run is stamped with the
commit version of its snapshot, so the invariant used by catch-up (a vector
committed at version `v` lives in a run stamped at or after `v`) holds; the
manifest loader accepts run generations up to the committed version for that
reason. Total re-insertion work over an index's life is O(n log n), and a
search visits a few runs per size tier instead of one per publication. A
merge reserves only its estimated footprint from the maintenance pool (at
most half of it) and never the maintenance exclusion, so commits scheduling a
checkpoint or a publication do not wait for it; a rebuilt graph must fit that
half, which bounds the largest run a merge can produce.

## Versioning policy

`format_version` lives in the marker, the catalog and the manifest; a
different number rejects the open with a clear error. Derived indexes (vidx)
carry their own format number: an old version is simply discarded and
rebuilt.
