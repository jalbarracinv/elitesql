# Mini-SaaS concurrency simulation — sidecar

Run: `probe6-100` on 2026-09-23T14:05:03 · commit `92e0290` · macOS-26.6.2-arm64-arm-64bit · 10 CPUs

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
- **seed time**: 1.12 s, 13.1 MiB after seeding

## Capacity and performance curve

Latencies are of the database call (service operation) in milliseconds; `user p99` adds the wait for a pooled connection.

| users | conns | ops/s | reads/s | writes/s | p50 | p95 | p99 | p99.9 | max | user p99 | success % | conflict retry % | srv CPU | srv RSS MiB | gen CPU | thr CV | invariants |
|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|
| 100 | 100 | 23347.9 | 18066.7 | 5281.3 | 0.888 | 14.901 | 26.124 | 65.74 | 985.332 | 26.125 | 99.99 | 0.731 | 6.1 | 197.8 | 2.53 | 0.062 | ok |

## Insights

- **Peak throughput**: 23347.9 ops/s at 100 users (p99 26.124 ms).
- **Capacity at p99 ≤ 10 ms**: 0 users (DB latency); 0 users counting pool wait.
- **Capacity at p99 ≤ 50 ms**: 100 users (DB latency); 100 users counting pool wait.
- **Capacity at p99 ≤ 100 ms**: 100 users (DB latency); 100 users counting pool wait.
- **Capacity at p99 ≤ 250 ms**: 100 users (DB latency); 100 users counting pool wait.
- **Capacity at p99 ≤ 1000 ms**: 100 users (DB latency); 100 users counting pool wait.
- **Ops per server core-second**: 100→3828.
- **Read-your-writes violations**: none.
- **Business invariants**: all stages ok.
- **Offline integrity check** (elitesql check): ok in 8.39 s.
  - right after killing the server (no clean close): exit 3, 0 errors, 281318 warnings; after one clean open/close: exit 0, 0 errors, 0 warnings.
- **Database size**: 87.2 MiB after the first stage, 87.2 MiB after the last; 14685 paid orders in total.

## p99 latency per operation (ms)

| op | share % | 100 |
|---|---:|---:|
| add_to_cart | 10.11 | 30.79 |
| admin_dashboard | 1.02 | 105.08 |
| browse | 20.41 | 5.39 |
| checkout | 4.07 | 76.64 |
| order_history | 3.01 | 13.50 |
| product_detail | 17.23 | 9.46 |
| recommend | 7.15 | 9.11 |
| relogin | 1.54 | 40.36 |
| restock | 0.3 | 88.72 |
| search_text | 8.19 | 8.26 |
| session_check | 12.17 | 24.07 |
| signup | 0.51 | 26.92 |
| update_cart_item | 3.03 | 25.72 |
| update_profile | 1.03 | 20.95 |
| view_cart | 8.19 | 10.97 |
| write_review | 2.04 | 24.43 |

## Errors and business outcomes at the highest level

```json
{
 "statuses": {
  "ok": 345933,
  "empty_cart": 4231,
  "not_found": 21,
  "conflict_exhausted": 34
 },
 "errors": {
  "conflict_exhausted": {
   "count": 50,
   "first": "[elitesql:9] conflict, retry transaction: products/b8 changed after this transaction began",
   "ops": {
    "checkout": 47,
    "restock": 3
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
