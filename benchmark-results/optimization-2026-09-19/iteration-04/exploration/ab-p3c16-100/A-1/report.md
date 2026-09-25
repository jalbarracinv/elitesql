# Mini-SaaS concurrency simulation — sidecar

Run: `A-1` on 2026-09-23T13:26:49 · commit `92e0290` · macOS-26.6.2-arm64-arm-64bit · 10 CPUs

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
| 100 | 100 | 14036.9 | 10866.7 | 3170.3 | 2.267 | 23.954 | 47.581 | 130.771 | 1082.551 | 47.583 | 99.96 | 0.738 | 5.39 | 139.6 | 1.97 | 0.086 | ok |

## Insights

- **Peak throughput**: 14036.9 ops/s at 100 users (p99 47.581 ms).
- **Capacity at p99 ≤ 10 ms**: 0 users (DB latency); 0 users counting pool wait.
- **Capacity at p99 ≤ 50 ms**: 100 users (DB latency); 100 users counting pool wait.
- **Capacity at p99 ≤ 100 ms**: 100 users (DB latency); 100 users counting pool wait.
- **Capacity at p99 ≤ 250 ms**: 100 users (DB latency); 100 users counting pool wait.
- **Capacity at p99 ≤ 1000 ms**: 100 users (DB latency); 100 users counting pool wait.
- **Ops per server core-second**: 100→2604.
- **Read-your-writes violations**: none.
- **Business invariants**: all stages ok.
- **Offline integrity check** (elitesql check): ok in 6.26 s.
  - right after killing the server (no clean close): exit 3, 0 errors, 196968 warnings; after one clean open/close: exit 0, 0 errors, 0 warnings.
- **Database size**: 58.4 MiB after the first stage, 58.4 MiB after the last; 9005 paid orders in total.

## p99 latency per operation (ms)

| op | share % | 100 |
|---|---:|---:|
| add_to_cart | 10.11 | 52.31 |
| admin_dashboard | 1.03 | 170.49 |
| browse | 20.54 | 21.11 |
| checkout | 4.07 | 265.26 |
| order_history | 3.06 | 37.74 |
| product_detail | 17.24 | 19.88 |
| recommend | 7.13 | 12.96 |
| relogin | 1.53 | 75.28 |
| restock | 0.31 | 98.28 |
| search_text | 8.13 | 25.77 |
| session_check | 12.15 | 47.08 |
| signup | 0.5 | 55.68 |
| update_cart_item | 3.03 | 45.03 |
| update_profile | 1.02 | 26.48 |
| view_cart | 8.14 | 34.04 |
| write_review | 2.02 | 44.50 |

## Errors and business outcomes at the highest level

```json
{
 "statuses": {
  "ok": 207871,
  "empty_cart": 2583,
  "not_found": 16,
  "conflict_exhausted": 84
 },
 "errors": {
  "conflict_exhausted": {
   "count": 106,
   "first": "[elitesql:9] conflict, retry transaction: products/b7 changed after this transaction began",
   "ops": {
    "restock": 1,
    "checkout": 105
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
