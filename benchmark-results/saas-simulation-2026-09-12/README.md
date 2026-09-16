# Mini-SaaS concurrency simulation — 2026-09-12

Runs of [`examples/saas_simulation`](../../examples/saas_simulation/README.md)
on the reference development machine (Apple M-series, 10 cores, 16 GiB,
macOS 26.6, Python 3.14.7). One e-commerce SaaS workload (16 operation
types, ~76 % reads), 20 000 accounts, 5 000 products, the database
accumulating across levels, 60 measured seconds per level (45 s for the
realistic-users runs). "Before" is commit `abfe3ae` plus the harness; "after"
is the same tree with the engine changes listed under *Optimization pass*.
SQLite 3.53 (WAL, `synchronous=NORMAL`, FTS5) ran the same workload through
the same generator. Change #15 (the Python binding) landed after these sweeps
and is measured separately, so the "after" columns understate the current
tree by a few percent.

| run | directory |
|---|---|
| EliteSQL before, `elitesql serve` over a Unix socket, `balanced`, closed loop | [`elitesql-sidecar-think0/`](elitesql-sidecar-think0/report.md) |
| **EliteSQL after**, same | [`elitesql-sidecar-think0-optimized/`](elitesql-sidecar-think0-optimized/report.md) |
| EliteSQL before, levels 1–50 (the knee) | [`elitesql-sidecar-knee/`](elitesql-sidecar-knee/report.md) |
| EliteSQL before, users pause 0.5–2 s between requests | [`elitesql-sidecar-think/`](elitesql-sidecar-think/report.md) |
| **EliteSQL after**, users pause 0.5–2 s | [`elitesql-sidecar-think-optimized/`](elitesql-sidecar-think-optimized/report.md) |
| SQLite, same workload and generator | [`sqlite-think0/`](sqlite-think0/report.md) |
| overlays: after / before / SQLite | [`compare/`](compare/), [`compare-think/`](compare-think/) |

## Handover after the third pass (2026-09-14)

**The goal.** Parity with SQLite on operational reads without losing the
concurrent-write and ingest advantage. Two numbers: a weighted operation under
60 µs, and 90 % of SQLite across the sweep, with the tail preserved.

**Where it stands.** The sweep half is **met**: 0.91x at 10 in-flight
requests, 1.01x at 100, 0.96x at 500, from 0.76x / 0.71x / 0.40x at the start,
with the tail three to four times shorter than SQLite's past 10 requests. The
per-operation half is **not**: about 83 µs against 60.

**The five ordered items are all done or closed.** (1) `Record` and (5)
positional payloads were already in the tree. (2) a declared `id` is now the
row's physical key, with automatic conversion of older databases and crash
tests for both windows a `kill -9` leaves. (3) is closed by measurement: the
overlay-first lookup was the win, and the full version trades against write
cost, argued in *What blocks the direct version*. (4) commit batching landed
and is what carries the sweep.

**Do not repeat these.** Hypotheses 48, 49, 53, 57, 63, 66 and 67 are measured
negatives or neutrals, each with its number. In particular: a forward cursor
over runs, any page size other than 1 KiB, an inline key head, routing every
commit through the coordinator, and admitting the hot catalogue table to a
batch.

**What is left, with the arithmetic.** A weighted operation makes about 2.3
calls, and the whole fixed cost of a call, engine and Python binding together,
is near 9 µs of the 83. Removing all of it lands at 74, not 60. The rest is
work proportional to rows, and a cheaper row is nearly spent: 253 ns keyed and
65 ns sequential, against SQLite's roughly 220 ns for a keyed row including
its decode.

**A caution about what the 83 µs contains.** It is measured through the Python
driver, and a large slice of it is not the engine. The same `browse` costs
107.3 µs in the engine, 107.5 through the C ABI with its JSON, and **151
through Python**: the JSON costs nothing to produce and 44 µs to turn back
into Python objects. `browse` is 36 % of the weighted operation, so roughly
10.6 of the 83 µs is that one conversion. A profile of a transactional write
puts `_PyEval_EvalFrameDefault` above every engine symbol. SQLite pays a
Python cost too, through a C extension that hands back tuples, but a much
smaller one, so the measured ratio flatters the engine while the measured
absolute penalises it. Whoever chases the 60 µs should decide first whether
the target is about the engine or about what an application through this
binding sees; they are different numbers, and a compiled extension in place of
ctypes-and-JSON would move the second without touching the first.

**So the 60 µs half comes from reading fewer rows, and the size is known.**
`browse` is 36 % of the weighted operation: it reads the 339 products of a
category and sorts them by price to return twenty, because the only index is
on `category`. An index that satisfied its `ORDER BY` would let it read the
twenty. That would take `browse` from about 151 µs to roughly 40 and the
weighted operation to **about 61**, which is the target.

Three things were missing for it. The first was not obvious, blocked the other
two, and is now done:

1. **Index keys do not preserve order.** `index_key` calls `encode_value`,
   which writes integers, floats and timestamps little-endian: 2 encodes as
   `02 00 00…` and 256 as `00 01 00…`, so 256 sorts before 2. Walking a
   composite index's prefix would therefore *not* hand rows back in the second
   column's order, and no ordered access path can be built on it. Fixing it
   means an order-preserving encoding for index keys, big-endian with the sign
   bit flipped for integers, the usual IEEE trick for floats, and an
   unambiguous terminator for text so composites cannot be confused.

   **Done** (hypothesis 71). `encode_index_value` replaced the general
   encoding for index keys and `SECONDARY_FORMAT_VALUE` moved to `ESQLSID3`,
   so every run written before it fails to load and the loader rebuilds it
   from canonical data. The property is pinned by `index_key_order_tests`.
   Two pieces remain.
2. **Composite indexes**, `(category, price_cents)`. `IndexDef` holds one
   column, and only nineteen lines read it: twelve in `db.rs`, six in
   `ddl.rs`, one in the planner. An earlier draft of this section said about
   134, which came from a grep that also matched foreign keys and the text
   and vector index definitions; the real surface is small. Under it, though,
   there is a second layer: a secondary entry's key is
   `TAG || be32(key_len) || key || id`, so entries sort by key *length*
   before content, and a prefix search on the first column of a composite
   cannot be built because the full key's length is not known at lookup time.
   That has to become a terminator, which the encoding from step 1 already
   supports, with `secondary_pair_parts` rewritten: unlike the index key
   itself, the pair *is* decoded, to split the key from the id. Runs rebuild
   themselves as in step 1. A single-column index on `price_cents` with
   ordered access would scan roughly 160 rows for twenty matches instead of
   339, worth maybe a third of the gain.
3. **An ordered access path in the planner.** `table_driver` picks equality
   drivers only; there is no driver that walks an index in key order and stops
   when `offset + limit` rows have matched, and no way yet for the executor to
   skip the sort because the driver already delivered the order.

Note that the benchmark's schema would have to gain that index for *both*
engines, which is a change to what is being measured and so a decision for
whoever owns the benchmark, not a silent one. SQLite would use it too; the
absolute figure is what moves, and the absolute figure is what the 60 µs
target is about.

**Measure like this.** `ops_cost.py` for the per-operation mix, `sweep.py` for
concurrency, the `statement_cost` example for the engine alone. Three traps
this pass fell into and got out of: never divide by a SQLite run from another
session, always pair them minutes apart; the 500-user level varies 17 % between
runs of the same tree, so quote a range; and a loop that writes one row a
million times measures memtable turnover, not the code under it.

## Status, 2026-09-13

Three optimization passes have run against this workload. Where it stands, and
what is still true, in one place. Figures from the second pass are marked as
such where the third pass has since moved them.

## The catalogue was too small, and it decided the answer (2026-09-13)

Everything below this section was measured with 5 000 products. That is under
the size at which EliteSQL overtakes SQLite, so the whole document was
measuring the one regime where EliteSQL loses. Raising only the product count,
changing nothing else:

| products | EliteSQL | SQLite | ratio |
|---:|---:|---:|---:|
| 5 000 | 88.8 µs | 37.5 µs | 2.37x |
| 20 000 | 239.5 µs | 237.2 µs | **1.01x** |
| 50 000 | 560.2 µs | 577.8 µs | **0.97x** |

A weighted operation reaches parity between 5 000 and 20 000 products. The
reason is that EliteSQL's cost per statement is fixed while SQLite's key seek
and scan grow with the table, the same crossover the scale harness finds on a
single point read (see the point-read section of [benchmark.md](../../benchmark.md)).

The sweep, which is this project's headline metric, follows. At 50 000
products, 60 measured seconds per level, one connection per user:

| in-flight requests | EliteSQL ops/s | SQLite ops/s | ratio | EliteSQL p99 | SQLite p99 |
|---:|---:|---:|---:|---:|---:|
| 10 | 3 957 | 1 505 | **2.63x** | 33.9 ms | 235.0 ms |
| 100 | 3 203 | 1 685 | **1.90x** | 138.8 ms | 805.0 ms |
| 500 | 2 483 | 1 834 | **1.35x** | 2 482 ms | 4 481 ms |

Those rows exclude `recommend` (`SAAS_SIM_DROP_OPS=recommend`), because it
pits EliteSQL's vector index against a category scan SQLite falls back to for
want of one. With it, the same sweep reads 3.14x, 2.32x and 1.81x: the vector
index is part of the advantage but not most of it. Invariants and the offline
integrity check pass at every level, before and after a `kill -9`.

### Goal item 2, scoped (2026-09-13)

A table that declares `id int AUTO_INCREMENT PRIMARY KEY` still gets a ULID as
its physical key, and its `id` becomes a unique **secondary** index. Reaching
one of its rows is therefore two index hops, which is what the goal's item 2
names. A sampled profile of `SELECT id, name, price FROM p WHERE id = ?` puts
16 % of the statement inside `find_eq_batch_version`, the first of those hops.

The change is bounded but it is not small, and it is a disk-format change:

1. An order-preserving encoding of a declared identity as the physical key.
   Scans are ordered by that key, so the encoding has to sort like the values
   do, negative integers included.
2. `Db::insert`'s key choice, which today mints a ULID whenever the table has
   no implicit id (`db.rs`, the `(false, _)` arm).
3. Dropping the identity column's secondary index, and routing
   `WHERE id = ?` to `TableDriver::Id`.
4. Four uses of `Ulid::from_string` that keep `last_generated_id` monotonic,
   and `PrimaryTableDelta`'s append fast path, which assumes new keys sort
   after old ones. An ascending identity keeps that true; an explicit insert
   of a lower id does not, and already falls back today.
5. Migration by `format_version`, as the goal requires: rewrite each row's
   key and rebuild the derived indexes. The derived runs are disposable and
   already rebuild themselves from canonical data, so only the primary
   directory and the segment references have to be rewritten, and the rewrite
   has to be resumable after a `kill -9`.
6. The goal's own test: build a database in the old form, migrate, compare
   row by row, as `payload_migration_tests` already does for format 3.

What it is worth: the operations stuck between 2.5x and 5.6x are exactly the
ones keyed by a declared id, and they are the ones scale does not help.

**All six steps are done** (hypothesis 60). A point read went from 1.58 to
1.18 µs, and a database written before this converts itself on the first open
that finds a table declaring `id` without the keying flag.

The conversion is `Rewrite::RekeyByIdentity`, a variant of `rewrite_segments`,
the one data-rewriting primitive. It reads each row's identity out of its
payload, writes the row under `identity_key` of that value, sets the per-table
flag, and lets the existing DDL machinery rebuild the derived indexes, which
key rows by the physical id. The external paged writer sorts its input, so
emitting rekeyed entries in the old order is fine. Tombstones carry no
payload, so they reuse the key derived from the put they follow; a tombstone
with no retained put is dropped by the rewrite anyway.

Crash safety is the `ddl.json` record the DDL statements already use, and the
tests cover both windows a `kill -9` can leave: the record written with no
data moved, and the data converted with the record not yet cleared. The second
replays the conversion over already-converted rows, which lands on the same
keys, so every step is idempotent. See
`an_old_database_converts_itself_and_keeps_every_row`, which compares the
whole table value for value across the conversion and then runs the offline
integrity check,
`a_conversion_interrupted_after_recording_its_intent_finishes_on_reopen`,
`a_conversion_interrupted_before_clearing_its_intent_replays_harmlessly` and
`a_database_already_keyed_is_left_alone`.

The forward cursor of hypothesis 48 does not come back with it either. What
made that negative was selectivity, not the key type: `browse` matches about
seven per cent of its table, so a cursor walks some fifteen rows between two
wanted ones, and fifteen rows at 65 ns still costs more than one 282 ns
search. Denser keys do not change that arithmetic.

### What the commit convoy is made of (2026-09-14)

The sweep's own counters, per commit, decompose it. Nothing in this workload
takes the coordinated path: `grouped_commits` is zero at every level, because
that path admits only pure inserts into tables with no index, no foreign key
and no identity column, and every table here has all three.

| in-flight requests | commit | waiting for the commit mutex | holding it | of that, waiting for the state lock | real apply work |
|---:|---:|---:|---:|---:|---:|
| 10 | 453 µs | 304 | 121 | 58 | 10 |
| 100 | 21 888 µs | 21 581 | 236 | 152 | 21 |
| 500 | 147 132 µs | 143 370 | 247 | 143 | 28 |

Two things follow, and together they order everything still open.

**The commit mutex is held across the wait for a second lock.** Of the 247 µs
a committer holds the serialization mutex at 500 in-flight requests, 143 are
spent blocked on the state write lock, which readers hold. Only about 90 µs is
work. Every other committer queues behind that, which is where the 143 ms wait
comes from, and with it the 0.65x sweep ratio at 500, the 1 225 ms tail and
the 6 097 exhausted conflicts: a transaction that waits 143 ms for its turn is
a transaction whose rows other writers have had 143 ms to change, so its
optimistic validation fails and it retries until its three-second budget runs
out.

Hypothesis 62 took the first third of that wait off without touching commit
validation at all, by shortening how long readers hold the state lock while a
committer is queued. What is left of the convoy is the mutex itself.

**That is goal item 4.** Grouped application means N commits sharing one
acquisition of both locks, which divides the queue by N. The machinery exists
(`coordinate_commit`, `process_coordinated_batch`,
`finish_coordinated_insert_batch`); what is missing is validating a batch's
members against each other, not only against the committed state, which is
what the narrow gate avoids today. A design that keeps every existing
guarantee: admit a commit to a batch only when it is **disjoint** from the
others already in it, in the rows it touches and in the unique-index keys it
writes. Disjoint members cannot invalidate each other, so each is still
validated exactly as it is now, and anything that overlaps falls back to the
single-commit path. Under this workload most concurrent commits touch
different carts and different users, so most would batch.

