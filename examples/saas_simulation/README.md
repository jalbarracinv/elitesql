# Mini-SaaS concurrency simulation

A load test shaped like a real application instead of a micro-benchmark: a
small e-commerce SaaS backend (accounts, sessions, catalogue, full-text and
vector search, persistent carts, checkout, reviews, an admin dashboard) runs
against EliteSQL while 10 to 5 000 *virtual users* each perform a different mix
of operations. Every level of concurrency yields one point of the capacity and
performance curves; the sweep also verifies business invariants, read-your-
writes consistency and the on-disk integrity of the database.

```bash
# 1. build the engine and the server once
cargo build --release -p elitesql-ffi -p elitesql-cli

# 2. EliteSQL through `elitesql serve` (default; the only way past ~3 500 users on macOS)
python3 examples/saas_simulation/sweep.py --transport sidecar \
    --levels 10,100,200,500,1000,2000,3000,4000,5000 --duration 60

# 3. the same workload on SQLite, for context
python3 examples/saas_simulation/sweep.py --transport sqlite --levels 10,100,200,500,1000,2000,3000,4000,5000

# 4. overlay several runs in one set of charts
python3 examples/saas_simulation/plot.py results/sidecar-… results/sqlite-… --out results/compare

# 5. what one operation costs on each engine, nothing else running
python3 examples/saas_simulation/ops_cost.py
# The same mix with products(category, price_cents) added to both engines.
python3 examples/saas_simulation/ops_cost.py --scenario compound-index
```

`ops_cost.py` is the counterpart of the sweep: it seeds both engines in the
same process, checkpoints, and times one operation at a time. By default its
`full-v2` metric uses the same sixteen weighted operations as the virtual-user
generator and normalizes their 98.3 total weight. Its `historical-v1` option
reproduces the older eleven-operation, weights-over-100 calculation solely for
comparison with earlier reports; it is a partial mix, not an average request.
Each timed mutation receives an independently prepared account and cart, so a
sample does not change the state measured by the next one. The sweep says how
the engine behaves under concurrency; this says what the work itself costs,
which is where per-row and per-statement overhead shows up undiluted.

`--scenario baseline` is the original schema. `--scenario compound-index`
keeps every baseline index and adds `products(category, price_cents)` to both
engines before the checkpoint; it isolates the browse access path and records
the scenario in the JSON output. Compare matching scenarios from the same run,
not a compound result against an older baseline.

A 9-level sweep with the default 60 s per level takes about 15 minutes. Each
run writes `stages.csv` (one row per level), `report.md`, SVG charts and a
`stage-NNNNN/` directory per level with `summary.json`, `per-op.csv`,
`timeseries.csv` (per-second throughput and p99) and `resources.csv` (CPU and
RSS samples). Pass `--keep-samples` to also keep every request as a row of a
gzipped CSV.

## The application

`saas/schema.py` declares nine tables (`users`, `sessions`, `products` with a
32-dimensional embedding, `carts`, `cart_items`, `orders`, `order_items`,
`reviews`, `audit_log`) and eleven indexes, plus a deterministic seed: 20 000
accounts and 5 000 products by default, with a BM25 text index on descriptions
and an HNSW index on embeddings. The first twenty products start with 25 units
so checkouts compete for scarce stock.

`saas/service.py` implements the sixteen operations a virtual user can run.
Multi-statement writes are transactions retried on optimistic-commit conflicts
(`run_transaction`); checkout decrements stock with a guarded
`UPDATE … WHERE stock >= qty` so stock can never go negative.

| operation | weight | statements |
|---|---:|---|
| browse (category page, `ORDER BY price LIMIT/OFFSET`) | 20 | 1 read |
| product_detail (+ last 5 reviews) | 17 | 2 reads |
| session_check (auth middleware: token → user, touch last seen) | 12 | 1 join read + 1 update |
| add_to_cart (upsert line, touch cart) | 10 | transaction, 4–5 statements |
| search_text (BM25) | 8 | 1 text search |
| view_cart (`JOIN products`) | 8 | 2 reads |
| recommend (HNSW nearest neighbours of a product) | 7 | 1 read + 1 vector search |
| checkout (stock, order, lines, empty cart, loyalty credits, audit) | 4 | transaction, 7 + 2·lines statements |
| update_cart_item / remove | 3 | transaction, 3 statements |
| order_history (+ lines of the latest order) | 3 | 2 reads |
| write_review (+ rating counters on the product) | 2 | transaction, 2 statements |
| relogin (logout, login: session insert, login counter, audit) | 1.5 | transaction |
| update_profile | 1 | 1 update |
| admin_dashboard (`GROUP BY`, aggregates, low-stock count) | 1 | 3 reads |
| signup (user, cart, audit) | 0.5 | transaction |
| restock (admin refills 3 hot products) | 0.3 | transaction |

Roughly 76 % of operations are reads and 24 % write. Product popularity is
skewed: 15 % of picks go to the twenty "hot" products and 40 % to the most
popular 5 %, so some rows are genuinely contended.

`saas/drivers.py` adapts three back ends to one tiny interface so the same
service code and workload run on each: `embedded` (one thread-safe `EliteSQL`
handle per process), `sidecar` (`SidecarClient` connections to
`elitesql serve` over a Unix socket) and `sqlite` (`sqlite3`, WAL,
`synchronous=NORMAL` by default, FTS5 for text search; vector recommendations degrade to
"best sellers of the same category").

`--durability` applies to both engines: `fast` selects SQLite WAL/OFF,
`balanced` WAL/NORMAL, and `safe` WAL/FULL with `fullfsync` and
`checkpoint_fullfsync` enabled. On macOS those flags request F_FULLFSYNC,
matching EliteSQL Safe's requested durability barrier. Each SQLite connection
reads its settings back; stages retain them in `summary.json`. Balanced/NORMAL
do not provide the same loss-window contract as strict per-commit durability.

