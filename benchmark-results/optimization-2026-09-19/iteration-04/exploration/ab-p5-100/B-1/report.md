# Mini-SaaS concurrency simulation — sidecar

Run: `B-1` on 2026-09-23T13:49:47 · commit `92e0290` · macOS-26.6.2-arm64-arm-64bit · 10 CPUs

## Configuration

- **transport**: sidecar
- **levels**: [100]
- **duration**: 15.0
- **warmup**: 3.0
- **ramp**: 3.0
- **think**: [0.0, 0.0]
- **processes**: 0
- **connections**: 0
- **durability**: balanced
- **products**: 5000
- **accounts**: 20000
- **scenario**: compound-index
- **seed**: 1
- **fresh_per_stage**: False
- **seed time**: 1.17 s, 13.1 MiB after seeding

## Capacity and performance curve

Latencies are of the database call (service operation) in milliseconds; `user p99` adds the wait for a pooled connection.

| users | conns | ops/s | reads/s | writes/s | p50 | p95 | p99 | p99.9 | max | user p99 | success % | conflict retry % | srv CPU | srv RSS MiB | gen CPU | thr CV | invariants |
|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|
| 100 | 100 | 18202.9 | 14081.5 | 4121.4 | 1.327 | 19.562 | 38.562 | 118.507 | 1142.813 | 38.565 | 99.987 | 0.701 | 5.1 | 177.3 | 2.41 | 0.136 | ok |

## Insights

- **Peak throughput**: 18202.9 ops/s at 100 users (p99 38.562 ms).
- **Capacity at p99 ≤ 10 ms**: 0 users (DB latency); 0 users counting pool wait.
- **Capacity at p99 ≤ 50 ms**: 100 users (DB latency); 100 users counting pool wait.
- **Capacity at p99 ≤ 100 ms**: 100 users (DB latency); 100 users counting pool wait.
- **Capacity at p99 ≤ 250 ms**: 100 users (DB latency); 100 users counting pool wait.
- **Capacity at p99 ≤ 1000 ms**: 100 users (DB latency); 100 users counting pool wait.
- **Ops per server core-second**: 100→3569.
- **Read-your-writes violations**: none.
- **Business invariants**: all stages ok.
- **Offline integrity check** (elitesql check): ok in 7.28 s.
  - right after killing the server (no clean close): exit 3, 0 errors, 235482 warnings; after one clean open/close: exit 0, 0 errors, 0 warnings.
- **Database size**: 72.2 MiB after the first stage, 72.2 MiB after the last; 11637 paid orders in total.

## p99 latency per operation (ms)

| op | share % | 100 |
|---|---:|---:|
| add_to_cart | 10.1 | 45.05 |
| admin_dashboard | 1.05 | 150.05 |
| browse | 20.48 | 13.82 |
| checkout | 4.1 | 96.81 |
| order_history | 3.04 | 30.23 |
| product_detail | 17.18 | 15.68 |
| recommend | 7.1 | 11.98 |
| relogin | 1.55 | 61.43 |
| restock | 0.31 | 111.34 |
| search_text | 8.15 | 18.02 |
| session_check | 12.19 | 38.98 |
| signup | 0.5 | 38.55 |
| update_cart_item | 3.04 | 36.84 |
| update_profile | 1.02 | 25.94 |
| view_cart | 8.18 | 24.54 |
| write_review | 2.02 | 31.34 |

## Errors and business outcomes at the highest level

```json
{
 "statuses": {
  "ok": 269610,
  "empty_cart": 3378,
  "conflict_exhausted": 36,
  "not_found": 19
 },
 "errors": {
  "conflict_exhausted": {
   "count": 46,
   "first": "[elitesql:9] conflict, retry transaction: products/b4 changed after this transaction began",
   "ops": {
    "checkout": 44,
    "restock": 2
   }
  }
 }
}
```

## Charts

![throughput](throughput.svg)
![latency](latency.svg)
![user-latency](user-latency.svg)
![errors](errors.svg)
![cpu](cpu.svg)
![efficiency](efficiency.svg)
![per-op-p99](per-op-p99.svg)
