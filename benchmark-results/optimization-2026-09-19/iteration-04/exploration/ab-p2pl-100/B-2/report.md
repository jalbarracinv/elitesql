# Mini-SaaS concurrency simulation — sidecar

Run: `B-2` on 2026-09-23T13:13:25 · commit `92e0290` · macOS-26.6.2-arm64-arm-64bit · 10 CPUs

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
- **seed time**: 1.18 s, 13.1 MiB after seeding

## Capacity and performance curve

Latencies are of the database call (service operation) in milliseconds; `user p99` adds the wait for a pooled connection.

| users | conns | ops/s | reads/s | writes/s | p50 | p95 | p99 | p99.9 | max | user p99 | success % | conflict retry % | srv CPU | srv RSS MiB | gen CPU | thr CV | invariants |
|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|
| 100 | 100 | 9271.7 | 7184.1 | 2087.7 | 5.569 | 34.183 | 56.364 | 119.096 | 1205.69 | 56.366 | 99.979 | 0.788 | 6.24 | 114.0 | 1.28 | 0.109 | ok |

## Insights

- **Peak throughput**: 9271.7 ops/s at 100 users (p99 56.364 ms).
- **Capacity at p99 ≤ 10 ms**: 0 users (DB latency); 0 users counting pool wait.
- **Capacity at p99 ≤ 50 ms**: 0 users (DB latency); 0 users counting pool wait.
- **Capacity at p99 ≤ 100 ms**: 100 users (DB latency); 100 users counting pool wait.
- **Capacity at p99 ≤ 250 ms**: 100 users (DB latency); 100 users counting pool wait.
- **Capacity at p99 ≤ 1000 ms**: 100 users (DB latency); 100 users counting pool wait.
- **Ops per server core-second**: 100→1486.
- **Read-your-writes violations**: none.
- **Business invariants**: all stages ok.
- **Offline integrity check** (elitesql check): ok in 4.89 s.
  - right after killing the server (no clean close): exit 3, 0 errors, 149868 warnings; after one clean open/close: exit 0, 0 errors, 0 warnings.
- **Database size**: 42.7 MiB after the first stage, 42.7 MiB after the last; 5913 paid orders in total.

## p99 latency per operation (ms)

| op | share % | 100 |
|---|---:|---:|
| add_to_cart | 10.09 | 51.36 |
| admin_dashboard | 1.04 | 95.57 |
| browse | 20.49 | 67.06 |
| checkout | 4.08 | 194.84 |
| order_history | 3.04 | 26.49 |
| product_detail | 17.28 | 15.20 |
| recommend | 7.18 | 14.59 |
| relogin | 1.55 | 71.96 |
| restock | 0.31 | 135.01 |
| search_text | 8.15 | 10.27 |
| session_check | 12.19 | 43.17 |
| signup | 0.48 | 47.46 |
| update_cart_item | 2.96 | 46.43 |
| update_profile | 1.03 | 36.56 |
| view_cart | 8.11 | 20.09 |
| write_review | 2.01 | 42.27 |

## Errors and business outcomes at the highest level

```json
{
 "statuses": {
  "ok": 137344,
  "empty_cart": 1693,
  "not_found": 10,
  "conflict_exhausted": 29
 },
 "errors": {
  "conflict_exhausted": {
   "count": 33,
   "first": "[elitesql:9] conflict, retry transaction: products/b8 changed after this transaction began",
   "ops": {
    "checkout": 32,
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
