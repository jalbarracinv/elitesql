# Mini-SaaS concurrency simulation — sidecar

Run: `A-1` on 2026-09-23T13:22:22 · commit `92e0290` · macOS-26.6.2-arm64-arm-64bit · 10 CPUs

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
- **seed time**: 1.15 s, 13.1 MiB after seeding

## Capacity and performance curve

Latencies are of the database call (service operation) in milliseconds; `user p99` adds the wait for a pooled connection.

| users | conns | ops/s | reads/s | writes/s | p50 | p95 | p99 | p99.9 | max | user p99 | success % | conflict retry % | srv CPU | srv RSS MiB | gen CPU | thr CV | invariants |
|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|
| 100 | 100 | 17876.3 | 13831.3 | 4044.9 | 1.997 | 19.298 | 38.6 | 83.68 | 944.802 | 38.601 | 99.993 | 0.522 | 5.13 | 156.9 | 2.07 | 0.102 | ok |

## Insights

- **Peak throughput**: 17876.3 ops/s at 100 users (p99 38.6 ms).
- **Capacity at p99 ≤ 10 ms**: 0 users (DB latency); 0 users counting pool wait.
- **Capacity at p99 ≤ 50 ms**: 100 users (DB latency); 100 users counting pool wait.
- **Capacity at p99 ≤ 100 ms**: 100 users (DB latency); 100 users counting pool wait.
- **Capacity at p99 ≤ 250 ms**: 100 users (DB latency); 100 users counting pool wait.
- **Capacity at p99 ≤ 1000 ms**: 100 users (DB latency); 100 users counting pool wait.
- **Ops per server core-second**: 100→3485.
- **Read-your-writes violations**: none.
- **Business invariants**: all stages ok.
- **Offline integrity check** (elitesql check): ok in 7.17 s.
  - right after killing the server (no clean close): exit 3, 0 errors, 226142 warnings; after one clean open/close: exit 0, 0 errors, 0 warnings.
- **Database size**: 68.2 MiB after the first stage, 68.2 MiB after the last; 11020 paid orders in total.

## p99 latency per operation (ms)

| op | share % | 100 |
|---|---:|---:|
| add_to_cart | 10.12 | 46.18 |
| admin_dashboard | 1.04 | 122.82 |
| browse | 20.49 | 18.99 |
| checkout | 4.1 | 83.94 |
| order_history | 3.04 | 32.53 |
| product_detail | 17.19 | 28.88 |
| recommend | 7.11 | 20.68 |
| relogin | 1.53 | 60.68 |
| restock | 0.31 | 76.50 |
| search_text | 8.14 | 21.96 |
| session_check | 12.15 | 44.49 |
| signup | 0.5 | 38.22 |
| update_cart_item | 3.04 | 37.92 |
| update_profile | 1.01 | 34.54 |
| view_cart | 8.2 | 25.51 |
| write_review | 2.03 | 41.37 |

## Errors and business outcomes at the highest level

```json
{
 "statuses": {
  "ok": 264806,
  "empty_cart": 3302,
  "not_found": 18,
  "conflict_exhausted": 18
 },
 "errors": {
  "conflict_exhausted": {
   "count": 21,
   "first": "[elitesql:9] conflict, retry transaction: products/b1 changed after this transaction began",
   "ops": {
    "checkout": 21
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
