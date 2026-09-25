# Mini-SaaS concurrency simulation — sidecar

Run: `B-2` on 2026-09-23T14:24:12 · commit `92e0290` · macOS-26.6.2-arm64-arm-64bit · 10 CPUs

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
| 100 | 100 | 23682.0 | 18318.9 | 5363.1 | 0.884 | 15.246 | 26.137 | 67.474 | 1054.275 | 26.138 | 99.983 | 0.716 | 6.01 | 186.0 | 2.61 | 0.068 | ok |

## Insights

- **Peak throughput**: 23682.0 ops/s at 100 users (p99 26.137 ms).
- **Capacity at p99 ≤ 10 ms**: 0 users (DB latency); 0 users counting pool wait.
- **Capacity at p99 ≤ 50 ms**: 100 users (DB latency); 100 users counting pool wait.
- **Capacity at p99 ≤ 100 ms**: 100 users (DB latency); 100 users counting pool wait.
- **Capacity at p99 ≤ 250 ms**: 100 users (DB latency); 100 users counting pool wait.
- **Capacity at p99 ≤ 1000 ms**: 100 users (DB latency); 100 users counting pool wait.
- **Ops per server core-second**: 100→3940.
- **Read-your-writes violations**: none.
- **Business invariants**: all stages ok.
- **Offline integrity check** (elitesql check): ok in 8.34 s.
  - right after killing the server (no clean close): exit 3, 0 errors, 285012 warnings; after one clean open/close: exit 0, 0 errors, 0 warnings.
- **Database size**: 88.4 MiB after the first stage, 88.4 MiB after the last; 14934 paid orders in total.

## p99 latency per operation (ms)

| op | share % | 100 |
|---|---:|---:|
| add_to_cart | 10.13 | 30.20 |
| admin_dashboard | 1.02 | 101.53 |
| browse | 20.38 | 5.97 |
| checkout | 4.07 | 88.56 |
| order_history | 3.02 | 12.82 |
| product_detail | 17.25 | 9.54 |
| recommend | 7.16 | 8.92 |
| relogin | 1.54 | 41.13 |
| restock | 0.3 | 83.30 |
| search_text | 8.19 | 8.26 |
| session_check | 12.17 | 23.12 |
| signup | 0.51 | 26.32 |
| update_cart_item | 3.04 | 25.10 |
| update_profile | 1.03 | 20.72 |
| view_cart | 8.17 | 11.22 |
| write_review | 2.04 | 30.05 |

## Errors and business outcomes at the highest level

```json
{
 "statuses": {
  "ok": 350895,
  "empty_cart": 4255,
  "not_found": 21,
  "conflict_exhausted": 59
 },
 "errors": {
  "conflict_exhausted": {
   "count": 78,
   "first": "[elitesql:9] conflict, retry transaction: products/c15 changed after this transaction began",
   "ops": {
    "checkout": 76,
    "restock": 2
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
