# Mini-SaaS concurrency simulation — sidecar

Run: `A-2` on 2026-09-23T13:50:28 · commit `92e0290` · macOS-26.6.2-arm64-arm-64bit · 10 CPUs

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
- **seed time**: 1.17 s, 13.1 MiB after seeding

## Capacity and performance curve

Latencies are of the database call (service operation) in milliseconds; `user p99` adds the wait for a pooled connection.

| users | conns | ops/s | reads/s | writes/s | p50 | p95 | p99 | p99.9 | max | user p99 | success % | conflict retry % | srv CPU | srv RSS MiB | gen CPU | thr CV | invariants |
|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|
| 100 | 100 | 18348.8 | 14199.3 | 4149.5 | 1.34 | 19.758 | 39.852 | 108.89 | 1138.055 | 39.855 | 99.974 | 0.712 | 5.13 | 162.7 | 2.53 | 0.072 | ok |

## Insights

- **Peak throughput**: 18348.8 ops/s at 100 users (p99 39.852 ms).
- **Capacity at p99 ≤ 10 ms**: 0 users (DB latency); 0 users counting pool wait.
- **Capacity at p99 ≤ 50 ms**: 100 users (DB latency); 100 users counting pool wait.
- **Capacity at p99 ≤ 100 ms**: 100 users (DB latency); 100 users counting pool wait.
- **Capacity at p99 ≤ 250 ms**: 100 users (DB latency); 100 users counting pool wait.
- **Capacity at p99 ≤ 1000 ms**: 100 users (DB latency); 100 users counting pool wait.
- **Ops per server core-second**: 100→3577.
- **Read-your-writes violations**: none.
- **Business invariants**: all stages ok.
- **Offline integrity check** (elitesql check): ok in 7.45 s.
  - right after killing the server (no clean close): exit 3, 0 errors, 236870 warnings; after one clean open/close: exit 0, 0 errors, 0 warnings.
- **Database size**: 72.8 MiB after the first stage, 72.8 MiB after the last; 11722 paid orders in total.

## p99 latency per operation (ms)

| op | share % | 100 |
|---|---:|---:|
| add_to_cart | 10.08 | 44.70 |
| admin_dashboard | 1.04 | 178.93 |
| browse | 20.45 | 15.31 |
| checkout | 4.11 | 139.97 |
| order_history | 3.04 | 31.55 |
| product_detail | 17.19 | 16.08 |
| recommend | 7.12 | 11.38 |
| relogin | 1.53 | 58.80 |
| restock | 0.31 | 105.07 |
| search_text | 8.15 | 17.48 |
| session_check | 12.17 | 41.98 |
| signup | 0.51 | 51.29 |
| update_cart_item | 3.03 | 35.96 |
| update_profile | 1.02 | 24.76 |
| view_cart | 8.21 | 26.91 |
| write_review | 2.03 | 40.36 |

## Errors and business outcomes at the highest level

```json
{
 "statuses": {
  "ok": 271744,
  "empty_cart": 3398,
  "not_found": 18,
  "conflict_exhausted": 72
 },
 "errors": {
  "conflict_exhausted": {
   "count": 89,
   "first": "[elitesql:9] conflict, retry transaction: products/b8 changed after this transaction began",
   "ops": {
    "checkout": 88,
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
