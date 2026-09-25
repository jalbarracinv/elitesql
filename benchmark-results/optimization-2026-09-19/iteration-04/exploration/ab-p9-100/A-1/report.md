# Mini-SaaS concurrency simulation — sidecar

Run: `A-1` on 2026-09-23T14:43:02 · commit `92e0290` · macOS-26.6.2-arm64-arm-64bit · 10 CPUs

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
- **seed time**: 1.13 s, 13.1 MiB after seeding

## Capacity and performance curve

Latencies are of the database call (service operation) in milliseconds; `user p99` adds the wait for a pooled connection.

| users | conns | ops/s | reads/s | writes/s | p50 | p95 | p99 | p99.9 | max | user p99 | success % | conflict retry % | srv CPU | srv RSS MiB | gen CPU | thr CV | invariants |
|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|
| 100 | 100 | 23861.7 | 18459.8 | 5401.9 | 0.846 | 14.719 | 25.289 | 65.859 | 959.71 | 25.29 | 99.987 | 0.692 | 6.08 | 186.6 | 2.58 | 0.069 | ok |

## Insights

- **Peak throughput**: 23861.7 ops/s at 100 users (p99 25.289 ms).
- **Capacity at p99 ≤ 10 ms**: 0 users (DB latency); 0 users counting pool wait.
- **Capacity at p99 ≤ 50 ms**: 100 users (DB latency); 100 users counting pool wait.
- **Capacity at p99 ≤ 100 ms**: 100 users (DB latency); 100 users counting pool wait.
- **Capacity at p99 ≤ 250 ms**: 100 users (DB latency); 100 users counting pool wait.
- **Capacity at p99 ≤ 1000 ms**: 100 users (DB latency); 100 users counting pool wait.
- **Ops per server core-second**: 100→3925.
- **Read-your-writes violations**: none.
- **Business invariants**: all stages ok.
- **Offline integrity check** (elitesql check): ok in 8.36 s.
  - right after killing the server (no clean close): exit 3, 0 errors, 287040 warnings; after one clean open/close: exit 0, 0 errors, 0 warnings.
- **Database size**: 89.0 MiB after the first stage, 89.0 MiB after the last; 15092 paid orders in total.

## p99 latency per operation (ms)

| op | share % | 100 |
|---|---:|---:|
| add_to_cart | 10.12 | 28.89 |
| admin_dashboard | 1.03 | 103.78 |
| browse | 20.38 | 5.86 |
| checkout | 4.08 | 75.88 |
| order_history | 3.02 | 12.58 |
| product_detail | 17.23 | 10.40 |
| recommend | 7.15 | 9.18 |
| relogin | 1.54 | 36.98 |
| restock | 0.3 | 101.15 |
| search_text | 8.18 | 8.50 |
| session_check | 12.17 | 22.09 |
| signup | 0.51 | 27.94 |
| update_cart_item | 3.03 | 23.51 |
| update_profile | 1.03 | 19.82 |
| view_cart | 8.19 | 11.25 |
| write_review | 2.04 | 24.30 |

## Errors and business outcomes at the highest level

```json
{
 "statuses": {
  "ok": 353518,
  "empty_cart": 4336,
  "conflict_exhausted": 47,
  "not_found": 24
 },
 "errors": {
  "conflict_exhausted": {
   "count": 61,
   "first": "[elitesql:9] conflict, retry transaction: products/c14 changed after this transaction began",
   "ops": {
    "checkout": 57,
    "restock": 4
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
