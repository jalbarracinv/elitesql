# Mini-SaaS concurrency simulation — sidecar

Run: `A-1` on 2026-09-23T13:07:07 · commit `92e0290` · macOS-26.6.2-arm64-arm-64bit · 10 CPUs

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
- **seed time**: 1.15 s, 13.1 MiB after seeding

## Capacity and performance curve

Latencies are of the database call (service operation) in milliseconds; `user p99` adds the wait for a pooled connection.

| users | conns | ops/s | reads/s | writes/s | p50 | p95 | p99 | p99.9 | max | user p99 | success % | conflict retry % | srv CPU | srv RSS MiB | gen CPU | thr CV | invariants |
|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|
| 100 | 100 | 18277.8 | 14142.6 | 4135.2 | 1.907 | 19.057 | 37.252 | 80.129 | 980.846 | 37.253 | 99.997 | 0.502 | 5.12 | 161.5 | 2.09 | 0.084 | ok |

## Insights

- **Peak throughput**: 18277.8 ops/s at 100 users (p99 37.252 ms).
- **Capacity at p99 ≤ 10 ms**: 0 users (DB latency); 0 users counting pool wait.
- **Capacity at p99 ≤ 50 ms**: 100 users (DB latency); 100 users counting pool wait.
- **Capacity at p99 ≤ 100 ms**: 100 users (DB latency); 100 users counting pool wait.
- **Capacity at p99 ≤ 250 ms**: 100 users (DB latency); 100 users counting pool wait.
- **Capacity at p99 ≤ 1000 ms**: 100 users (DB latency); 100 users counting pool wait.
- **Ops per server core-second**: 100→3570.
- **Read-your-writes violations**: none.
- **Business invariants**: all stages ok.
- **Offline integrity check** (elitesql check): ok in 7.29 s.
  - right after killing the server (no clean close): exit 3, 0 errors, 232610 warnings; after one clean open/close: exit 0, 0 errors, 0 warnings.
- **Database size**: 70.3 MiB after the first stage, 70.3 MiB after the last; 11473 paid orders in total.

## p99 latency per operation (ms)

| op | share % | 100 |
|---|---:|---:|
| add_to_cart | 10.08 | 40.61 |
| admin_dashboard | 1.05 | 104.70 |
| browse | 20.44 | 18.32 |
| checkout | 4.1 | 68.74 |
| order_history | 3.04 | 31.60 |
| product_detail | 17.21 | 26.98 |
| recommend | 7.11 | 18.76 |
| relogin | 1.54 | 66.13 |
| restock | 0.31 | 86.02 |
| search_text | 8.14 | 23.11 |
| session_check | 12.17 | 44.92 |
| signup | 0.51 | 38.10 |
| update_cart_item | 3.04 | 38.83 |
| update_profile | 1.02 | 37.07 |
| view_cart | 8.21 | 25.70 |
| write_review | 2.03 | 36.87 |

## Errors and business outcomes at the highest level

```json
{
 "statuses": {
  "ok": 270745,
  "empty_cart": 3398,
  "not_found": 17,
  "conflict_exhausted": 7
 },
 "errors": {
  "conflict_exhausted": {
   "count": 8,
   "first": "[elitesql:9] conflict, retry transaction: products/c78 changed after this transaction began",
   "ops": {
    "checkout": 7,
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