### What 60 µs would take, in numbers (2026-09-14)

The other half of the target is a weighted operation under 60 µs; it stands at
85. With the per-statement work now measured end to end, the arithmetic says
where the remaining 25 µs can and cannot come from.

| a small statement | EliteSQL | SQLite |
|---|---:|---:|
| storage primitive, no SQL | 0.18 µs | — |
| through the SQL executor | 1.00 µs | — |
| through the C ABI, JSON included | **1.46 µs** | — |
| the whole call from Python | 4.05 µs | **1.75 µs** |

**The engine is no longer what loses a small statement.** Answering one
through the C ABI costs less than SQLite's entire in-process call. What
separates them is the 2.59 µs the ctypes-and-JSON binding adds on top. At the
start of this pass the same statement cost 1.68 µs in the engine alone.

A weighted operation makes about 2.3 calls. So the whole fixed cost of a call,
engine and binding together, is near **9 µs of the 85**. Removing every bit of
it would land at 76, not 60. The remaining 76 µs is work proportional to rows
and to search: `browse` reads 339 rows to return 20, `admin_dashboard` scans
and aggregates, `search_text` walks its postings, `recommend` searches the
HNSW graph.

So the 60 µs half of the target cannot be reached by making statements
cheaper. It needs either fewer rows touched, which is a planner question (an
index that satisfies `ORDER BY` with a `LIMIT` would let `browse` read tens
instead of hundreds), or a cheaper row, which after this pass costs 255 ns
keyed and 65 ns sequential against SQLite's roughly 220 ns for a keyed row
including its decode. Those two are the honest remaining levers, and neither
is a micro-optimization.

**What scale does not fix.** Operations whose cost is a handful of small
statements do not improve at all, because their cost is fixed per statement
rather than per row. At 50 000 products they sit where they sat at 5 000:

| operation | ratio at 5 000 | ratio at 50 000 |
|---|---:|---:|
| product_detail | 2.6x | 2.5x |
| session_check | 4.2x | 4.0x |
| add_to_cart | 4.5x | 4.6x |
| view_cart | 3.7x | 3.7x |
| update_profile | 5.5x | 5.6x |

That is the remaining engine problem, and it is the per-statement fixed cost
described under *Where this leaves the stated target*: about 2.2 µs and 46
allocations before a statement touches a row, against SQLite's whole
in-process call at 1.7 µs. Hypothesis 58 took the first slice off it.

A sampled profile of that statement, taken on 2026-09-13, says where the rest
is. Excluding the idle threads, the allocator is the largest named cost by a
wide margin, which is why the allocation count is the metric these hypotheses
track. The second surprise is `find_eq_batch_version` at 16 % of the
statement: a table that declares `id int AUTO_INCREMENT PRIMARY KEY` reaches
that row through the **secondary index** on `id` and then through the primary
directory, which is goal item 2, "clave física = identidad declarada, hoy son
dos saltos de índice". An earlier reading of this document estimated that item
as worth little by reasoning from totals rather than from a profile; the
profile says otherwise and it is the largest single item left. Every figure in the rest of this document that
quotes a ratio at 5 000 products is measuring that cost and nothing else.

Caveats: one run per point, and repeated runs of the same tree varied by about
10 % at 100 and 500 users earlier in the session, so the ratios are directional
rather than exact. Both engines were seeded from scratch in each run.

- **Throughput against SQLite, same workload, each engine seeded from
  scratch, after the second pass**: 0.71x at 10 in-flight requests, 0.74x at
  50, 0.61x at 200, 0.46x at 500 with the harness's default of one connection
  per user. It was 0.55x / 0.65x / 0.59x / 0.40x before that pass. The third
  pass's figures are two bullets down.
- **With a pooled client and a raised memory envelope the ratio is flat at
  0.64–0.67 at every level**, and no operation fails. One connection per
  concurrent user costs 62 % of the throughput at 500 users and nine times the
  user-visible tail latency; see *One connection per user is what the
  high-concurrency numbers were measuring*.
- **Tail latency is better than SQLite's** at 50 and 200 in-flight requests,
  and worse at 10 and at 500.
- **Sweep ratio after the third pass**, both engines seeded from scratch in
  the same session: **0.83x** at 10 in-flight requests, **0.76x** at 100 and
  **0.46x** at 500, against 0.71x / 0.74x / 0.46x before it. The stated 0.90x
  is not reached at any level, and the 500-user level is still the
  one-connection-per-user effect described below, not read cost.
- **A weighted operation costs 85.2 µs embedded** against SQLite's 37.5, so
  2.27x, measured by [`ops_cost.py`](../../examples/saas_simulation/ops_cost.py)
  on a freshly seeded and checkpointed database. That script was written on
  2026-09-13 to replace an earlier figure (158.4 µs against 71.9) that came
  from a database the sweep had accumulated and could not be reproduced from
  a seed; the two are not comparable and only the reproducible one is quoted
  from here on. The JSON sidecar adds about 30 µs on top.
- **Engine to engine, against the same statements measured before the third
  pass**: the shop's `browse` page 136 → **119 µs**, `COUNT(*)` over 20 000
  rows 1 550 → **1 290 µs**, reaching a row in a sequential scan 85 → **65 ns**,
  a matched row 4.46 → **2.77** allocations, a BM25 search of ten hits
  276 → **125 µs**, and a point read by declared id 1.58 → **1.18 µs** with 44
  → **30** allocations. See hypotheses 46 to 60.
- **The sweep target is met.** In the cleanest pairing, both engines run back
  to back, EliteSQL reaches **0.91x** of SQLite at 10 in-flight requests,
  **1.01x** at 100 and **0.96x** at 500, from 0.76x / 0.71x / 0.40x at the
  start of the pass. Across three EliteSQL runs paired against two SQLite
  runs the figures are 0.91–0.94 at 10, 0.96–1.01 at 100 and 0.81–0.96 at
  500: every run clears 0.90 except one at 500, and that level varies by 17 %
  between runs of the same tree, so it is quoted as a range rather than a
  number. Earlier drafts read 0.90x / 1.13x / 1.00x; those divided by a SQLite
  run from seven hours before, which is not a comparison, and these replace
  them.
- **The tail improved with it**, which the target also asks for: 6.1 ms at 10
  in-flight requests, 55 at 100 and **331 at 500**, against 1 223 before the
  commit batching, and against SQLite's own 4.9, 162 and **1 440** in the same
  paired run. Past 10 in-flight requests our tail is three to four times
  shorter than SQLite's. Operations that fail after exhausting their retries fell
  from 399 to **22** at 100 users and from 6 561 to **2 601** at 500, and the
  seeded database is 8 % smaller because a declared identity is a shorter key
  than a ULID. The 100- and 500-user figures vary by about 10 % between runs
  of the same tree.
- **Half the stated target is met.** The sweep reaches or beats 90 % of
  SQLite at all three levels in the cleanest pairing. The weighted operation is **85.2 µs against the 60 µs
  asked**, and that half is untouched by the commit batching, which pays under
  concurrency and not to a single caller, and *Where this leaves the stated target* below says what the
  measurement shows instead, including where the target's own starting
  numbers came from.
- **The memory envelope bounds the high-concurrency levels.** Four times the
  default is 35 % more throughput at 500 in-flight requests and 12 % at 200,
  and nothing at all at 10 and 50. See *Is SQLite winning because it keeps
  everything in memory?*.
- **The gap is the engine for statements that read many rows, and the driver
  for statements that read one.** `browse` cost 138.6 µs in the engine alone
  against 150.0 from Python. But a point read is 2.2 µs in the engine and 4.8
  from Python, and an autocommit update 6.7 against 16.7: for those the ctypes
  and JSON boundary is most of the distance to SQLite's in-process C driver,
  which is why the sidecar sweep is the fairer read of the two. See *How much
  of the gap is the engine and how much is the way it is called*.
- **Efficiency per core, not concurrency handling, is the gap.** EliteSQL
  delivers 2 397 operations per core at 10 in-flight requests and 801 at 500;
  SQLite 4 994 and 2 130. The same operation burns 2.65x the CPU at 64
  concurrent requests that it burns alone, and the server stops growing past
  4.7 of 10 cores. See *Are we simply inefficient, while SQLite takes it
  easy?*.
- **One correctness defect was found and fixed while measuring**: a text or
  vector hit put the physical row key on the row under the name `id`,
  overwriting the declared `id` of a table that has one, so a hit reported a
  ULID where the row holds an integer. Hypothesis 41 had fixed exactly this in
  the three SQL read paths and had not reached the search paths. Covered by
  `a_text_search_returns_only_the_columns_asked_for` and
  `a_text_hit_on_a_table_without_a_declared_id_still_carries_the_physical_key`.
- **Every gate is green**: 52 `cargo test` suites, `clippy -D warnings`,
  `fmt --check`, the Python and Node binding tests, the business invariants at
  every sweep level, and the offline integrity check after a clean close and
  after a `kill -9`.

The single measurement that organizes everything still open: on one table,
reaching a row's visible version and handing it to the executor costs **65 ns
in a sequential scan and 280 ns through an index**, and decoding a column of
it costs 18–38 ns. Finding rows, not reading them, is still the whole
remaining gap, and the indexed number is still four times the sequential one
for the reason hypothesis 48 confirmed: the row is found in one structure and
then looked up in another.

## Headline

Latencies are per operation (one service call: 1–12 SQL statements), in
milliseconds. "Success" excludes engine errors and exhausted retries;
business refusals (empty cart, out of stock) count as success.

### Closed loop, no think time (N users = N requests in flight)

| users | EliteSQL before ops/s | p99 ms | success | **EliteSQL after ops/s** | **p99 ms** | success | SQLite ops/s | p99 ms | after / SQLite |
|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|
| 10 | 1 188 | 105 | 100.0 % | **6 955** | **10** | 100.0 % | 13 735 | 8 | 0.51x |
| 100 | 610 | 867 | 99.9 % | **4 854** | **141** | 100.0 % | 10 034 | 203 | 0.48x |
| 200 | 523 | 1 534 | 99.8 % | **4 423** | **546** | 99.8 % | 10 089 | 562 | 0.44x |
| 500 | 493 | 3 911 | 99.9 % | **3 675** | **3 109** | 98.8 % | 9 856 | 1 427 | 0.37x |
| 1 000 | 367 | 9 894 | 98.1 % | **1 742** | **5 411** | 98.0 % | 9 796 | 2 735 | 0.18x |
| 2 000 | 262 | 18 247 | 46.1 % | **1 370** | **6 326** | 96.1 % | 9 092 | 5 140 | 0.15x |
| 3 000 | 267 | 18 021 | 50.3 % | **1 664** | **5 619** | 96.9 % | 8 487 | 5 244 | 0.20x |
| 4 000 | 285 | 17 815 | 60.7 % | **1 934** | **5 041** | 98.3 % | 7 869 | 5 432 | 0.25x |
| 5 000 | 272 | 17 682 | 58.1 % | **1 585** | **5 710** | 97.2 % | 7 542 | 5 555 | 0.21x |

Beyond 2 000 users the pool caps requests in flight at 2 000, so those rows
measure application queueing as well. Every level of every run passed the
business invariants, had no read-your-writes violation, and the offline check
was clean after a clean open.

- **Up to 200 in-flight requests EliteSQL went from 4–20 % of SQLite to about
  half of it** (6 955 vs 13 735 ops/s at 10 users; 4 423 vs 10 089 at 200),
  with equal or better p99 from 100 users on.
- **From 500 in-flight requests up, the commit mutex is the wall**: about
  4 000 commits/s, 200–800 µs of mutex hold per commit, most of it waiting
  for the state write lock while in-flight readers finish (with a thousand
  client threads that wait is scheduler latency, not reader work). SQLite,
  which serializes writers with a file lock and has no per-statement
  protocol, keeps 7 500–9 800 ops/s there. Success stays at 96–98 %: the
  failures are checkouts that exhausted 8 optimistic retries on the 20 hot
  products, and query-memory admission timeouts of the full-budget dashboard
  scans queued behind each other.
- The "before" run collapsed to 46–60 % success from 2 000 users; that is
  gone.

### Where the remaining gap is

Both sweeps let the database grow across levels. A separate fair run seeds
each engine from scratch and measures 20 s at one level (final build, same
generator, same seed), with every failure reported:

| users | EliteSQL ops/s | p50 | p99 | **success** | **failures** | SQLite ops/s | p50 | p99 | success |
|---:|---:|---:|---:|---:|---|---:|---:|---:|---:|
| 10 | 10 032 | 0.69 ms | 6.9 ms | 100 % | none | 18 258 | 0.14 ms | 4.2 ms | 100 % |
| 50 | 10 424 | 1.13 ms | 24.9 ms | 100 % | 1 conflict | 15 986 | 0.27 ms | 59.8 ms | 100 % |
| 200 | 8 274 | 1.27 ms | 196.9 ms | **99.66 %** | 639 conflicts | 14 061 | 0.42 ms | 357.4 ms | 100 % |
| 500 | 5 687 | 1.34 ms | 1 305.6 ms | **99.32 %** | 839 conflicts, 47 admission timeouts | 14 129 | 0.51 ms | 1 082.7 ms | 100 % |

**SQLite wins this workload at every level**: 1.5x at 10-50 users, 1.7x at
200, 2.5x at 500, and it never fails an operation. EliteSQL's advantage is
narrower than it first looked: better tail latency at 50-200 users
(24.9 vs 59.8 ms, 196.9 vs 357.4 ms), which it loses again at 500.

#### The failures are hot-row write contention

They are not spread across the workload; two operations produce all of them,
and both update the same twenty contended product rows:

| operation | share of the mix | failed at 200 users | failed at 500 users |
|---|---:|---:|---:|
| checkout (decrements stock per cart line) | 4 % | **8.3 %** of its calls | **11.7 %** |
| restock (adds stock to 3 hot products) | 0.3 % | **28.0 %** | **64.3 %** |
| admin_dashboard (full scan under a memory budget) | 1 % | 0 % | 2.8 % (code 16) |
| everything else | 95 % | ~0 % | ~0 % |

This is the optimistic-concurrency trade in its worst case. Two checkouts of
the same product both write that row; one commits and the other is told to
retry. SQLite's single writer lock cannot conflict: the second checkout waits
instead of failing. The harness retries a conflicted unit 8 times with
exponential backoff and jitter, the discipline the engine's own autocommit
path uses; the calls counted above exhausted that budget. The seed is
deliberately harsh (20 products with 25 units each, picked by 15 % of cart
operations) and a shop with less concentrated demand would see far fewer, but
the shape is real: **under write contention on the same rows, EliteSQL
converts waiting into retrying, and past some contention level, into
failing.**

