# Mini-SaaS concurrency simulation — sidecar

Run: `A-1` on 2026-09-23T14:22:06 · commit `92e0290` · macOS-26.6.2-arm64-arm-64bit · 10 CPUs

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
| 100 | 100 | 23723.4 | 18355.9 | 5367.5 | 0.809 | 14.856 | 26.471 | 72.974 | 978.176 | 26.471 | 99.98 | 0.706 | 6.06 | 186.8 | 2.62 | 0.083 | ok |

## Insights

- **Peak throughput**: 23723.4 ops/s at 100 users (p99 26.471 ms).
- **Capacity at p99 ≤ 10 ms**: 0 users (DB latency); 0 users counting pool wait.
- **Capacity at p99 ≤ 50 ms**: 100 users (DB latency); 100 users counting pool wait.
- **Capacity at p99 ≤ 100 ms**: 100 users (DB latency); 100 users counting pool wait.
- **Capacity at p99 ≤ 250 ms**: 100 users (DB latency); 100 users counting pool wait.
- **Capacity at p99 ≤ 1000 ms**: 100 users (DB latency); 100 users counting pool wait.
- **Ops per server core-second**: 100→3915.
- **Read-your-writes violations**: none.
- **Business invariants**: all stages ok.
- **Offline integrity check** (elitesql check): ok in 8.33 s.
  - right after killing the server (no clean close): exit 3, 0 errors, 285534 warnings; after one clean open/close: exit 0, 0 errors, 0 warnings.
- **Database size**: 88.6 MiB after the first stage, 88.6 MiB after the last; 14969 paid orders in total.

## p99 latency per operation (ms)

| op | share % | 100 |
|---|---:|---:|
| add_to_cart | 10.12 | 33.43 |
| admin_dashboard | 1.03 | 107.17 |
| browse | 20.4 | 6.38 |
| checkout | 4.07 | 95.11 |
| order_history | 3.02 | 14.90 |
| product_detail | 17.23 | 10.96 |
| recommend | 7.16 | 10.18 |
| relogin | 1.53 | 40.00 |
| restock | 0.3 | 104.38 |
| search_text | 8.18 | 9.25 |
| session_check | 12.18 | 24.06 |
| signup | 0.51 | 28.50 |
| update_cart_item | 3.03 | 25.22 |
| update_profile | 1.03 | 21.07 |
| view_cart | 8.19 | 11.34 |
| write_review | 2.03 | 25.41 |

## Errors and business outcomes at the highest level

```json
{
 "statuses": {
  "ok": 351477,
  "empty_cart": 4282,
  "not_found": 21,
  "conflict_exhausted": 71
 },
 "errors": {
  "conflict_exhausted": {
   "count": 90,
   "first": "[elitesql:9] conflict, retry transaction: products/c14 changed after this transaction began",
   "ops": {
    "checkout": 89,
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
