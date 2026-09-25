# Mini-SaaS concurrency simulation — sidecar

Run: `B-2` on 2026-09-23T14:20:14 · commit `92e0290` · macOS-26.6.2-arm64-arm-64bit · 10 CPUs

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
- **seed time**: 1.14 s, 13.1 MiB after seeding

## Capacity and performance curve

Latencies are of the database call (service operation) in milliseconds; `user p99` adds the wait for a pooled connection.

| users | conns | ops/s | reads/s | writes/s | p50 | p95 | p99 | p99.9 | max | user p99 | success % | conflict retry % | srv CPU | srv RSS MiB | gen CPU | thr CV | invariants |
|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|
| 100 | 100 | 25771.3 | 19929.1 | 5842.1 | 0.613 | 14.273 | 23.724 | 60.011 | 995.94 | 23.725 | 99.98 | 0.705 | 6.01 | 205.0 | 2.73 | 0.087 | ok |

## Insights

- **Peak throughput**: 25771.3 ops/s at 100 users (p99 23.724 ms).
- **Capacity at p99 ≤ 10 ms**: 0 users (DB latency); 0 users counting pool wait.
- **Capacity at p99 ≤ 50 ms**: 100 users (DB latency); 100 users counting pool wait.
- **Capacity at p99 ≤ 100 ms**: 100 users (DB latency); 100 users counting pool wait.
- **Capacity at p99 ≤ 250 ms**: 100 users (DB latency); 100 users counting pool wait.
- **Capacity at p99 ≤ 1000 ms**: 100 users (DB latency); 100 users counting pool wait.
- **Ops per server core-second**: 100→4288.
- **Read-your-writes violations**: none.
- **Business invariants**: all stages ok.
- **Offline integrity check** (elitesql check): ok in 9.08 s.
  - right after killing the server (no clean close): exit 3, 0 errors, 310750 warnings; after one clean open/close: exit 0, 0 errors, 0 warnings.
- **Database size**: 97.9 MiB after the first stage, 97.9 MiB after the last; 16619 paid orders in total.

## p99 latency per operation (ms)

| op | share % | 100 |
|---|---:|---:|
| add_to_cart | 10.14 | 25.72 |
| admin_dashboard | 1.02 | 97.34 |
| browse | 20.36 | 4.07 |
| checkout | 4.08 | 77.08 |
| order_history | 3.0 | 9.08 |
| product_detail | 17.25 | 7.02 |
| recommend | 7.14 | 4.47 |
| relogin | 1.53 | 34.93 |
| restock | 0.29 | 86.49 |
| search_text | 8.18 | 1.56 |
| session_check | 12.2 | 20.53 |
| signup | 0.51 | 22.60 |
| update_cart_item | 3.05 | 22.16 |
| update_profile | 1.02 | 19.70 |
| view_cart | 8.17 | 6.98 |
| write_review | 2.04 | 19.83 |

## Errors and business outcomes at the highest level

```json
{
 "statuses": {
  "ok": 381784,
  "empty_cart": 4678,
  "conflict_exhausted": 78,
  "not_found": 29
 },
 "errors": {
  "conflict_exhausted": {
   "count": 105,
   "first": "[elitesql:9] conflict, retry transaction: products/b6 changed after this transaction began",
   "ops": {
    "checkout": 97,
    "restock": 8
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
