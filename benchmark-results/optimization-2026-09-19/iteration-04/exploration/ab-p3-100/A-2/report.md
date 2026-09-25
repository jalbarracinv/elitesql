# Mini-SaaS concurrency simulation — sidecar

Run: `A-2` on 2026-09-23T13:23:41 · commit `92e0290` · macOS-26.6.2-arm64-arm-64bit · 10 CPUs

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
| 100 | 100 | 17775.5 | 13749.5 | 4026.0 | 2.072 | 19.215 | 38.17 | 86.63 | 908.298 | 38.172 | 99.988 | 0.497 | 5.09 | 159.2 | 2.14 | 0.079 | ok |

## Insights

- **Peak throughput**: 17775.5 ops/s at 100 users (p99 38.17 ms).
- **Capacity at p99 ≤ 10 ms**: 0 users (DB latency); 0 users counting pool wait.
- **Capacity at p99 ≤ 50 ms**: 100 users (DB latency); 100 users counting pool wait.
- **Capacity at p99 ≤ 100 ms**: 100 users (DB latency); 100 users counting pool wait.
- **Capacity at p99 ≤ 250 ms**: 100 users (DB latency); 100 users counting pool wait.
- **Capacity at p99 ≤ 1000 ms**: 100 users (DB latency); 100 users counting pool wait.
- **Ops per server core-second**: 100→3492.
- **Read-your-writes violations**: none.
- **Business invariants**: all stages ok.
- **Offline integrity check** (elitesql check): ok in 7.81 s.
  - right after killing the server (no clean close): exit 3, 0 errors, 227340 warnings; after one clean open/close: exit 0, 0 errors, 0 warnings.
- **Database size**: 68.7 MiB after the first stage, 68.7 MiB after the last; 11101 paid orders in total.

## p99 latency per operation (ms)

| op | share % | 100 |
|---|---:|---:|
| add_to_cart | 10.12 | 43.83 |
| admin_dashboard | 1.04 | 128.18 |
| browse | 20.47 | 18.62 |
| checkout | 4.1 | 75.54 |
| order_history | 3.05 | 31.24 |
| product_detail | 17.19 | 27.23 |
| recommend | 7.12 | 18.21 |
| relogin | 1.53 | 68.01 |
| restock | 0.31 | 80.33 |
| search_text | 8.13 | 20.63 |
| session_check | 12.15 | 43.89 |
| signup | 0.5 | 44.72 |
| update_cart_item | 3.04 | 37.05 |
| update_profile | 1.02 | 33.92 |
| view_cart | 8.21 | 25.95 |
| write_review | 2.03 | 35.46 |

## Errors and business outcomes at the highest level

```json
{
 "statuses": {
  "ok": 263306,
  "empty_cart": 3279,
  "not_found": 17,
  "conflict_exhausted": 31
 },
 "errors": {
  "conflict_exhausted": {
   "count": 33,
   "first": "[elitesql:9] conflict, retry transaction: products/c15 changed after this transaction began",
   "ops": {
    "checkout": 33
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
