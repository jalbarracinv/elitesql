# Mini-SaaS concurrency simulation — sidecar

Run: `B-1` on 2026-09-23T13:39:27 · commit `92e0290` · macOS-26.6.2-arm64-arm-64bit · 10 CPUs

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
| 100 | 100 | 18251.1 | 14116.7 | 4134.5 | 1.376 | 19.788 | 38.483 | 110.291 | 1089.176 | 38.486 | 99.967 | 0.714 | 5.15 | 157.7 | 2.53 | 0.088 | ok |

## Insights

- **Peak throughput**: 18251.1 ops/s at 100 users (p99 38.483 ms).
- **Capacity at p99 ≤ 10 ms**: 0 users (DB latency); 0 users counting pool wait.
- **Capacity at p99 ≤ 50 ms**: 100 users (DB latency); 100 users counting pool wait.
- **Capacity at p99 ≤ 100 ms**: 100 users (DB latency); 100 users counting pool wait.
- **Capacity at p99 ≤ 250 ms**: 100 users (DB latency); 100 users counting pool wait.
- **Capacity at p99 ≤ 1000 ms**: 100 users (DB latency); 100 users counting pool wait.
- **Ops per server core-second**: 100→3544.
- **Read-your-writes violations**: none.
- **Business invariants**: all stages ok.
- **Offline integrity check** (elitesql check): ok in 7.17 s.
  - right after killing the server (no clean close): exit 3, 0 errors, 229582 warnings; after one clean open/close: exit 0, 0 errors, 0 warnings.
- **Database size**: 69.4 MiB after the first stage, 69.4 MiB after the last; 11204 paid orders in total.

## p99 latency per operation (ms)

| op | share % | 100 |
|---|---:|---:|
| add_to_cart | 10.12 | 42.43 |
| admin_dashboard | 1.05 | 166.76 |
| browse | 20.47 | 14.88 |
| checkout | 4.1 | 161.17 |
| order_history | 3.04 | 28.52 |
| product_detail | 17.18 | 16.77 |
| recommend | 7.1 | 11.50 |
| relogin | 1.55 | 59.48 |
| restock | 0.31 | 72.44 |
| search_text | 8.14 | 18.54 |
| session_check | 12.17 | 39.05 |
| signup | 0.51 | 54.15 |
| update_cart_item | 3.03 | 35.27 |
| update_profile | 1.01 | 23.89 |
| view_cart | 8.21 | 24.47 |
| write_review | 2.02 | 34.22 |

## Errors and business outcomes at the highest level

```json
{
 "statuses": {
  "ok": 270300,
  "empty_cart": 3358,
  "conflict_exhausted": 90,
  "not_found": 19
 },
 "errors": {
  "conflict_exhausted": {
   "count": 104,
   "first": "[elitesql:9] conflict, retry transaction: products/c14 changed after this transaction began",
   "ops": {
    "checkout": 103,
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