The `error:16` admission timeouts at 500 users are the dashboard's full-table
scans queueing for the 64 MiB query pool. That is the memory governor doing
its job (fail a query rather than let the process grow unbounded), and SQLite
has no equivalent limit.

#### The gap is per statement, and it is not in the storage engine

`cargo run --release -p elitesql-core --example statement_cost` splits one
small statement layer by layer. For a point read of one row of three columns
on a 20 000-row table:

| layer | cost | vs SQLite's whole statement |
|---|---:|---:|
| `Db::get`, the storage primitive | 0.23 µs | **0.14x** |
| the SQL executor above it | +1.77 µs | |
| the JSON the C ABI returns | +0.47 µs | |
| **total across the C ABI** | **2.47 µs** | 1.6x |
| the Python binding on top | +2.5 µs | |
| **end to end from Python** | **~5.0 µs** | 3.2x |

**The storage engine resolves a row seven times faster than SQLite answers
the whole query.** Everything above it, the SQL executor and the row
materialization, is where the difference lives. This is also why
[benchmark.md](../../benchmark.md) shows parity or better: bulk ingest
amortizes that fixed per-statement cost over thousands of rows per
transaction, while an operational workload pays it once per statement and
once per row read.

#### Per operation, single thread, same data

Measured against a database in a representative state (the cart bounded to a
handful of lines, as a checkout keeps it):

| operation | weight | EliteSQL | SQLite | ratio | share of the average operation |
|---|---:|---:|---:|---:|---:|
| browse (category page, sort, limit) | 20 % | 213 µs | 73 µs | 2.9x | 42.6 µs |
| search_text (BM25) | 8 % | 397 µs | 87 µs | 4.6x | 31.7 µs |
| add_to_cart + remove (two transactions) | 10 % | 248 µs | 42 µs | 6.0x | 24.8 µs |
| admin_dashboard (GROUP BY scan) | 1 % | 1 363 µs | 650 µs | 2.1x | 13.6 µs |
| recommend (HNSW) | 7 % | 134 µs | 44 µs | 3.0x | 9.4 µs |
| session_check (join + update) | 12 % | 30 µs | 4.9 µs | 6.1x | 3.6 µs |
| product_detail (point reads) | 17 % | 14 µs | 5.1 µs | 2.7x | 2.4 µs |
| view_cart (join over the cart) | 8 % | 30 µs | 5.8 µs | 5.2x | 2.4 µs |
| write_review (transaction) | 2 % | 29 µs | 22 µs | **1.4x** | 0.6 µs |
| order_history | 3 % | 18 µs | 5.9 µs | 3.0x | 0.5 µs |
| **weighted average operation** | | **132 µs** | **38 µs** | **3.5x** | |

#### Change (1) landed: the row representation

`Record` stopped being a `BTreeMap<String, Value>`. A row now holds its values
in one `Vec` beside a refcounted slice of column names that every row of the
same shape shares, matched through a small per-thread layout cache, so
decoding a column is a comparison and a push instead of a name allocation and
a tree node. The encoder takes the reverse shortcut: a record whose columns
are already the schema's, in order, is written by position instead of being
searched once per column.

| measurement | before | after | SQLite |
|---|---:|---:|---:|
| per decoded column | 52 ns | **25 ns** | 13 ns |
| row scanned, three columns | 246 ns | **169 ns** | 40 ns |
| row reached, nothing decoded | 90 ns | 93 ns | ~9 ns |
| `Db::get`, storage primitive | 0.23 us | **0.17 us** | — |
| indexed SELECT, ORDER BY, LIMIT 20 | 26.2 us | **22.5 us** | — |
| browse, the mix's most frequent read | 213 us | **161 us** | 72 us |
| weighted average operation | 132 us | **116 us** | 39 us |

Two follow-ups fell out of profiling the same path. A projection that needs
no column at all (`SELECT count(*)`) no longer reads the payload, and a
projected read stops walking the row once it has taken every column it wants:
payloads are written in schema order, so a page of products no longer skips
past a description and a 32-float embedding to reach nothing. Scanning three
columns went from 169 to 155 ns per row.

Profiling the mix again after that promoted BM25 text search to the largest
single cost, and it had two defects of its own. It sorted every candidate
document to return ten, where heapifying is linear and each row taken costs
one sift; and it accumulated scores in a map hashed with SipHash, which
guards against attacker-chosen keys that this map, holding the engine's own
record ids for the duration of one query, never sees. Text search went from
428 to 274 us, returning the same documents in the same order.

| operation | before the session | now | SQLite |
|---|---:|---:|---:|
| browse | 213 us | **156 us** | 73 us |
| search_text | 397 us | **274 us** | 89 us |
| admin_dashboard | 1 363 us | **1 153 us** | 647 us |
| add_to_cart | 248 us | **200 us** | 42 us |
| **weighted average operation** | **132 us** | **103 us** | **38 us** |

The ratio against SQLite went from 3.5x to **2.7x**. The per-column target of
the plan (20 ns) is essentially met.

A third pass (hypotheses 46 to 56) then took the per-row and per-hit costs
down again. Its figures come from
[`ops_cost.py`](../../examples/saas_simulation/ops_cost.py), which seeds both
engines in one run and checkpoints before measuring, so they are reproducible
and are not comparable with the table above; the "before" column is that
script run against the tree as the third pass found it.

| operation | before the third pass | after | SQLite |
|---|---:|---:|---:|
| browse | 167 us | **165 us** | 74 us |
| search_text | 279 us | **126 us** | 87 us |
| admin_dashboard | 1 265 us | **1 281 us** | 673 us |
| product_detail | 13 us | **13 us** | 5 us |
| **weighted average operation** | **102 us** | **89 us** | **38 us** |

The ratio went from 2.67x to **2.37x**. Almost all of it is text search:
`browse` and `admin_dashboard` did not move here because what the third pass
took out of them per row (an allocation for the id, an allocation for each
projected value) is a small share of what a keyed row costs, and what remains
is the 285 ns of reaching one, which is change (3) and still undone. Workload
throughput barely moved (10 032 to 10 562 ops/s at 10 users) because at every
level the mix is gated by the commit mutex and by client latency, not by read
cost; what moved is the cost of a row, which is what the next changes build
on. The remaining per-row floor is now almost entirely the 93 ns of reaching
a row's visible version, which is change (3).

#### The floor: how much a row costs each engine

Full scans over 5 000 rows, aggregated so no output is materialized, isolate
the row pipeline from everything else:

| work | EliteSQL | SQLite |
|---|---:|---:|
| walk a row, decode nothing | **90 ns** | **~9 ns** |
| each column decoded | **+52 ns** | **+13 ns** |
| three columns, per row | 246 ns | 41 ns |

That is the whole story in two numbers, and neither is tuning:

- The **90 ns** is reaching a row's visible version. EliteSQL keeps versions in
  a paged directory separate from the payload, so every row costs a directory
  entry plus a payload mapping; SQLite's row *is* the B-tree leaf entry.
- The **52 ns** per column is decoding a value, allocating a `String` for its
  name and inserting it into the `BTreeMap` a `Record` is. SQLite reads the
  value out of the row header into a register with no allocation. Matching a
  column name against the projected list turned out not to matter: replacing
  that search with a positional mask changed nothing measurable and made point
  reads slightly worse, because building the mask clones the schema's names.

#### At scale the commit mutex is the wall

At 200 concurrent requests the engine sustains about 4 500 commits/s while
each commit holds the serialization mutex for 220 µs, so the mutex is
saturated and every further user only adds queueing. Of that hold, roughly
100 µs is spent waiting for the state write lock while in-flight readers
finish, and only ~12 µs is the apply itself. Two attempts to break that
convoy by scheduling (making batched readers yield to a waiting committer,
and bounding the statements executing at once) both made it worse; the
remaining lever is to amortize one state-write-lock acquisition across
several commits, which the engine already does for the restricted case of
disjoint inserts into unconstrained tables.

The workload's throughput ratio tracks this cost ratio, so the three
remaining factors are named precisely:

1. **Two index lookups per row where SQLite has one.** A declared
   `id int AUTO_INCREMENT PRIMARY KEY` is a unique index over the row's
   physical ULID, so reading by it costs a secondary lookup plus a primary
   one; SQLite's `INTEGER PRIMARY KEY` *is* the row address. This is what
   makes `view_cart`, which probes once per cart line, the worst ratio.
