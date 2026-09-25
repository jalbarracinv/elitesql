# Mini-SaaS concurrency simulation — sidecar

Run: `B-1` on 2026-09-23T13:27:28 · commit `92e0290` · macOS-26.6.2-arm64-arm-64bit · 10 CPUs

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
- **seed time**: 1.19 s, 13.1 MiB after seeding

## Capacity and performance curve

Latencies are of the database call (service operation) in milliseconds; `user p99` adds the wait for a pooled connection.

| users | conns | ops/s | reads/s | writes/s | p50 | p95 | p99 | p99.9 | max | user p99 | success % | conflict retry % | srv CPU | srv RSS MiB | gen CPU | thr CV | invariants |
|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|
| 100 | 100 | 14307.7 | 11074.4 | 3233.3 | 2.14 | 23.612 | 46.732 | 121.62 | 1124.277 | 46.734 | 99.973 | 0.757 | 5.42 | 139.1 | 1.95 | 0.092 | ok |

## Insights

- **Peak throughput**: 14307.7 ops/s at 100 users (p99 46.732 ms).
- **Capacity at p99 ≤ 10 ms**: 0 users (DB latency); 0 users counting pool wait.
- **Capacity at p99 ≤ 50 ms**: 100 users (DB latency); 100 users counting pool wait.
- **Capacity at p99 ≤ 100 ms**: 100 users (DB latency); 100 users counting pool wait.
- **Capacity at p99 ≤ 250 ms**: 100 users (DB latency); 100 users counting pool wait.
- **Capacity at p99 ≤ 1000 ms**: 100 users (DB latency); 100 users counting pool wait.
- **Ops per server core-second**: 100→2640.
- **Read-your-writes violations**: none.
- **Business invariants**: all stages ok.
- **Offline integrity check** (elitesql check): ok in 6.21 s.
  - right after killing the server (no clean close): exit 3, 0 errors, 197024 warnings; after one clean open/close: exit 0, 0 errors, 0 warnings.
- **Database size**: 58.3 MiB after the first stage, 58.3 MiB after the last; 9002 paid orders in total.

## p99 latency per operation (ms)

| op | share % | 100 |
|---|---:|---:|
| add_to_cart | 10.11 | 50.82 |
| admin_dashboard | 1.03 | 163.94 |
| browse | 20.55 | 17.75 |
| checkout | 4.05 | 173.37 |
| order_history | 3.04 | 38.33 |
| product_detail | 17.21 | 18.91 |
| recommend | 7.12 | 12.58 |
| relogin | 1.54 | 69.39 |
| restock | 0.31 | 92.14 |
| search_text | 8.14 | 20.26 |
| session_check | 12.14 | 46.82 |
| signup | 0.5 | 52.50 |
| update_cart_item | 3.05 | 44.28 |
| update_profile | 1.02 | 26.11 |
| view_cart | 8.16 | 29.68 |
| write_review | 2.03 | 44.06 |

## Errors and business outcomes at the highest level

```json
{
 "statuses": {
  "ok": 211916,
  "empty_cart": 2626,
  "conflict_exhausted": 58,
  "not_found": 16
 },
 "errors": {
  "conflict_exhausted": {
   "count": 68,
   "first": "[elitesql:9] conflict, retry transaction: products/b3 changed after this transaction began",
   "ops": {
    "checkout": 68
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