The read/write shares above classify application operations. `session_check`
is counted as a read operation but also updates the session; the mix is not
76% read-only requests. User p99 includes each request's connection-pool wait,
combined with its query time before percentile calculation.

For the strict-durability comparison, including a SQLite FIFO control and
recovery checks, see [the central-advantage report](../../docs/central-advantage.md).

## The load generator

A *virtual user* (`saas/vuser.py`) is a thread with its own account, session
token and a local model of its own cart. In a closed loop it picks the next
operation by weight, runs it, records the sample and optionally sleeps a think
time (`--think 0.5:2` for a uniform 0.5–2 s; the default `0` measures raw
capacity: N users means N requests in flight).

Threads are spread over several processes (`--processes`, default one per CPU)
so the Python GIL is not the first thing to saturate; each process owns a pool
of database connections. macOS caps a process at 4 096 threads
(`kern.num_taskthreads`) and the sidecar spends one server thread per
connection, so 5 000 concurrent users cannot each hold a connection on one
machine: `--connections` (default `min(users, 2000)`) bounds the requests in
flight, exactly as an application's connection pool would, and the wait for a
pooled connection is measured separately from the database call. The pool
hands connections over strictly FIFO; without that a thread that just released
a connection re-takes it under the GIL and the waiting users starve, which
would hide the queue from the percentiles.

Each level runs a ramp (`--ramp`, users start spread over these seconds), a
warm-up that is excluded, and the measured window (`--duration`). The
database accumulates across levels, as a production database would (`--fresh-
per-stage` reseeds instead), so the database-size column tells growth apart
from concurrency.

## What is measured

Per level, in `stages.csv` and `report.md`:

- **Throughput**: operations/s, split into reads and writes, and its
  per-second coefficient of variation plus the number of seconds with zero
  completed operations (stalls, e.g. during a checkpoint).
- **Latency** of the database call: mean, p50, p90, p95, p99, p99.9 and max,
  overall and per operation (`per-op.csv`), and a per-second series
  (`timeseries.csv`) to see hiccups that the aggregate hides.
- **User-perceived latency**: database call plus the wait for a pooled
  connection (`user p99`, `pool_wait p99`).
- **Correctness under load**: success rate; failures by error code;
  optimistic-commit conflicts retried and the share of operations that needed
  a retry; retries exhausted; sidecar reconnects; read-your-writes violations
  (a user's cart differing from what that user wrote); business outcomes
  (`out_of_stock`, `empty_cart`, …).
- **Business invariants** after every level: no negative stock; for every
  product `initial + restocked == stock + sold`; units sold == units in order
  lines == units in order headers; loyalty credits == 10 × paid orders.
- **Resources**: CPU cores and RSS of the server and of the load generators
  (a generator near 100 % of its processes means the measurement is of the
  generator, and the report says so), database size after each level.
- **Engine counters** (sidecar transport): the server's `{"op":"stats"}`
  counters are diffed around every level and stored in `summary.json`
  (`engine_stats`); `stages.csv` carries the per-commit mutex wait and hold,
  the wait for the state write lock and the query-admission waits.
- **Integrity**: at the end `elitesql check` runs twice — right after the
  server is killed with SIGTERM (crash consistency) and after one clean
  open/close — or `PRAGMA integrity_check` on SQLite.

The report derives a few figures from the curve: peak throughput and where it
occurs, the largest user count whose p99 stays under 10/50/100/250/1000 ms,
scaling efficiency, operations per server core-second, and a fit of Gunther's
Universal Scalability Law (σ = contention, κ = coherency, predicted peak).

## Options worth knowing

| flag | default | effect |
|---|---|---|
| `--transport` | `sidecar` | `sidecar`, `embedded` (≤ 3 500 users, one process) or `sqlite` |
| `--levels` | `10,…,5000` | concurrent users per level |
| `--duration`, `--warmup`, `--ramp` | 60, 5, 5 s | window per level |
| `--think` | `0` | seconds between one user's requests: `0`, `1` or `0.5:2` |
| `--connections` | `min(users, 2000)` | requests in flight (pool size across processes) |
| `--processes` | CPUs | load-generator processes |
| `--durability` | `balanced` | `safe`, `balanced`, `fast` (server and seed) |
| `--products`, `--accounts` | 5000, 20000 | seed size |
| `--scenario` | `baseline` | `compound-index` adds `(category, price_cents)` to both engines |
| `--fresh-per-stage` | off | reseed before every level |
| `--compare-with DIR` | – | overlay another run's curve in the charts |
| env `SAAS_SIM_DROP_OPS=op,op` | – | remove operations from the mix (e.g. `search_text,recommend`) to attribute a bottleneck |
| `--rebuild-report DIR [--reverify-db PATH]` | – | rewrite `stages.csv`, `report.md` and charts of a finished run, optionally re-running the invariants on its database |

Results of the reference runs live in `benchmark-results/saas-simulation-<date>/`.

For an isolated paired pagination measurement, use `browse_cost.py`. It checks
price sequences against SQLite, records both query plans, alternates engine
order, and saves warmed block timings for offsets 0–1000. Block timings are
average query costs, not request percentiles. Run before/after libraries in
separate processes, without simultaneous builds or other benchmarks:

```bash
ELITESQL_LIB="$PWD/target/release/libelitesql.dylib" \
  python3 examples/saas_simulation/browse_cost.py \
  --products 5000 --iterations 500 --repetitions 5 --out /tmp/browse.json
```

Choose the platform's library filename (`.so` on Linux). The report records
its SHA-256 and the exact SQL; both engines retain their baseline indexes and
add the same compound index. The seed is checkpointed before timing.