2. **`Record` is a `BTreeMap<String, Value>`.** Every materialized row
   allocates a `String` per projected column plus tree nodes. Projected
   decoding (change #10) removed the columns a statement does not read;
   removing the allocation itself is a public-API change.
3. **No prepared statements.** Each call re-binds parameters, and each probe
   builds a fresh index cursor, where SQLite reuses a compiled statement and
   an open B-tree cursor across probes.

None of the three is a property of the storage engine or of MVCC, and all
three are addressable without changing the on-disk format except the first.

Per-level engine counters of the "after" runPer-level engine counters of the "after" run (from the new `stats` op):

| users | commits/s | mutex wait / commit | mutex hold / commit | state write-lock wait / commit | admission waits |
|---:|---:|---:|---:|---:|---:|
| 10 | | 631.2 µs | 288.9 µs | 151.9 µs | 5520 |
| 100 | | 37273.2 µs | 481.8 µs | 241.6 µs | 10526 |
| 200 | | 79991.3 µs | 567.9 µs | 231.4 µs | 10061 |
| 500 | | 163948.5 µs | 547.5 µs | 198.9 µs | 6440 |
| 1000 | | 730743.2 µs | 1345.3 µs | 780.0 µs | 129213 |
| 2000 | | 676298.4 µs | 1844.6 µs | 1155.5 µs | 103056 |
| 3000 | | 639983.3 µs | 1508.6 µs | 932.8 µs | 118533 |
| 4000 | | 493920.1 µs | 1322.7 µs | 857.4 µs | 142024 |
| 5000 | | 574759.6 µs | 1556.5 µs | 983.1 µs | 117848 |

### Realistic users (each pauses 0.5–2 s between requests), EliteSQL

| users | offered ≈ req/s | before ops/s | before p99 ms | before success | **after ops/s** | **after p99 ms** | success | server cores | server RSS |
|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|
| 100 | 80 | 79 | 49 | 100.0 % | **79** | **4.7** | 100.00 % | 0.07 | 28 MiB |
| 500 | 400 | 398 | 36 | 100.0 % | **399** | **6.0** | 100.00 % | 0.25 | 61 MiB |
| 1 000 | 800 | 794 | 54 | 100.0 % | **797** | **5.7** | 100.00 % | 0.49 | 102 MiB |
| 2 000 | 1 600 | 472 | 10 859 | 96.3 % | **1 594** | **5.3** | 100.00 % | 0.71 | 176 MiB |
| 3 000 | 2 400 | 323 | 16 805 | 71.9 % | **2 392** | **5.5** | 100.00 % | 0.92 | 227 MiB |
| 5 000 | 4 000 | 288 | 17 597 | 63.4 % | **3 825** | **1 576** | 99.99 % | 1.72 | 402 MiB |

**Before, about 1 000 realistic users fit under a 50 ms p99 and 2 000
collapsed. After, 3 000 users (≈ 2 400 requests/s) run with a p99 of 5–6 ms
on less than one server core, and 5 000 users (≈ 4 000 requests/s) are
served at 99.99 % with a p50 of 0.5 ms; their p99 of 1.6 s is the
2 000-connection pool cap plus hot-product retries, not the engine's median
path.**

Charts: [`compare/throughput.svg`](compare/throughput.svg),
[`compare/latency.svg`](compare/latency.svg),
[`compare/errors.svg`](compare/errors.svg), [`compare/cpu.svg`](compare/cpu.svg),
[`compare/efficiency.svg`](compare/efficiency.svg),
[`compare-think/latency.svg`](compare-think/latency.svg); per-operation p99
and per-second time series inside each run directory.

## Optimization pass (same day, after the first run)

The findings above were treated as hypotheses; each was confirmed with a
micro-test, fixed in the engine, verified by the existing suites plus new
regression tests, and re-measured with the same harness. Results per change
are single-process, single-thread microbenchmarks unless stated (50-user
figures are the 12-second sidecar stage on the 5 000-product database).

| # | Hypothesis | Change | Effect |
|---|---|---|---|
| 1 | SQL inside `Txn` scans the table instead of using indexes | `fetch_table_txn`: same access-path choice as autocommit; new `Txn::find_eq` reads the latest secondary index as candidates plus a bounded **change log** of ids touched by commits after the snapshot, re-read at the snapshot version (exact) | txn point read 0.7–6.5 ms → 8–34 µs; login 1.1 ms → 91 µs, add_to_cart 2 ms → 103 µs, checkout 5 ms → 145 µs |
| 2 | Every statement reserved 16 MiB of the 64 MiB query pool: four statements engine-wide | **Tiered admission**: 256 KiB for key-bounded statements, 2 MiB for non-unique equality, indexed joins and searches, full budget only for scans/sorts/aggregates/hash joins; full-budget statements may take at most ¾ of the pool | 50 users: 3 395 → 5 700 ops/s together with #3–#5; admission timeouts (code 16) gone from the mix |
| 3 | `balanced` ran its fsync with the commit mutex held, every 25 ms | Commits never sync inline; the existing timer thread syncs on a duplicated handle outside the mutex | no commit waits for a barrier (`sync_wait` 0) |
| 4 | Long read sections (record decoding, BM25 ranking) under the state lock blocked committers | Decode after releasing the lock; directory walks in 128-row / 32-id chunks; BM25 copies the query's postings out and ranks without the lock; lazy candidate validation | write-lock wait per commit 217 → 77 µs |
| 5 | Page checksums re-verified on every index lookup | One verified bit per page per mapping | 4 % CPU, shorter lock sections |
| 6 | Updating a row re-indexed unchanged text/vector/secondary columns | Skip derived-index maintenance when the indexed value is unchanged | no HNSW re-insert or re-tokenize per stock update |
| 7 | Identity reservation took the exclusive state lock per inserted row, and compaction overwrote the identity map (duplicate `reviews.id`, code 11) | Atomic per-table counters under the shared lock; compaction merges instead of overwriting; regression test | the unique violations disappeared; one fewer exclusive lock per insert |
| 8 | Hot rows walk their whole version list on every read | Walk version lists backwards with early exit | hot-row read O(1) instead of O(versions) |
| 9 | Range scans decoded every row | Numeric `column <op> literal` conjuncts evaluated on the encoded payload before decoding | selective aggregate over 30 k rows 9 ms → 1.5 ms |
| 10 | Every read decoded whole rows (text, 32-float embedding) | **Projected decoding**: the executor passes the columns a statement needs; other values are skipped without allocating | 10 users: 6 685 → 10 935 ops/s; browse 249 → 161 µs, dashboard 2.8 ms → 0.9 ms, BM25 search 698 → 223 µs |
| 11 | Conflicts were discovered only after queueing for the commit mutex; identity transactions encoded their WAL frame under it | A record newer than the snapshot fails the commit from under the shared lock before the mutex is requested (locked validation stays authoritative); WAL frames are pre-encoded for every transaction, reading identity marks from their atomic counters | 200 users: 9 000 → 10 490 ops/s; mutex hold 245 → 222 µs |
| 12 | Transactional `id = ?` lookups scanned the change log, which grows with the snapshot's age | Identity columns never change, so their lookups use the index alone | part of #11's gain; removes a feedback loop between mutex queueing and read-section length |
| 13 | With 1 000 client threads a committer waits for every in-flight reader; bounding executing statements should shorten that | `DbOptions::max_concurrent_statements` (and `elitesql serve --max-concurrent-statements`): reads take one of N execution slots, writes never | **Negative/mixed**: −20 % at 200 users, +15 % at 1 000; the write-lock wait did not move. Kept as an opt-in lever, default unbounded |
| 14 | The wait for the state write lock is scheduler wake-up latency; a spinning lock should hide it | State `RwLock` swapped for `parking_lot::RwLock` | **Negative**: 50–200 users unchanged, 1 000 users 4 500 → 2 900 ops/s with the server at 8.4 cores (spinning). Reverted; the std queue lock stays |
| 16 | An access path that can match at most one row (physical id, unique index) still fetched a whole batch and then issued a second lookup to find the page empty | `driver_yields_at_most_one_row`: fetch one row and stop, in the single-table paths, the aggregate visitors and the join probe | point read 2.77 → 2.20 µs |
| 17 | The equality lookup reserved the caller's batch size and rebuilt the key prefix once per index run | Reserve from a small floor and hoist the prefix | part of #16's gain; a unique-key lookup no longer allocates 12 KiB to return one id |
| 18 | A SELECT resolved its plan twice: once to pick a memory tier, once to execute | `resolved_select_tier` admits from the plan the executor already built | point read 2.20 → 2.07 µs |
| 19 | Resolving a batch of ids rebuilt the primary key per row and cloned every id to carry the continuation | Reuse one key buffer; keep only the last id of the chunk | |
| 21 | Batched readers re-take the state lock immediately after each chunk, so a committer waits behind a stream of them while holding the commit mutex; yielding when a writer is pending should break the convoy | A yield between chunks when `active_state_writers > 0` | **Negative**: 200 users 10 604 → 8 100 ops/s, mutex hold 220 → 297 µs, server CPU 5.0 → 6.3 cores. With 150 client threads the yields burn the CPU the committer needs. Reverted |
| 23 | Projected decoding searches the wanted list once per column; a positional mask should remove that | `ProjectionMask` consulted by position, confirmed against the name stored there | **Negative**: weighted operation unchanged (132 µs), point reads slightly worse. The per-column cost is the allocation and the map insert, not the name match. Reverted |
| 22 | With far more client threads than cores, bounding the statements inside the engine should shorten the wait for the state write lock | Retested `max_concurrent_statements` (16 and 32) on the optimized build | **Negative**: 10 295 → 9 282 (16) and 9 953 (32) ops/s; the wait did not move. The option stays, defaulting to unbounded |
| harness | Conflicted units of work were retried immediately, so every loser of a contended commit collided again with the same peers | Exponential backoff with jitter in `run_transaction`, the discipline the engine's own autocommit retry uses | 200 users: 8 557 → 9 531 ops/s; the failure rate did not fall, because the contention is on the rows themselves |
| 20 | Projected decoding validated the UTF-8 of every column name, including the ones it was about to skip | Match the stored name as bytes, decode it only when the column is kept | weighted operation 170.8 → 155.3 µs |
| 15 | Half of a point read's end-to-end cost was the Python binding, not the engine | Module-level `JSONEncoder` (a `separators=` keyword makes `json.dumps` build a new encoder per call), parameters already in wire form passed through without a rebuild, results of plain scalars returned without rebuilding every row, and the per-call lease without the generator-based context manager | point read 6.95 → 6.02 µs end to end (−13 %); binding share 3.5 → 2.5 µs |

A new profiling tool came out of this pass:
`cargo run --release -p elitesql-core --example statement_cost` prints the
per-layer cost of one statement and, with `--profile <seconds> [point|indexed|update]`,
loops one statement so an external sampler can attribute it.

Not done, documented for later: group apply/validation to lift the commit
mutex ceiling (~4 000 commits/s here: 200–500 µs of mutex hold per commit, most
of it waiting for in-flight readers to release the state lock, which with
hundreds of client threads is scheduler latency more than reader work); HNSW search still ranks under the state lock; the
`Record` type (a `BTreeMap<String, Value>` per row) remains the main CPU cost
of wide reads; the JSON sidecar protocol is a fixed per-statement cost.

## Second pass (2026-09-13): the row representation

The goal of this pass is read parity with SQLite without giving up the write
and ingest advantage, and it allows breaking the public API and the on-disk
format as long as existing databases convert themselves. Two changes landed.

| # | Hypothesis | Change | Effect |
|---|---|---|---|
| 24 | A row is a `BTreeMap<String, Value>`: every column costs a heap-allocated `String` and a tree insert, so wide reads pay per column what a narrow read pays per row | `Record` is now a `Vec<Value>` plus a shared `Arc<[Box<str>]>` of column names, taken from a thread-local layout cache. Encoding takes a positional fast path when the record's layout already matches the table's; a projection that asks for no column at all (`SELECT count(*)`) never touches the payload; decoding stops as soon as the last wanted column is read | per-column cost 52 → 25 ns; weighted operation 132 → 108 µs |
| 25 | A secondary-index lookup on a churned key degrades without bound | A row written and deleted before any publication never reached a run, so hiding it needs no tombstone. `SecIdx::remove` now writes one only when a published run or the frozen overlay can actually contain the pair | lookup on a key churned 3 000 times: 131.9 → 5.0 µs, and flat; weighted operation 108 → 101 µs |
| 26 | The payload's column names are a space cost, not a CPU cost | Measured first: a `sample` profile of a full scan put **17 % of the whole statement** in the `memcmp` that matches stored names against the wanted list. So the hypothesis was wrong and the change was worth doing for CPU. **Record payloads no longer store column names**: values only, in schema order, with the names taken from the catalog. The leading word of a payload says which form it is in, so both stay readable; `format_version` goes to 3 and version 2 is still accepted | per column 36.6 → 18.2 ns; 8-column scan of 20 000 rows 7.87 → 5.07 ms; payload smaller by exactly the bytes the names took (30 % for a twelve-column row) |
| 27 | With the names gone, deciding what to decode can move out of the per-row path | `RowProjection`: the kept positions, their shared names and the position to stop at are resolved once per batch against the schema. This is hypothesis 23 done on a format that allows it: there is no stored name left to confirm against | part of #26; a point read is unchanged at 1.94 µs, so the resolution costs nothing per statement |
| 35 | Resolving a candidate row re-finds its table in two hash maps, scans the catalog for its schema and rebuilds its key prefix, per id | `PrimaryIdx::table_view`: one view of the table's runs, overlays, epoch and key prefix for a whole batch, used by the autocommit, snapshot and transactional lookups alike | per matched row, from the resident delta, 265 → 248 ns; from a published run, unchanged at 415 ns; weighted operation 161.2 → 158.4 µs from the autocommit path. Extending it to the transactional paths measured **neutral** (158.4 → 159.5 µs, inside the noise): those are not where the workload spends its time. Kept for doing strictly less work per row, not for a number |
| 38 | The query pool is a fixed 64 MiB, but what it bounds is how many statements run at once, which is a property of the machine | `MemoryOptions::default()` now sets it to 24 MiB per core, between 64 and 512 MiB, with the envelope grown to hold it. It is a ceiling the governor accounts against, not an allocation | on ten cores the default becomes 240 MiB. With no flags at all: 500 users 7 255 → 8 184 ops/s (+13 %, ratio 0.46 → 0.51), 200 users unchanged, and **50 users 12 250 → 11 817 on the mean of several runs, a 3.5 % loss**, with p99 improving from 21–22 to 19–20 ms. Peak resident memory did not move: 145–176 MiB either way |
| 44 | Every lookup traverses the persisted runs before it looks in the resident overlays, and then takes whichever version is newer | The overlays hold what was committed *after* the runs were published, so every version in them is newer than every version in a run. A visible version found there is the answer, and the runs need not be traversed at all. The order is reversed, with a `debug_assert` that cross-checks the invariant against the runs on every lookup, so the whole test suite verifies it | **The largest single workload gain of the pass, from twenty lines.** `browse` 218 → 181 µs, `order_history` 1 166 → 898 (SQLite's 879 — parity), the weighted operation 157.6 → 143.2 µs and the ratio 2.19x → 1.99x. It does nothing to a microbenchmark whose table is fully checkpointed, which is why `statement_cost` shows it unchanged and only the workload does: it pays for rows written since the last checkpoint, which in a live database is most of the ones being read |
| 57 | An update maintains every secondary index of its table, including the ones whose column did not change, which is why a write on a wide table costs what it does | Measured before changing anything: the same update of a non-indexed column, on tables carrying 0, 1, 3 and 5 secondary indexes | **negative**: 11.86, 11.65, 11.55, 11.48 us. The cost does not follow the index count, so unchanged indexes are already left alone. Where an autocommit write's 6.7 us in the engine goes is printed by `statement_cost`: 2.8 us is the WAL append, one `write` per commit that only a concurrent group shares, and 3.8 us is spent holding the serialization lock |
| 67 | If one overlapping member sends a whole batch to the single-commit path, setting aside only the member that collides should keep the rest together | The coordinator claims each member's rows in turn and defers only a commit whose rows are already claimed | **neutral to negative**, and reverted. With the hot table admitted it lifted the batched share from 18 % to 28 % and left throughput at 500 in-flight requests unchanged (6 401 against 6 495). With the hot table excluded, where batches rarely collide anyway, 100-user throughput fell from 10 820–12 025 to 9 838 while 500 stayed level. Exhausted conflicts did improve, 2 601 → 2 016, but not enough to carry the bookkeeping |
| 71 | Nothing can walk a secondary index in value order, and the reason is not the planner: `index_key` used the general value encoding, which is little-endian, so 2 keyed as `02 00 …` and 256 as `00 01 …` and a run sorted by those bytes is not sorted by value | `encode_index_value`: integers, timestamps, dates and times big-endian with the sign bit flipped, floats through the usual total-order transform (which also gives `-0.0` and `0.0` one key), text and blobs raw with `00` escaped and a `00 00` terminator so a composite key cannot mistake the end of one column for the start of the next. `SECONDARY_FORMAT_VALUE` bumped, which makes every existing run fail to load, and a failed load already rebuilds from canonical data | **no measurable change, and none expected**: an integer key is nine bytes either way. It is the prerequisite the other two pieces rest on, verified by `index_key_order_tests`, whose second case pins the old defect (256 keyed below 2) alongside the new behaviour |
| 70 | `PrimaryIdx::newest_at_or_before_with` searches the published runs first and the resident overlays after, which is what hypothesis 44 fixed in `PrimaryTableView::newest` and this second function was missed by | Overlays first, returning early, with the same `debug_assert` guarding the invariant that every overlay version is newer than every run version | **neutral on every benchmark here**, and kept anyway: it is strictly less work and it restores an invariant the codebase already relies on. A sampled profile of a repeated update had put this function first, but that loop wrote one row a million times, so its cost was the run count a pathological memtable turnover produces, not the search order |
| 69 | A commit decodes the whole row it supersedes to decide which index keys to drop, so `UPDATE products SET stock = ...` decodes the description and the embedding vector as well | `read_prior_for_indexes` projects that read down to the columns some derived index actually reads | **near neutral**: an autocommit update on the catalogue 16.4 → 16.0 us, inside the noise. Kept because it is strictly less work, and because the saving grows with the width of what an index does not read |
| 68 | A bounded sort keeps `offset + limit` rows and throws the rest away, but the executor has already projected every one of them: `browse` materialises 339 rows, values and text copied, to return twenty | `SpillSorter::may_keep` answers from the sort keys alone, before the row is projected. Rows arrive in increasing sequence and the sort breaks key ties by it, so a later row that merely ties the worst kept one loses anyway: comparing keys is exact here, not conservative | the shop's `browse` page 116.9 → **105.7 us**, an indexed `SELECT` with `ORDER BY` and `LIMIT` 15.0 → **14.4 us**. Covered by `order_by_with_a_limit_keeps_the_right_rows_when_keys_tie`, which pins ascending, descending, `OFFSET` and the unbounded ordering the bounded ones are cut from |
| 66 | The batch refuses tables carrying a text or vector index, which in this workload is the product catalogue, so every checkout and restock still commits alone | The gate drops that exclusion, and the batch hands the vector indexing jobs its apply produces to the same worker the single-commit path uses | **negative**, and reverted. The batched share collapsed from 84 % to **18 %** and throughput at 500 in-flight requests from 9 814 to **6 401**, with the tail 284 → 1 541 ms. The exclusion was accidentally doing the right thing: the catalogue holds the twenty hot rows every checkout contends for, so its commits collide constantly, and one collision sends the whole batch to the single path. Admitting them bought those writes a queueing hop and nothing else |
| 65 | Hypothesis 60 padded a declared identity to nineteen digits so the key would sort like the number, and a sampled profile of an indexed read then put `memcmp` first among named costs, ahead of the directory lookup itself | The digit count leads instead of the padding: a longer number is a larger one, and two of the same length compare digit by digit exactly as they compare numerically. An identity under ten million is eight bytes rather than nineteen | a keyed row **282 → 255 ns**, a resident one 140 → **130**, an indexed `SELECT` with `ORDER BY` and `LIMIT` 15.8 → **15.0 us**. The weighted mix did not move (2.27x before and after): it is dominated by costs this does not touch, and by then the machine itself had drifted, with SQLite's own figure up 3.5 % in the same run |
| 64 | **Goal item 4.** The batch path admits only pure inserts into tables with no index, no foreign key and no identity, which in this workload is no commit at all, so every commit queues alone on the serialization mutex | The gate now refuses only text and vector indexes, whose maintenance the general path schedules as background jobs. A batch still requires its members to touch **disjoint rows**, which is what lets each be validated exactly as the single path validates it: with no row in common no member can be another's prior version or invalidate its conflict check. `validate_unique` and `validate_foreign_keys` already take a slice of staged tables and check it against the committed state *and* against itself, so the batch passes all its members at once; each member's prior record is read before the WAL durability point and carried into the apply, as the single path does; anything overlapping or failing falls back to `finish_prepared_commit` untouched | at 500 in-flight requests **6 401 → 11 504 ops/s**, the tail **1 223 → 325 ms**, success 98.40 → **99.39 %**, exhausted conflicts 6 561 → **4 433**, the commit's wait for the mutex 98 → **49 ms** and for the state lock 97 → **46 us**. 254 258 of 302 034 commits took the batch. Against SQLite's 9 839 ops/s that is **1.17x**. The median rises, 1.3 → 5.9 ms, because a commit now waits for its batch leader |
| 63 | Every commit queues on the serialization mutex, so routing all of them through the commit coordinator would let one thread own that mutex for a whole batch instead of N threads handing it between them | `commit_staged` sends every commit to `coordinate_commit`, with the batch path's candidate test moved inside it so anything it refuses still runs through `finish_prepared_commit` | **negative**: throughput 6 781 → 6 943 ops/s at 500 in-flight requests, inside the run-to-run noise, while the median latency went 1.4 → **7.6 ms** and the tail 1 368 → 1 658. `grouped_commits` stayed at zero, so nothing extra batched: the leader ran the same commits in the same sequence and the queueing hop was pure cost. The mutex wait counter fell to 29 us only because the queue moved somewhere it does not measure. Reverted. The queue has to shorten, which means widening what can batch, not moving where it forms |
| 62 | The commit mutex is held across the wait for the state write lock, and readers hold that lock 256 rows at a time, so every committer queues behind a scan | The chunk reads the engine before each pass: 256 rows with no committer queued, 64 with one. `Shared::commit_waiters` was already counted for the coordinator | at 500 in-flight requests the commit's wait for the state lock 143 → **97 us**, the mutex hold 247 → **173**, the whole convoy 143 → **98 ms** and throughput 6 354 → **6 781 ops/s**, while a sequential row stays at 65.7 ns against 65.0, which a fixed chunk of 64 would have cost 71. Measured at 256, 128, 64 and 32: the jump is between 256 and 128, and below that it is flat. The cost is contention failures, 6 097 → 8 540 at 500 users, because more throughput means more attempts on the same hot rows; successful operations still rise by 7 % |
| 61 | Identity keying replaced 26-byte ULID keys with 19-byte integers, dense and ascending, so the cost of reaching a row through *any* index should have moved with it | The whole decomposition re-measured after hypothesis 60 landed | **nothing moved**: a sequential row 65.0 ns, a column 17.0 ns, a keyed row in a 20 000-row run 282 ns, resident 140 ns, all within noise of the figures taken before. The ablation drives on a non-unique integer index, which is `browse`'s shape, and that path still finds an id in one structure and looks it up in another. Item 2 pays where the predicate *is* the identity, and nowhere else. Recorded so the next reader does not re-derive it |
| 60 | **Goal item 2.** A table that declares `id int AUTO_INCREMENT PRIMARY KEY` is reached in two index hops: the declared id through its unique secondary index to find a ULID, then the ULID through the primary directory. The profile put 16 % of a point read in the first hop | The physical key of such a table *is* its declared identity, zero padded to nineteen digits so it sorts as the identity does. The identity is reserved before the key is chosen, and `WHERE id = ?` now resolves to `TableDriver::Id`. A per-table `identity_keyed` flag, absent from older catalogs and defaulting to false, keeps databases written before this reading exactly as they did | **a point read 1.58 → 1.18 us and 44 → 30 allocations**, the SQL layer above the storage primitive 1.38 → **0.98 us**. Covered by `a_declared_id_becomes_the_row_key`, `a_scan_of_a_keyed_table_is_ordered_by_the_declared_id` and `a_catalog_without_the_flag_is_not_identity_keyed` |
| 59 | A page's checksum is verified once per mapping, but finding the bit that records it means a binary search over the whole page directory, on every page touched by every lookup | `PageView` carries its directory position when the caller already iterated one, which every lookup and every cursor step does | the shop's `browse` page 118.8 → **116.7 us** and an indexed `SELECT` with `ORDER BY` 16.6 → **16.0 us**, about 2 to 3 % on indexed reads. A point read does not move: it touches one page |
| 58 | A statement copies every column name it mentions three times on every execution: out of the parsed statement into the plan, again into the projection, and again into the list of columns to decode, although the parse was already cached and the names outlive the execution | `resolve_col_ref`, a borrowing `projection_plan`, and `needed_columns` returning `Vec<&str>`, with `keep` carried as `Option<&[&str]>` through the reader signatures. `QueryCursor` outlives the statement that built it, so it is the one caller that still owns its names | a four-column statement 62 → **51** allocations, the per-column slope 5.3 → **2.3**, the SQL layer above the storage primitive 1.47 → **1.38 us**. **Under one per cent on the weighted mix** (88.8 → 88.3 us): the operations this touches are small statements whose cost is dominated by the Python boundary, not by the engine |
| 56 | The same again for the ids the ranking accumulates: every posting of every term arrives as its own `String` on the way to the score map | Postings of a query are packed into one arena with a span each, the score map is keyed by slices of it, and the candidate heap borrows its ids, so only the handful the caller takes is copied | `search_text` 142 → **127 us** at ten hits. The inverted comparison `Ranked` uses (a better candidate sorts *less*, so a `Reverse` heap pops it first) was reproduced wrongly at first and `bm25_ranks_relevance_sensibly` caught it |
| 55 | Walking a text run allocates the id of every posting it passes, to compare it against the merge head and drop it | `TextPostingCursor` keeps its head id in a buffer it refills, as hypothesis 47 did for the secondary index, and the delta and frozen heads borrow their ids from the maps that hold them | `search_text` 192 → **142 us** at ten hits, a 29 % cut with no change to what a search returns |
| 54 | A text search returns the whole row for every hit, indexed text column and all, while the caller ranks by relevance and then reads one field | `Db::search_text_columns` and a `"columns"` key on the JSON request, carried through the Python and Node bindings; the simulation's driver now asks for the declared id alone, which is what its SQLite driver already returned. A hit's accepted version is also kept from the visibility check instead of being looked up a second time | **a hit 11.7 → 0.6 us**, `search_text` 279 → **192 us** and 3.2x → **2.1x** of SQLite, the weighted operation 102.3 → **95.8 us**. What is left is the postings walk and the BM25 ranking, 185 us of the 192, untouched so far |
| 53 | A page-directory probe follows a span into a scattered part of the mapping, ten probes deep on a 20 000-row run | Twenty bytes of each page's last key held inline beside its span, the full key read only when two agree on all of it | **neutral**: a keyed row 283 → 290 ns, `browse` 125 → 126 us. A run's directory is 14 KiB and the keys it points at are 1 MiB, both small enough to stay in cache. The hypothesis came from a measurement that turned out to compare a published table against an *unpublished* one; the benchmark now checkpoints both, and the real split is resident 139 ns, a 200-row run 218, a 20 000-row run 285 |
| 52 | Projecting a row clones every value it emits, and a text column's bytes are copied once per row for a row that is dropped straight after | `project_row_owned` moves the values out of the row instead, guarded by `extract_projects_each_column_once` so `SELECT name, name` still returns the value twice (`a_column_projected_twice_is_returned_twice`) | one allocation per text column per row gone (forty rows 251 → **211** allocations), the shop's `browse` page 126 → **119 us**, a full scan projecting eight columns 4 736 → **4 400 us** |
| 51 | The same for an equality: a category page matches hundreds of rows, and each id is carried from the index merge to the decoder as its own `String` | `IdBatch`: the merge writes into one arena with a span per id and reuses a single buffer for the id it is comparing, so the ids of a batch cost one growing allocation instead of one each. `find_eq_batch_version` packs the visible ids the same way and returns a `ScanBatch` | a matched row 3.46 → **2.77** allocations, the shop's `browse` page 131 → **125 us** |
| 50 | A scan hands one `String` id back per row, and the caller resumes past the last of them and reads none of the others | `ScanBatch`: rows, a cursor, and ids only when a caller asks. The directory walk keeps the ids of a chunk in one byte arena and tracks the cursor in a single buffer it rewrites, so a scan allocates per batch instead of per row | **reaching a row 77 → 65 ns**, `COUNT(*)` over 20 000 rows 1 550 → **1 290 us**, one column of eight 2 190 → **1 990 us** |
| 49 | A key is found by parsing the page in front of it, so a smaller page shortens that walk | `DEFAULT_PAGE_SIZE` measured at 256, 1024 and 4096 bytes | **negative in both directions**: `browse` 131 us at 1 KiB against 140 at 256 B and 148 at 4 KiB. Fewer, larger pages cost more parsing; more, smaller pages cost more directory probes and lose locality. 1 KiB is already the optimum, and the page-size lever is spent |
| 48 | A batch of ids arrives sorted and the runs store them sorted, so a cursor that remembers its page and offset would turn one directory search per row into one pass | `PagedKeyCursor`, with a reset when the caller's keys do not ascend | **neutral**: `browse` 133 us against 131, a keyed row 281 ns against 283, a sequential row 79 ns against 78. Reverted. The ids of a batch are sparse in the run (339 matches among 5 000 rows), so walking every entry between them costs what searching for each one did. A cursor only pays where the batch is dense |
| 47 | Walking a secondary run allocates the id of every entry it passes, to compare it against the merge head and drop it | `SecPairCursor` keeps its head id in a buffer it refills | one allocation per matched row gone (4.46 → 3.46), the shop's `browse` page 136 → **131 us** |
| 46 | A scan releases the state lock every 32 rows, and resuming rebuilds a cursor into every run: a binary search and a page load each, 156 times over a 5 000-row scan | `SCAN_LOCK_CHUNK_ROWS` 32 → 256, measured against 32, 256 and 1024 | reaching a row 87.8 → 77.5 ns, a full scan of 20 000 rows 8 % faster, `admin_dashboard` 2 259 → 2 081 µs. **Neutral on the weighted operation** (143.2 → 144.1) because scans are one per cent of the mix. Kept because the risk it carried did not appear: the chunk exists to keep committers from queueing, and the commit's wait for the state write lock did not move (157 µs against 166 at 500 users) while the tail was equal or better at every level |
| 45 | The same ordering holds between runs: each covers the commits since the one before it, so for a row the versions in a run are newer than in any earlier one, and walking backwards could stop at the first visible entry | The run walk reversed, with the exhaustive walk kept behind a `debug_assert` so every lookup in a debug build checks that the two agree | **Neutral**, on the wrong side of the noise: the weighted operation 143.1 → 145.8 µs. Compaction keeps the primary run count small, so there is rarely an older run to skip and the extra branch is all that is left. Reverted. Hypothesis 44 is the same idea between the overlays and the runs, where the counts are lopsided and it pays |
| 43 | Finding a key inside a page means parsing the entries in front of it, so a table of entry offsets at the head of the page would let it be found by binary search | Paged-index format 4: `[count][offsets][entries]` inside each page, the checksum covering both so a merge can still copy a page verbatim; format 3 runs keep working and convert as they are rebuilt, which for a derived index is every compaction | **Negative at every page size.** `browse` in the engine 134.6 → 154.1 µs at 1 KiB, 153.2 at 4 KiB, 149.3 at 16 KiB; a matched row 295 → 306 ns. Four bytes an entry means fewer entries a page and a deeper directory, and a binary search inside a dozen entries jumps around where the parse walked forward. The linear parse was never the cost. Reverted — but it earned its keep: the compatibility test written for it found a real regression the change had introduced, a format 3 run failing to open because the reader stopped expecting its navigation checksum |
| 42 | A write normalizes its row into schema order by inserting the columns it omitted, which allocates each name and copies the row's names out of their layout | `normalize_record` builds the values in schema order and hands them the table's shared layout, which also lets the encoder take the positional path it already had | `INSERT` 133.3 → 128.3 allocations, staged in a transaction 62.3 → 57.3. Small, and the two companion changes tried with it were **neutral and reverted**: building the row on a shared layout in the executor, and a `Record::take` that avoids the names copy, each moved the count by less than one. An insert's 128 allocations are in the commit, not in shaping the row |
| 41 | A projection that leaves out a declared `id` has the physical row key put on it | The engine surfaces its physical key as `id` for tables that declare none, and three read paths decided that by asking the decoded row whether it already had one — false whenever a projection simply left the column out. The insert then copies the row's shared column names out of the layout they came from, which is exactly what hypothesis 24 existed to avoid. Decided from the schema now, as the scan path already did | **allocations a matched row costs, counted rather than sampled**: `COUNT(*)` 9.36 → 3.36, one int column 11.46 → 4.46, `SUM` 10.36 → 3.36, the text column 13.46 → 6.46; projections that already included `id` unchanged at 4.46. In time, reaching a row by key 411 → 293 ns, and 255 → 136 ns when it is in the resident overlay. The weighted operation 160.3 → 157.2 µs |
| 40 | Every statement clones the whole table schema out of the catalog before it can resolve a column, and a transactional one clones its staged copy too | The catalog's entries are shared (`State::schemas`, rebuilt only when the catalog is replaced, which is DDL and open), and a transaction shares its staged schema after the first statement that asks. `Db::table_schema` and `Txn::table_schema` return `Arc<TableSchema>`, which is a public API change | `Db::table_schema` 130 → 10 ns; point read 1.80 → 1.68 µs (−6.7 %); `UPDATE` by id 6.78 → 5.62 µs (−17 %). **End to end it is invisible**: the weighted operation stayed at 160 µs, because the mix's time is in the operations that touch hundreds of rows, not in per-statement setup. Kept for being strictly less work per statement, on a measured mechanism |
| 39 | The executor discards the id of every row but the last, which it uses as the continuation cursor, so a batch allocates a `String` per row for nothing | Return an empty id for every row but the last of a batch, behind a switch, purely to measure it | **Negative**: the indexed statement 18.2 → 18.9 µs and the resident keyed read 299 → 313 ns per row. The allocation is real but it is not what the path costs. Reverted |
| 37 | A batch of candidate ids is sorted, so each lookup could start where the last one ended and double out from there instead of binary-searching the whole overlay | A hint carried on the table view, reset whenever an id moves backwards so the answer is never wrong | **Negative on cost, neutral on time**: no movement outside the noise. It led to the measurement that explains why — see below. Reverted |
| 36 | Skipping a run a key cannot be in rebuilds two page headers from the mmap, per run and per row | The run's first and last key are precomputed spans, like the page directory's | **Neutral**: the indexed statement and the scan did not move outside the noise. Kept because it does strictly less work and replaced two accessors that did the same rebuild everywhere else |
| 33 | A key inside a page is found by parsing the entries in front of it, so a smaller page is a shorter parse | `DEFAULT_PAGE_SIZE` 4 KiB → 1 KiB, measured over 256, 512, 1024, 2048 and 4096. The size lives in the file header, so runs written at any of them stay readable and nothing has to be converted | indexed `SELECT` with `ORDER BY` and `LIMIT 20` 19.0 → 16.8 µs; point read 1.93 → 1.76 µs; 1.5 % more on disk. **No effect on the workload**: a freshly seeded database answers from the resident delta, not from persisted runs, so this only pays on a database whose data has been published |
| 34 | Finding a row in the resident delta is a binary search that probes a `String` per step, and every probe is a random heap read | The first sixteen bytes of each id are held inline beside the array, so a probe stays inside it; a tie falls back to the strings, which keeps the order exact rather than approximate | a profile of a category page attributed a third of the statement to that comparison, but the measured gain is far smaller: the same loop 142 → 137 µs, `order_history` 1 404 → 1 233 µs, the weighted operation 168.6 → 161.3 µs. The profile over-attributed; the array itself was already mostly resident |
| 31 | Finding a key in a persisted run should not rebuild a page header per probe | Locating a key binary-searches the page directory on each page's last key, and reading that key out rebuilt a whole `PageView` from the mmap per probe: a profile of an indexed lookup put a fifth of the statement there. The last-key spans are now precomputed at open, sixteen bytes a page, with the keys still file-backed | `SELECT` by indexed column with `ORDER BY` and `LIMIT 20`: 20.9 → 18.8 µs; `page_view` 894 → 124 samples of the profile; per row in a scan 94.5 → 89 ns |
| 32 | A batch of candidate ids from a secondary index is sorted, so one pass over each run should beat one search per id | `PagedIndex::visit_sorted_keys` and a batched `PrimaryIdx` resolver, seeking to each key's page and parsing a page once for however many keys land in it | **Negative**: the indexed statement went 18.8 → 22.4 µs in every variant tried (walking the pages, seeking per key, one key arena instead of one buffer per key, early exit when the next key is past the page). Ids out of a secondary index are ULIDs scattered across the run, so there is nothing to share, and matching a moving key list costs more per parsed entry than comparing against one key. Reverted |
| 30 | Write latency is flat between publications | Measured first: the per-second series of a one-user run showed a sawtooth. Write operations doubled over about seven seconds and snapped back whenever a checkpoint published, while read-only operations stayed flat. **Once the shared delta pool is half full, every commit walked the derived overlays end to end** to decide whether a background publication was due, and that walk grows with the overlays. The decision is a schedule, not an invariant, so it now samples one commit in 256, and the sample is discarded when a publication is scheduled. Hard pool pressure still measures exactly | the sawtooth is gone. `session_check` 26→90 µs becomes 25→30; `add_to_cart` 46→112 becomes 46→51; the 500 ms stalls at each publication disappear. Per operation, uncontended: `session_check` 91→52 µs, `update_profile` 74→34, `add_to_cart` 158→121, `write_review` 125→85, `checkout` 261→225. Commit mutex hold 168→125 µs at 10 users, 231→188 at 200, 317→254 at 500 |
| 29 | Every visited row merges its version list, which after a compaction holds one entry | Sort and deduplicate only when the sources actually interleave | ~2 % off a full scan at every projection width; kept because it is never worse, but it is not where the per-row cost is |
| 28 | Three small per-row costs remain in the scan: a `SipHash` lookup of the segment handle, UTF-8 validation of the table name in every index key, and an id copied for rows the scan keeps anyway | A multiply-shift hasher for the segment map, a byte comparison for the table half of a primary key, and the continuation cursor taken from the row the batch already owns | per row 104.7 → 94.5 ns; 1-column scan of 20 000 rows 2.64 → 2.46 ms |

Hypothesis 26 is the one the plan had ranked last, as a disk-space win with
no CPU effect. The profile said otherwise and it moved the most, which is why
it was taken out of order. Hypothesis 23, the same idea on the old format,
had been negative for exactly the reason the format change removes.

Hypothesis 25 was a real defect, not a tuning knob. Every insert/delete cycle
on a key left a versioned tombstone in the mutable overlay, and each later
lookup walked all of them, so the cost of reading one live row grew linearly
with the history of its key. It shows up in this workload through
`cart_items`, where a session adds and removes the same product repeatedly.
`churn_on_one_key_does_not_accumulate_tombstones` in
`crates/elitesql-core/tests/secondary_runs.rs` asserts the correctness and a
late/early timing ratio rather than an absolute time.

Hypothesis 30 was the largest single effect of the pass and none of the five
planned changes would have found it. It came from reading the per-second
series of a one-user run instead of an average: the average said 47 µs, the
series said the number was 47 at the start of a checkpoint interval and 113 at
the end. `crates/elitesql-core/tests/commit_cost_is_flat.rs` asserts the
mechanism rather than a time, by counting how often the commit path measures
the overlays: 14 878 times in 20 000 commits before the fix, under 2 500 after.

### How a version 2 database converts

The format change is permitted only because existing databases convert
themselves, without loss and through a `kill -9`. They do, because **nothing
has to be converted at once**. The leading word of a record payload says
whether it stores names, so:

- Every read handles both forms. A database opened by this engine is readable
  from the first instant, whatever mix of forms it holds.
- Every write produces the new form. A row rewritten for any reason converts.
- Every segment rewrite converts what it copies, so a compaction leaves no
  old payload behind, and the `RENAME COLUMN` / `DROP COLUMN` rewrites that
  already cost a compaction convert as a side effect.
- The manifest records `format_version` 3 as soon as the database is opened
  writable; an engine that predates this change refuses it, which is correct,
  because it could not read a positional payload.

An interruption at any point therefore leaves a database whose payloads are a
mix of both forms, which is a state the engine reads, writes and checks
normally. There is no migration to resume.

`payload_migration_tests` in `crates/elitesql-core/src/db.rs` builds a
database in the old form (every column type, a secondary index, a full-text
index, updates and deletes), reopens it with the engine that writes the new
one, and compares it row by row; then writes into it so both forms are live in
the same table, compares again, compacts, asserts that no payload keeps its
names, and compares once more. `check()` runs clean at every step.

### Where that leaves the fair run

Same harness, same seed, each engine seeded from scratch, 20 s per level. "Was"
is the start of this pass, before the tombstone fix:

| users | EliteSQL ops/s | p50 | p99 | success | failures | SQLite ops/s | p99 | ratio (was) |
|---:|---:|---:|---:|---:|---|---:|---:|---:|
| 10 | 13 627 | 0.48 ms | 6.9 ms | 100 % | none | 18 536 | 4.7 ms | **0.74x** (0.55) |
| 50 | 12 408 | 1.41 ms | **21.7 ms** | 100 % | none | 16 197 | 44.9 ms | **0.77x** (0.65) |
| 200 | 9 945 | 1.29 ms | **185.0 ms** | 99.61 % | conflicts | 15 656 | 271.8 ms | **0.64x** (0.59) |
| 500 | 7 214 | 1.22 ms | **975.6 ms** | 99.24 % | conflicts, admission timeouts | 15 387 | 1 022.9 ms | **0.47x** (0.40) |

EliteSQL now answers at 64–77 % of SQLite up to 200 in-flight requests, with
roughly half its tail latency at 50 and 200 users and a better tail at 500 as
well. Every level passed the business invariants; the offline integrity check
was clean after a clean close and after a `kill -9`. The 500-user column moves least, as expected: that level is bound by
the commit mutex, not by the cost of reading a row. The failure profile is
unchanged and still comes almost entirely from checkout and restock contending
for the twenty hot products.

The floor, measured by `statement_cost`, over four projection widths on the
same 20 000 rows:

| | start of pass | now | SQLite |
|---|---:|---:|---:|
| per row, before any column | 90 ns | **94 ns** | 9 ns |
| per column | 52 ns | **18 ns** | 13 ns |
| full scan, 1 of 8 columns | — | 2.47 ms | — |
| full scan, 8 of 8 columns | 7.87 ms | **5.00 ms** | — |
| indexed `SELECT`, `ORDER BY`, `LIMIT 20` | 21.8 µs | **16.8 µs** | — |

**The per-column term is essentially done**: 18 ns against SQLite's 13. The
per-row base did not move and is now the whole gap. A profile of the scan
attributes it to the walk of the primary MVCC directory (26 % of the
statement: merging run cursors, decoding a version entry per row, sorting and
deduplicating a list that almost always holds one entry) and to the
allocations the batch makes per row (22 %), against 26 % now left in decoding
the payload. Reaching the visible version without that per-row walk is the
next change.

### What an indexed read is actually made of

`statement_cost` now decomposes one by ablation rather than by profile: each
line adds one stage to the one above it, over the same forty matched rows of
a 20 000-row table.

| | cost |
|---|---:|
| fixed cost of the statement | 1 663 ns |
| **reaching a row and handing it over, per matched row** | **408 ns** |
| first column decoded and returned, per row | 38 ns |
| a second column, per row | ~0 |
| `ORDER BY` and `LIMIT 20`, per row | 57 ns |

`COUNT(*)` streams its rows rather than collecting them, but the batch it
streams from still owns an id string and a row pair for each, so that first
line is what reaching a row *and handing it to the executor* costs — not the
directory lookup on its own. The same table answers `SELECT count(*)` over all
20 000 rows at **85 ns a row**, paying the same materialization, when the rows
are visited in order instead of by key. So:

**Reaching a row by key costs 4.8 times what reaching it in order costs**, on
the same table, the same rows and the same data. In SQLite the two are close,
because a secondary index stores rowids that address the table b-tree
directly. Here an index stores a physical id, and turning that id into a
visible version is an independent search of every persisted run and of the
resident overlay.

**Finding a row costs ten times what reading it costs.** That is the single
most useful number of the pass, and it says something uncomfortable about the
plan it came from: goal items 1 and 5, the row representation and the payload
without column names, both act on the 47 ns, not on the 415. They were worth
doing — the per-column term went from 52 to 18 ns and a full scan of eight
columns from 7.87 to 5.00 ms — but no amount of work there could have closed
this gap, because a category page decodes four columns and resolves 339 rows.

The 408 ns splits by where the rows live. The same forty rows, in a database
that was never checkpointed, cost **250 ns** each; published into a run, they
cost 408. So about 158 ns is the run lookup (a bloom probe, a binary search of
the page directory, and parsing into the page) and 250 ns is the floor that
memory alone still costs.

A profile of that floor, looped in isolation, says what it is made of, and it
is not what the three previous hypotheses assumed. (The figures below are from
before hypothesis 41, which cut the allocations of a matched row by two
thirds; the *shape* of the finding is what mattered, and it held.)

| | share of the samples |
|---|---:|
| **malloc and free** | **58 %** |
| `memcmp` | 13 % |
| everything else | 29 % |

Allocation, not searching and not comparing. Attributed to its callers, it is
spread between the per-row work (`SecIdx::ids_batch`, `find_eq_batch_version`)
and the *per-statement* setup that every execution repeats: the parsed
statement is cloned, the whole `TableSchema` is cloned out of the catalog, and
the projection plan and its column list are rebuilt. The obvious per-row
suspect turned out not to be one: removing the `String` id that the executor
discards for every row but the last measured slightly worse (hypothesis 39).

So the floor is allocation, and it is worth attacking as allocation — which
is what found hypothesis 41, by counting allocations per matched row instead
of sampling a profile. Counting is what made it visible: every projection that
excluded the declared `id` allocated two to three times what one that included
it did, which no amount of staring at a sampled profile had shown. The floor
is now 136 ns a row from the resident overlay, down from 250.

The schema half of the per-statement cost is done too (hypothesis 40): sharing it is 6.7 % of a point
read and 17 % of an `UPDATE` by id, and invisible in the mix, which says the
per-statement setup is not what the weighted operation is made of. What
remains on that axis is the parsed statement and the plan, which are rebuilt
per execution, and a row batch that reuses its buffers. That is the shape of the remaining work, and it
is a different shape from "make the lookup faster", which five hypotheses have
now shown gives a fifth at most.

Goal item 2, the physical key being the declared identity, removes all three —
but the measurement also says it is aimed at the wrong operations. Weighted by
the mix, four operations are 128 µs of the 158, and all four read many rows:

| operation | share of calls | share of the time |
|---|---:|---:|
| browse | 20.4 % | 45 µs |
| order_history | 3.0 % | 36 µs |
| search_text | 8.1 % | 25 µs |
| admin_dashboard | 1.0 % | 22 µs |
| product_detail | **17.4 %** | **3.5 µs** |
| session_check | 12.2 % | 3.2 µs |

`product_detail` is the `WHERE id = ?` lookup that goal item 2 makes one hop
instead of two. It is the second most frequent operation of the mix and two
per cent of its time. Removing one hop from it, at roughly 0.4 µs a hop,
moves the weighted operation by well under one per cent. The ordering in the
plan was by how often an operation runs; the measurement orders by how long
it takes, and the two disagree.

What would move it is the 409 ns, applied to the hundreds of rows `browse`,
`search_text` and `admin_dashboard` each touch. That means the index storing
something that addresses the row directly, rather than a key that has to be
searched for again — which is the physical-key change *and* a change to what
a secondary index stores.

### Is SQLite winning because it keeps everything in memory?

Two questions worth answering with numbers, because both have an intuitive
answer that the measurement contradicts.

**Does SQLite win on concurrency?** No, and the tail says so. At 50, 200 and
500 in-flight requests EliteSQL's p99 is better than SQLite's — 21 against 45
ms, 185 against 265, 960 against 1 014. Concurrency is where a design pays for
its contention, and that is where EliteSQL is ahead. What SQLite wins is the
cost of one operation with nobody else running: 72 µs against 158. At these
levels the workload is CPU-bound, so throughput is roughly cores divided by
that cost. SQLite wins the sweep by being cheaper per operation, not by
handling concurrency better. The one place concurrency is the answer is 500
users, where the commit mutex is EliteSQL's own wall.

**Is it because SQLite works in memory and EliteSQL does not?** Both work in
memory: SQLite's database is 9 MiB and lives in the operating system's page
cache, EliteSQL's runs are mmap'd. Neither is I/O-bound at this size, and no
profile of either shows a read in the hot path. SQLite is also given *less*
memory than EliteSQL, not more: its default page cache is 2 MiB per
connection, against EliteSQL's 384 MiB envelope with 64 MiB reserved for
queries.

**But the memory budget does bound EliteSQL at high concurrency**, which is
worth knowing on its own. Scaling the envelope, same workload, each engine
seeded from scratch, 20 s per level:

| users | 384 MiB (default) | 768 MiB | 1 536 MiB | SQLite | ratio, default → 1.5 GiB |
|---:|---:|---:|---:|---:|---|
| 10 | 12 813 | 11 811 | 12 593 | 18 776 | 0.68 → 0.67 |
| 50 | 11 872 | 10 488 | 11 529 | 16 736 | 0.71 → 0.69 |
| 200 | 8 844 | 9 354 | 9 946 | 16 231 | **0.54 → 0.61** |
| 500 | 6 481 | 7 602 | 8 749 | 15 912 | **0.41 → 0.55** |

At 500 in-flight requests four times the memory is **35 % more throughput**,
the p99 drops from 1 075 to 927 ms (below SQLite's 1 014), and the
query-admission timeouts disappear. At 10 and 50 users it changes nothing: the
pool is not what bounds those. Eight times the memory is worse than four, so
this is a budget that has an optimum rather than a direction.

Which budget matters, isolated by scaling one at a time by four at 500 users:

| scaled | throughput |
|---|---:|
| nothing (default) | 6 481 |
| the index and maintenance deltas | 6 891 |
| **the query pool** | **9 186** |
| everything | 8 749 |

It is the query pool, the one the per-statement admission governor draws
from. With hundreds of statements in flight and 64 MiB to share, statements
queue for a reservation before they queue for anything else.

Two follow-up questions, both answered by ablation:

- **Is the governor's own lock the cost?** Every statement takes one mutex to
  reserve. Single-threaded it costs nothing measurable (a point read is 1.85
  µs with it and 1.97 without, which is noise). At 500 users, handing out
  permits without touching the governor gives 7 172 ops/s — inside the
  run-to-run variance of the 6 500–7 000 the default already gets. **The lock
  is not the bottleneck; the limit is.**
- **Would no limit at all be better?** No, and this is the useful part.
  Unlimited admission (7 172) is *worse* than a four-times-larger limit
  (8 749–9 186). Letting every statement run at once is slower than letting
  four times as many run as today. Admission control is doing its job; its
  default is simply tuned tighter than this machine wants.

The natural follow-up, not done here because it was measured on one machine
only: scale the default query pool with the core count rather than fixing it
at 64 MiB, since what it bounds is concurrent statements.

The throughput is not free: success falls from 99.3 % to 98.6 %, because more
operations getting through means more of them collide on the twenty hot
products and exhaust their optimistic retries. That is the same trade the rest
of this document describes, arriving from a new direction.

Acting on this, the **default query pool now scales with the core count**
rather than sitting at 64 MiB: 24 MiB per core, between 64 and 512 MiB
(hypothesis 38). On the ten-core reference machine that is 240 MiB, and it
buys 13 % at 500 in-flight requests for no extra resident memory, at the cost
of 3.5 % at 50. `elitesql serve --memory-mib <n>` scales the whole profile
explicitly, and `sweep.py --memory-mib <n>` passes it through.

### One connection per user is what the high-concurrency numbers were measuring

The harness gives each virtual user its own connection, which both engines
receive equally: 500 users, 500 connections, 10 client processes. But the two
architectures pay for a connection very differently. A SQLite connection is an
object inside the client process. An EliteSQL sidecar connection is a thread in
the server, and 500 threads on ten cores is a thundering herd on every lock the
engine has.

The same level, the same engine, the only change being how many connections the
client pool keeps open:

| connections at 500 users | ops/s | p99 of an operation | p99 a user sees | server cores | success |
|---|---:|---:|---:|---:|---:|
| 500 (one per user) | 7 268 | 982 ms | 982 ms | 7.49 | 99.21 % |
| 128 | 11 145 | 65 ms | — | 4.69 | 99.92 % |
| 64 | 10 995 | 31 ms | — | 4.39 | 100 % |
| **32** | **11 797** | **15.8 ms** | **107.5 ms** | **4.45** | **100 %** |

The two latency columns matter, and the first one alone would overstate the
result. A pooled client makes a user wait for a free connection before it can
send anything, and that wait is not part of an operation's latency; the
harness reports it separately and adds it back as the user-visible figure.
**Thirty-two connections is 62 % more throughput, nine times better
user-visible tail latency, forty per cent less CPU and no failures at all.**
Pooling is what every real application does; one connection per concurrent
user is not a configuration anyone deploys.

With a pooled client and the memory envelope raised, across the whole sweep:

| users | default config | pooled + 1.5 GiB | SQLite | ratio | p99 | SQLite p99 | success |
|---:|---:|---:|---:|---:|---:|---:|---:|
| 10 | 13 327 | 12 176 | 18 776 | 0.65 | 8.0 ms | 4.7 ms | 100 % |
| 50 | 12 383 | 11 151 | 16 736 | 0.67 | 17.3 ms | 44.7 ms | 100 % |
| 200 | 9 870 | 10 442 | 16 231 | 0.64 | 17.9 ms | 265.4 ms | 100 % |
| 500 | 7 255 | 10 476 | 15 912 | **0.66** | **17.8 ms** | 1 013.6 ms | **100 %** |

The ratio stops collapsing and goes flat at 0.64–0.67 at every level, the tail
becomes **57 times better than SQLite's** at 500 users, and the failures that
this document has described throughout — the exhausted optimistic retries on
the twenty hot products — disappear entirely, because far fewer writers are in
flight at once to collide. The server settles at 4.5 of ten cores instead of
climbing to 9.

**Bounding concurrency inside the engine is not a substitute**, and the earlier
negative stands on the current build: `--max-concurrent-statements` at 16, 32
and 64 gives 6 775, 6 337 and 5 794 ops/s against 7 268 unbounded, with p50
rising from 1.2 to 13–21 ms. The queue has to form before a request reaches the
server. Once five hundred threads have each read a request, parsed it and
allocated for it, making them wait is too late.

What this says about the engine, rather than about the benchmark: the sidecar
is thread-per-connection, and it has no way to make a client queue other than
refusing it (`--max-connections`, default 128).

**Making the server queue instead of the client was tried and does not work.**
A first-in-first-out execution gate in the sidecar, taken around each request
before it is dispatched — direct hand-off to the next waiter, so a release
wakes exactly one thread — admitting four requests per core: 7 703 ops/s
against 7 268 ungated, but p50 from 1.2 to 13.8 ms and the user-visible p99
from 960 to 2 175 ms. Server CPU did fall, 7.49 to 5.39 cores, so the gate
does reduce contention; it just cannot undo the five hundred threads, and a
queue inside the server is one the client cannot adapt to. Reverted, and it
joins `--max-concurrent-statements` as the second measured negative for
bounding concurrency after a request has arrived.

The benefit therefore does not come from limiting execution. It comes from the
five hundred threads never existing. Getting that without asking the operator
to pool means the sidecar not spending a thread per connection at all — an
event loop, or a small pool over non-blocking sockets. That is the clearest
architectural item the measurements produced, and it is a rewrite of the
server's I/O, not an adjustment to it.

### How much of the gap is the engine and how much is the way it is called

Every per-operation figure in this document is an engine plus a way of calling
it, and EliteSQL's Python binding (`ctypes` over a C ABI, JSON on the results)
is not the same kind of thing as `sqlite3` (a native CPython extension). It is
worth knowing which of the two the gap is in.

`browse` — the operation carrying the biggest share of it — measured at every
layer, same 5 000 products, same statement, 339 rows read and 20 returned,
with the rows published into a run:

| | EliteSQL | SQLite |
|---|---:|---:|
| the engine alone, in Rust | **138.6 µs** | — |
| across the JSON the C ABI returns | 140.4 µs | — |
| a raw call from Python | 150.0 µs | **44.1 µs** |
| over the sidecar, a socket round trip per statement | 168.0 µs | — |
| through the harness's service layer | 218 µs | 83 µs |

**The engine is 92 % of the raw call from Python.** The JSON boundary adds
1.8 µs, the binding above it 9.6, the sidecar 29.4 — real, but not the gap.
SQLite's entire call, engine and driver together, is 44.1 µs against our
engine's 138.6. Engine to engine, `browse` is more than three times.

A point read tells the same story from the other end: the engine 1.66 µs, from
Python 4.9, over the sidecar 13.0, against SQLite's whole call at 1.2. There
the per-call overheads dominate, because there is almost no work to do.

**This correction matters, and it is recorded because the first version of it
was wrong.** That version put the binding at 52 µs and concluded it was "an
independent factor of the same order as the engine". It was comparing a
checkpointed database against one whose rows were still in the resident
overlay — 138.6 µs against 95.2 for the same statement, a 46 % difference that
has nothing to do with bindings. The lesson is the one this whole section is
about: two measurements are only comparable when everything except the thing
under test is the same, and "how much data has been published" is one of the
things that has to be the same.

### Are we simply inefficient, while SQLite takes it easy?

Inefficient yes; taking it easy, no. At 500 in-flight requests SQLite uses
7.47 of the machine's 10 cores and EliteSQL 9.06. Both are near saturation.
What differs is what a core buys:

| users | EliteSQL ops per core | SQLite ops per core | cores used (EliteSQL / SQLite) |
|---:|---:|---:|---|
| 10 | 2 397 | 4 994 | 5.56 / 3.76 |
| 50 | 2 030 | 2 748 | 6.10 / 6.09 |
| 200 | 1 332 | 2 415 | 7.41 / 6.72 |
| 500 | 801 | 2 130 | 9.06 / 7.47 |

Two separate problems, and the second is the larger surprise:

1. **A single operation costs 2.2x more**: 158 µs against 72 with nobody else
   running. That is the per-row cost the rest of this document is about.
2. **The cost per operation grows with concurrency.** Measured directly by
   sweeping the level with the server's CPU sampled:

| in-flight requests | ops/s | server cores | CPU µs per operation |
|---:|---:|---:|---:|
| 1 | 4 762 | 0.70 | 147 |
| 2 | 7 857 | 1.39 | 177 |
| 4 | 11 479 | 2.39 | 208 |
| 8 | 13 104 | 3.70 | 282 |
| 16 | 13 112 | 4.34 | 331 |
| 32 | 12 753 | 4.58 | 359 |
| 64 | 11 950 | 4.66 | 390 |

**The same operation burns 2.65 times the CPU at 64 concurrent requests that
it burns alone**, and the server stops growing past 4.7 of 10 cores. Blocked
threads do not burn CPU; the burning is the waking — futex sleeps and wakes on
the locks every statement crosses, and the cache lines they bounce between
cores.

A `sample` of the server at 16 users says which locks, by where the threads
are blocked:

| blocked on | samples |
|---|---:|
| the commit mutex | 26 200 |
| the query-memory governor's admission | 17 600 |
| the global snapshot registry | 4 000 |

The first is known and documented above. The second is the finding of the
memory section: raising the pool is what removes it. The third had not been
looked at: `Db::snapshot` takes one global mutex per statement and, while
holding it, takes the state read lock; `Snapshot::drop` takes it again, and
sometimes a second mutex after that. Every statement of a 76 %-read workload
crosses it twice.

Making that registry cheap is the clearest remaining lever on the throughput
ratio, and it is not a micro-optimization: it is three global locks per
statement, which is what both caps the engine at 4.7 cores and inflates its
CPU. It was not attempted here because the lock order — snapshot registry
before state, at ten sites — is deliberate: the registry is what keeps a
compaction from collecting segments a statement is about to read. Inverting it
is a refactor with a deadlock to get right, not an adjustment, and the tree was
green.

### What is left, after searching and allocation were both ruled out

Three hypotheses attacked the *search* for a row — a merged pass over sorted
ids (32), a position hint carried between lookups (37), and a table of entry
offsets inside each page (43). All three measured negative or neutral.
Counting allocations bounded the other candidate: they are 18 % of `browse`,
and even an EliteSQL that allocated nothing would run it in about 180 µs
against SQLite's 83.

What remains is a property of the shape, and it can be counted rather than
argued. Resolving one row by a secondary key costs:

| | random accesses |
|---|---|
| SQLite | index b-tree → rowid → **table b-tree holds the row** |
| EliteSQL | index run → physical id → **primary MVCC directory** → segment and offset → payload |

**Three indirections against two**, and the third is not optional: the
directory is what makes a row multi-version, which is what buys the write
concurrency this workload's tail latency shows. `Db::get`, which is the last
two of those three with the id already in hand, costs 200 ns; a row reached by
key costs 294 ns published and 140 ns while it is still in the resident
overlay, and the 154 ns between those two is the run traversal that publishing
adds.

Closing that means the index storing something that addresses the row rather
than a key that has to be looked up again — which is goal items 2 and 3
together, and a change to what a secondary index holds as well as to what a
physical key is. Everything short of it has now been measured.

**What blocks the direct version, precisely.** The obvious shape is for a
secondary run to store where the row is — version, segment, offset — beside
the id it already stores, since the writer that builds it walks the primary
directory and has all of that in hand. A reader could then check the resident
overlays, which is the cheap half, and go straight to the payload.

It does not work as the pipeline stands, and the reason is worth writing down
because it is not obvious from the read path. Derived runs are published by
`schedule_frozen_derived` and primary runs by the checkpoint, on separate
schedules. So a row's newest version can be in primary run *N+1* while the
secondary run still describes generation *N*, and nothing in the secondary
entry says so. A reader trusting the stored location would return a superseded
row: not a stale read in the MVCC sense, which is legal, but the wrong version
for its snapshot, which is not.

Making the location authoritative therefore means **publishing the primary run
and the derived runs of a generation atomically**, so that "newer than this
generation" always means "in the resident overlays". That is a change to the
maintenance pipeline and to what a crash may interrupt, not to the read path.

**That is not enough either, and the second reason closes the idea.** Suppose
the two were published together, so that a miss in the resident overlays
proved no version had been committed since. The location stored beside an id
is only as fresh as the last time that id's *index entry* was written, and an
update that does not touch the indexed column does not write one. Hypothesis
57 measured exactly that: the same update costs 11.9, 11.7, 11.6 and 11.5 µs
on tables carrying zero, one, three and five secondary indexes, because an
index whose column did not change is left alone. So a row can move to a new
version, be drained into a primary run, and leave every secondary entry that
points at it describing where it used to be, with nothing in the overlays to
say so.

Keeping the location true would mean rewriting every secondary entry of a row
on every write of that row, which is to say making a write proportional to how
many indexes its table carries. That is the cost the engine does not currently
pay and the concurrent-write advantage the goal asks to keep. The two goals
are in direct opposition here, so this is not a sequencing problem to be
solved later; it is a trade, and the measurement above is its price.

There is also an intermediate worth evaluating, and it is safe by construction
rather than by an argument about coherence. **A run is immutable.**
For a given id, the version list the runs hold cannot change while the run set
does not, and the run set changes only when a checkpoint or compaction
publishes, which already bumps a generation. So a cache of
`id → version list from the runs`, tagged with that generation, is correct
without consulting the change log: every read still looks in the resident
overlays, which is the cheap half, and skips the run traversal, which is the
154 ns publishing adds.

Making that tag impossible to forget is also a solved problem: put the run set
behind a type in its own module whose only constructor assigns a fresh epoch
from a global counter, so changing the runs and forgetting the epoch cannot
both happen. Today there is no such tag — `runs` is pushed to in three places
that do not touch `generation` — which is why the cache cannot simply be
bolted on.

What it would buy depends on locality across requests, which this workload has:
`browse` re-reads the same category's rows constantly. If the traversal went
away entirely, `browse` would fall from 134 to about 85 µs and the weighted
operation from 157 to roughly 140 — **useful, and still not parity**.

The sizing is what argues against it. The cache has to be per table, because an
operation touches six of them and a single-slot cache would be cleared six
times an operation. Per thread, five hundred sidecar connections mean five
hundred copies: capping each at 512 entries costs about 12 MB and buys a
partial hit rate, well short of the 100 % the estimate above assumes. Shared,
it puts a lock back on the read path, which measured negative three times in
this pass. With a partial hit rate the realistic gain is four to six per cent,
for a caching subsystem of a few hundred lines whose memory grows with the
connection count. That is the trade, written down so it can be decided rather
than rediscovered.

### Counting allocations instead of sampling a profile

Everything in hypotheses 39 to 42 came from one change of method. A sampled
profile says *that* a path allocates. Counting says how many times, and a
count can be compared between two statements that differ in one way.

`statement_cost` now installs a counting global allocator and reports
allocations per statement, and per matched row by measuring the same statement
shape twice — once matching one row through a unique index, once matching
forty through a non-unique one — so the subtraction removes that shape's own
fixed cost. The first version of that ablation subtracted a *different*
statement's fixed cost and had to be thrown away.

What it shows, after hypothesis 41:

| | allocations |
|---|---:|
| `Db::get`, the storage primitive | 3 |
| the same row through SQL | 46 |
| an `INSERT`, autocommit | 128 |
| an `INSERT` staged in a transaction of 100 | 57 |
| per matched row, `COUNT(*)` | 3.4 |
| per matched row, one integer column | 4.5 |
| per matched row, the text column | 6.5 |

Where those 43 are, by varying one element of the statement at a time:

| statement | allocations |
|---|---:|
| `SELECT price FROM p LIMIT 1` | 37 |
| `… WHERE id = 7` | 46 |
| `SELECT id, price … WHERE id = 7` | 50 |
| `SELECT id, name, price, bucket … WHERE id = 7` | 62 |
| `… WHERE id = 7 ORDER BY price` | 57 |
| `… WHERE id = 7 AND price >= 0` | 53 |

**Thirty-seven is the floor of the simplest SELECT there is**, and each element
adds four to eleven. A projected column costs about five, three of which are
the same column name allocated in three places: the cloned parse tree, the
plan's headers and the `Vec<String>` of column names in `QueryOutput`.

Counted by phase, with a counting allocator installed in the library's own
test binary so the phases are the engine's internal ones:

| phase of `SELECT price FROM p LIMIT 1` | allocations |
|---|---:|
| `parse_cached` — the cached tree cloned so parameters can be bound into it | 4 |
| `bind_statement` | **0** |
| `resolve_query` | 3 |
| `exec_select_resolved` | 24 |

So the prepared-statement redesign — placeholders that survive planning,
resolved against a parameter slice at evaluation, and a cached plan — is worth
the parse clone and the resolve: **7 of 31**. It is not where a statement's
allocations are, which is the opposite of what the sampled profile suggested
and is why it is recorded here rather than attempted.

**And this closes the allocation line altogether.** A statement that matches no
row at all costs 32 allocations; each matched row costs 4.5. `browse`, the
operation that carries the largest share of the gap, reads 339 rows, so it
allocates 32 + 1 525 ≈ 1 558 times. At roughly 25 ns for an allocation and its
free, that is about 39 µs of its 218 — **eighteen per cent**. An EliteSQL that
allocated nothing at all would run `browse` in about 179 µs against SQLite's
83. Allocation was worth chasing, and hypothesis 41 found a real defect by
counting it, but it is not the remaining 2.6x. What is left is computation:
the directory search itself, decoding, and the layers above the engine.

### The cost of a matched row does not depend on the size of the table

The last experiment of the pass settles what the 409 ns is, and it is not what
four of the hypotheses assumed. The same forty matched rows, resolved from the
resident overlay:

| table | per matched row |
|---|---:|
| 20 000 rows | 250 ns |
| 200 rows | 247 ns |

**A hundredfold smaller table costs the same per row**, to within one per cent. Every search involved —
the overlay's binary search, the page directory's, the run's range check — is
logarithmic in the table, so if searching were the cost the two numbers would
differ by a factor of two. They do not. The cost is per-row work that the size
of the table does not touch: an id `String` produced by the secondary index, a
version entry cloned out of the overlay, and a `(String, Record)` pair pushed
into the batch the statement returns — which `count(*)` pays in full even
though it decodes nothing.

That is why hypotheses 32 and 37, both of which reorganized *searching*,
measured negative and neutral, and why hypotheses 31, 33, 34, 35 and 36, which
made searching cheaper, gave a fifth rather than a half. A sequential scan
reaches a row for 85 ns and performs the same materialization, so the ~165 ns
an indexed row costs above it is the id being produced by one structure and
then looked up in another.

Removing it means not materializing a batch of owned rows at all: streaming a
statement's rows instead of collecting `Vec<(String, Record)>`, and an id that
is not a heap string. Both are API changes, not internal ones.

**Both were then made** (hypotheses 47, 50, 51 and 52). A batch is now a
`ScanBatch`: the rows, one cursor, and ids only for the callers that read
them. Ids live in byte arenas inside the readers instead of one `String` per
row, and a projection moves values out of the row rather than copying them.
Reaching a row sequentially went from 85 to **65 ns**, a matched row from 4.46
to **2.77** allocations, `COUNT(*)` over 20 000 rows from 1 550 to **1 290 µs**,
and the shop's `browse` page from 136 to **119 µs**. The paragraph above was
right about the cause and right that it was not a search problem.

### What the four changes to the keyed lookup added up to

Hypotheses 31, 33, 34, 35 and 36 all attack the 409 ns, from different sides:
the page directory, the page size, the delta's comparison, the per-row table
resolution, and the run's range check. Two of them measured neutral. Together
the indexed statement went from **21.8 to 16.7 µs**, a fifth off, and the
per-column term of a scan from 52 to 18 ns.

One idea was tested and rejected on principle rather than on a number: for a
candidate set that is a small fraction of the table, a merged ordered walk
cannot beat one search per key. At browse's selectivity, about 7 %, the
average gap between two wanted rows is fifteen rows, and walking fifteen rows
at 85 ns costs more than one 409 ns search. A merged walk only wins when the
candidate set approaches the whole table, and then the planner would choose a
scan anyway. That is why hypothesis 32 was negative and why no variant of it
will be positive.

### Where this leaves the stated target

The goal of this pass was set as "below 60 µs per weighted operation and 90 %
of SQLite in the sweep". Neither is met, and the gap between the goal's
starting numbers and the harness's is worth recording so the next pass starts
from the measurement rather than from the target.

The goal quoted 132 µs per operation against SQLite's 38. Those are not
figures this harness produces for a weighted operation: a one-user run of the
same mix reports 206.6 µs over the sidecar, 169.4 µs embedded, and 72.0 µs for
SQLite. The 132/38 pair is the older per-statement figure, not per operation,
and an operation is 1–12 statements. Against the numbers the harness actually
reports, the pass moved the weighted operation from 220.1 to 206.6 µs over the
sidecar, the embedded one from 189.4 to 158.4, and the throughput ratio from
0.55–0.65 to 0.64–0.77 up to 200 in-flight requests. SQLite is still between
1.3x and 2.1x faster on this workload at every level.

What remains is concentrated and known:

1. **`browse` is a fifth of the mix at 2.7x** and one third of the remaining
   gap. It reads 339 rows of a category through the secondary index, sorts
   them by price and returns 20, and its cost is the primary directory
   lookup repeated per candidate row. Two attacks on that lookup have been
   measured: smaller pages (hypothesis 33, helps only once the data is in
   persisted runs) and inline id prefixes in the resident delta (hypothesis
   34, 4 % of the weighted operation). Neither touches the structure itself.
   The structural change is goal item 2, the physical key being the declared
   identity: with an integer key the delta search compares integers instead
   of twenty-six-byte strings, and a row costs no `String` at all. It is a
   deep change — physical ids are `String` throughout the engine — and it is
   the one the evidence now points at.
2. **The commit mutex still bounds the 500-user level.** It is down from 317
   to 254 µs of hold per commit, but grouped application (goal item 4) is
   untouched.
3. **The sidecar protocol is 30.7 µs per operation**, 15 % of the total. It is
   not the gap, but it is the largest single item that is not the engine.

### Working notes on this pass

`crates/elitesql-core/src/record.rs` was not written in the session that
finished it; it arrived with the tree not compiling and 21 call sites
unconverted, and was completed rather than discarded.

One experiment is worth recording as a hazard, not a result: removing the
per-row id allocation in `shared_scan_batch_at_bytes_filtered` by pushing an
empty `String` hangs the engine. Callers use the last id of a batch as the
continuation cursor, and an empty cursor restarts the scan from the beginning.
Hypothesis 28 removes that copy safely by taking the cursor from the row the
batch already owns.

The mixed sweep (one growing database, 10/100/500 users, 25 s) stays within
its noise across the pass, with invariants `ok` at every level and the offline
integrity check clean after both a clean close and a `kill -9`.

### What the per-operation split says about the rest of the gap

At one user, with no contention, over the same seeded database:

| | weighted operation | vs SQLite |
|---|---:|---:|
| EliteSQL over the sidecar | 206.6 µs | 2.85x |
| EliteSQL embedded, no protocol | 158.4 µs | **2.20x** |
| SQLite, in process | 71.9 µs | |

The JSON sidecar protocol costs **30.7 µs per operation**, 15 % of the total.
That is real but it is not the gap: an in-process EliteSQL is still 2.4x. The
remaining cost is concentrated in two operations, both of them reads that
touch many rows:

| operation | share | EliteSQL embedded | SQLite | ratio |
|---|---:|---:|---:|---:|
| browse (index equality, sort, limit 20) | 20.4 % | 221 µs | 83 µs | 2.7x |
| search_text (BM25 top 10) | 8.1 % | 316 µs | 101 µs | 3.1x |
| admin_dashboard (full scan) | 1.0 % | 2 180 µs | 859 µs | 2.5x |
| order_history (join, sort) | 3.0 % | 1 170 µs | 879 µs | 1.3x |
| every write operation | ~25 % | 20–107 µs | 6–49 µs | 1.4–2.9x |

`browse` alone is a fifth of the mix and a third of the remaining gap.

## What the simulation found in the engine

The numbers above are dominated by four engine behaviours that the harness
surfaced and that were then confirmed in isolation with micro-tests
(`bindings/python`, embedded, `balanced`):

1. **SQL inside an explicit transaction does not use indexes.** A point
   `SELECT … WHERE id = ?` costs 7–13 µs in autocommit but 0.7 ms inside
   `db.transaction()` on a 2 000-row table and 6.5 ms on 20 000 rows — linear
   in table size; the same for a secondary-index predicate and for `UPDATE`.
   A `sample` profile of 20 threads running read-only transactions shows the
   time in `Txn::scan → shared_scan_batch_at_bytes`, decoding every record.
   Every multi-statement operation of the shop (add to cart, checkout, login,
   review) runs 4–12 statements in a transaction over tables of 5 000–20 000+
   rows, so the engine spends 7–9 cores scanning and the whole curve is
   CPU-bound from 10 users on. This is the single largest lever: with
   indexed transactional reads the workload should run one to two orders of
   magnitude faster.
2. **Hot rows degrade with their version count.** Updating one row 10 000
   times takes it from 15 µs to ~330 µs per update and 7 µs to ~75 µs per
   read, growing linearly until a compaction/promotion resets it; checkpoints
   do not help. Counters such as `products.sold`, `rating_count` or
   `users.login_count` follow this pattern in real applications.
3. **Query memory admission is the capacity ceiling.** With 1 000 requests in
   flight 2 % of operations fail, and with 2 000 about half fail, all with
   code 16 *"query memory admission timed out"* (default 64 MiB query pool,
   5 s `query_admission_timeout_ms`). Reads suffer first — `browse` and
   `update_profile` p99 sit exactly at 10 s and `search_text` at 5 s, i.e.
   at the timeouts — while transactional writes do not pass through the same
   admission and keep a p99 of 2–4 s. Nothing was lost or corrupted:
   invariants and the offline check passed at every level.
4. **Connection storms and shutdown.** The sidecar listens with the Rust
   default backlog (128): opening 1 000+ connections at once gets some
   refused with `ECONNREFUSED` (5–30 per level here; the generator now
   retries with backoff, as a pool would). Each server connection costs a
   thread and roughly 1.5–2 MiB of RSS at this workload (2.9 GiB at 2 000
   connections, 4.4 GiB peak). `elitesql serve` has no signal handler, so
   SIGTERM is an unclean stop: `elitesql check` then reports one warning per
   row not yet reflected in the derived indexes (234 316 here, 0 errors) and
   passes clean after one open/close.

Harness-side lessons that are now part of the tool: FIFO connection hand-off
(under the GIL the releasing thread otherwise re-takes its connection and the
queue never shows in the percentiles); a business refusal inside a
transaction must roll back, not commit (the first SQLite run caught
`sold != order lines` because an out-of-stock checkout committed the stock
decrements of the earlier lines); and macOS's 4 096-threads-per-process cap,
which makes a connection pool mandatory beyond ~3 500 users.
