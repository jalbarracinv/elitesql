# Mini-SaaS concurrency simulation — sidecar

Run: `A-2` on 2026-09-23T14:23:30 · commit `92e0290` · macOS-26.6.2-arm64-arm-64bit · 10 CPUs

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
| 100 | 100 | 23401.7 | 18101.1 | 5300.6 | 0.883 | 14.825 | 25.709 | 73.442 | 997.027 | 25.709 | 99.975 | 0.703 | 6.09 | 185.2 | 2.64 | 0.074 | ok |

## Insights

- **Peak throughput**: 23401.7 ops/s at 100 users (p99 25.709 ms).
- **Capacity at p99 ≤ 10 ms**: 0 users (DB latency); 0 users counting pool wait.
- **Capacity at p99 ≤ 50 ms**: 100 users (DB latency); 100 users counting pool wait.
- **Capacity at p99 ≤ 100 ms**: 100 users (DB latency); 100 users counting pool wait.
- **Capacity at p99 ≤ 250 ms**: 100 users (DB latency); 100 users counting pool wait.
- **Capacity at p99 ≤ 1000 ms**: 100 users (DB latency); 100 users counting pool wait.
- **Ops per server core-second**: 100→3843.
- **Read-your-writes violations**: none.
- **Business invariants**: all stages ok.
- **Offline integrity check** (elitesql check): ok in 8.34 s.
  - right after killing the server (no clean close): exit 3, 0 errors, 283508 warnings; after one clean open/close: exit 0, 0 errors, 0 warnings.
- **Database size**: 88.0 MiB after the first stage, 88.0 MiB after the last; 14818 paid orders in total.

## p99 latency per operation (ms)

| op | share % | 100 |
|---|---:|---:|
| add_to_cart | 10.11 | 29.42 |
| admin_dashboard | 1.03 | 110.32 |
| browse | 20.39 | 5.88 |
| checkout | 4.08 | 121.23 |
| order_history | 3.01 | 13.00 |
| product_detail | 17.23 | 9.80 |
| recommend | 7.16 | 9.91 |
| relogin | 1.54 | 39.81 |
| restock | 0.3 | 106.71 |
| search_text | 8.19 | 8.74 |
| session_check | 12.16 | 22.62 |
| signup | 0.51 | 29.55 |
| update_cart_item | 3.05 | 22.65 |
| update_profile | 1.04 | 19.67 |
| view_cart | 8.19 | 11.09 |
| write_review | 2.04 | 26.11 |

## Errors and business outcomes at the highest level

```json
{
 "statuses": {
  "ok": 346676,
  "empty_cart": 4240,
  "conflict_exhausted": 88,
  "not_found": 22
 },
 "errors": {
  "conflict_exhausted": {
   "count": 102,
   "first": "[elitesql:9] conflict, retry transaction: products/c11 changed after this transaction began",
   "ops": {
    "checkout": 102
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
