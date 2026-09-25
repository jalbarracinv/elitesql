# Mini-SaaS concurrency simulation — sidecar

Run: `B-2` on 2026-09-23T13:09:08 · commit `92e0290` · macOS-26.6.2-arm64-arm-64bit · 10 CPUs

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
- **seed time**: 1.16 s, 13.1 MiB after seeding

## Capacity and performance curve

Latencies are of the database call (service operation) in milliseconds; `user p99` adds the wait for a pooled connection.

| users | conns | ops/s | reads/s | writes/s | p50 | p95 | p99 | p99.9 | max | user p99 | success % | conflict retry % | srv CPU | srv RSS MiB | gen CPU | thr CV | invariants |
|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|
| 100 | 100 | 17179.3 | 13293.5 | 3885.9 | 1.478 | 20.813 | 41.354 | 103.555 | 1075.74 | 41.356 | 99.988 | 0.725 | 5.22 | 156.5 | 2.22 | 0.076 | ok |

## Insights

- **Peak throughput**: 17179.3 ops/s at 100 users (p99 41.354 ms).
- **Capacity at p99 ≤ 10 ms**: 0 users (DB latency); 0 users counting pool wait.
- **Capacity at p99 ≤ 50 ms**: 100 users (DB latency); 100 users counting pool wait.
- **Capacity at p99 ≤ 100 ms**: 100 users (DB latency); 100 users counting pool wait.
- **Capacity at p99 ≤ 250 ms**: 100 users (DB latency); 100 users counting pool wait.
- **Capacity at p99 ≤ 1000 ms**: 100 users (DB latency); 100 users counting pool wait.
- **Ops per server core-second**: 100→3291.
- **Read-your-writes violations**: none.
- **Business invariants**: all stages ok.
- **Offline integrity check** (elitesql check): ok in 6.93 s.
  - right after killing the server (no clean close): exit 3, 0 errors, 221558 warnings; after one clean open/close: exit 0, 0 errors, 0 warnings.
- **Database size**: 67.0 MiB after the first stage, 67.0 MiB after the last; 10707 paid orders in total.

## p99 latency per operation (ms)

| op | share % | 100 |
|---|---:|---:|
| add_to_cart | 10.09 | 45.35 |
| admin_dashboard | 1.05 | 142.80 |
| browse | 20.48 | 27.94 |
| checkout | 4.1 | 119.24 |
| order_history | 3.06 | 34.33 |
| product_detail | 17.17 | 24.64 |
| recommend | 7.12 | 18.83 |
| relogin | 1.53 | 61.24 |
| restock | 0.31 | 82.53 |
| search_text | 8.16 | 16.97 |
| session_check | 12.14 | 39.64 |
| signup | 0.51 | 41.77 |
| update_cart_item | 3.03 | 37.68 |
| update_profile | 1.02 | 28.95 |
| view_cart | 8.2 | 27.91 |
| write_review | 2.03 | 37.76 |

## Errors and business outcomes at the highest level

```json
{
 "statuses": {
  "ok": 254452,
  "not_found": 18,
  "empty_cart": 3188,
  "conflict_exhausted": 32
 },
 "errors": {
  "conflict_exhausted": {
   "count": 42,
   "first": "[elitesql:9] conflict, retry transaction: products/c12 changed after this transaction began",
   "ops": {
    "checkout": 41,
    "restock": 1
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
