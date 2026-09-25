# Mini-SaaS concurrency simulation — sidecar

Run: `A-2` on 2026-09-23T14:57:15 · commit `92e0290` · macOS-26.6.2-arm64-arm-64bit · 10 CPUs

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
| 100 | 100 | 22647.3 | 17522.3 | 5125.0 | 0.882 | 14.996 | 26.52 | 79.934 | 1030.715 | 26.521 | 99.989 | 0.715 | 5.84 | 192.2 | 2.5 | 0.131 | ok |

## Insights

- **Peak throughput**: 22647.3 ops/s at 100 users (p99 26.52 ms).
- **Capacity at p99 ≤ 10 ms**: 0 users (DB latency); 0 users counting pool wait.
- **Capacity at p99 ≤ 50 ms**: 100 users (DB latency); 100 users counting pool wait.
- **Capacity at p99 ≤ 100 ms**: 100 users (DB latency); 100 users counting pool wait.
- **Capacity at p99 ≤ 250 ms**: 100 users (DB latency); 100 users counting pool wait.
- **Capacity at p99 ≤ 1000 ms**: 100 users (DB latency); 100 users counting pool wait.
- **Ops per server core-second**: 100→3878.
- **Read-your-writes violations**: none.
- **Business invariants**: all stages ok.
- **Offline integrity check** (elitesql check): ok in 8.17 s.
  - right after killing the server (no clean close): exit 3, 0 errors, 277700 warnings; after one clean open/close: exit 0, 0 errors, 0 warnings.
- **Database size**: 86.0 MiB after the first stage, 86.0 MiB after the last; 14439 paid orders in total.

## p99 latency per operation (ms)

| op | share % | 100 |
|---|---:|---:|
| add_to_cart | 10.12 | 32.71 |
| admin_dashboard | 1.02 | 113.58 |
| browse | 20.38 | 6.39 |
| checkout | 4.07 | 79.36 |
| order_history | 3.02 | 14.96 |
| product_detail | 17.22 | 10.67 |
| recommend | 7.17 | 9.96 |
| relogin | 1.53 | 44.89 |
| restock | 0.3 | 101.63 |
| search_text | 8.19 | 8.75 |
| session_check | 12.17 | 24.29 |
| signup | 0.51 | 31.03 |
| update_cart_item | 3.03 | 25.07 |
| update_profile | 1.03 | 20.14 |
| view_cart | 8.19 | 10.96 |
| write_review | 2.03 | 26.19 |

## Errors and business outcomes at the highest level

```json
{
 "statuses": {
  "ok": 335518,
  "empty_cart": 4132,
  "conflict_exhausted": 39,
  "not_found": 20
 },
 "errors": {
  "conflict_exhausted": {
   "count": 56,
   "first": "[elitesql:9] conflict, retry transaction: products/c15 changed after this transaction began",
   "ops": {
    "checkout": 53,
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
