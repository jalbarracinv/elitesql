# Mini-SaaS concurrency simulation — sidecar

Run: `A-2` on 2026-09-23T13:08:28 · commit `92e0290` · macOS-26.6.2-arm64-arm-64bit · 10 CPUs

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
- **seed time**: 1.32 s, 13.1 MiB after seeding

## Capacity and performance curve

Latencies are of the database call (service operation) in milliseconds; `user p99` adds the wait for a pooled connection.

| users | conns | ops/s | reads/s | writes/s | p50 | p95 | p99 | p99.9 | max | user p99 | success % | conflict retry % | srv CPU | srv RSS MiB | gen CPU | thr CV | invariants |
|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|
| 100 | 100 | 18280.5 | 14142.8 | 4137.7 | 1.879 | 19.042 | 36.887 | 79.207 | 1028.018 | 36.888 | 99.998 | 0.506 | 5.22 | 162.8 | 2.08 | 0.064 | ok |

## Insights

- **Peak throughput**: 18280.5 ops/s at 100 users (p99 36.887 ms).
- **Capacity at p99 ≤ 10 ms**: 0 users (DB latency); 0 users counting pool wait.
- **Capacity at p99 ≤ 50 ms**: 100 users (DB latency); 100 users counting pool wait.
- **Capacity at p99 ≤ 100 ms**: 100 users (DB latency); 100 users counting pool wait.
- **Capacity at p99 ≤ 250 ms**: 100 users (DB latency); 100 users counting pool wait.
- **Capacity at p99 ≤ 1000 ms**: 100 users (DB latency); 100 users counting pool wait.
- **Ops per server core-second**: 100→3502.
- **Read-your-writes violations**: none.
- **Business invariants**: all stages ok.
- **Offline integrity check** (elitesql check): ok in 7.21 s.
  - right after killing the server (no clean close): exit 3, 0 errors, 232518 warnings; after one clean open/close: exit 0, 0 errors, 0 warnings.
- **Database size**: 70.3 MiB after the first stage, 70.3 MiB after the last; 11461 paid orders in total.

## p99 latency per operation (ms)

| op | share % | 100 |
|---|---:|---:|
| add_to_cart | 10.1 | 42.24 |
| admin_dashboard | 1.05 | 106.19 |
| browse | 20.46 | 18.59 |
| checkout | 4.09 | 67.16 |
| order_history | 3.04 | 29.42 |
| product_detail | 17.2 | 27.11 |
| recommend | 7.1 | 19.57 |
| relogin | 1.54 | 61.62 |
| restock | 0.31 | 68.44 |
| search_text | 8.14 | 20.98 |
| session_check | 12.15 | 43.73 |
| signup | 0.51 | 39.43 |
| update_cart_item | 3.05 | 36.26 |
| update_profile | 1.02 | 33.95 |
| view_cart | 8.22 | 25.49 |
| write_review | 2.03 | 38.91 |

## Errors and business outcomes at the highest level

```json
{
 "statuses": {
  "ok": 270810,
  "empty_cart": 3376,
  "not_found": 17,
  "conflict_exhausted": 5
 },
 "errors": {
  "conflict_exhausted": {
   "count": 9,
   "first": "[elitesql:9] conflict, retry transaction: products/c15 changed after this transaction began",
   "ops": {
    "checkout": 9
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
