# Mini-SaaS concurrency simulation — sidecar

Run: `A-1` on 2026-09-23T13:28:31 · commit `92e0290` · macOS-26.6.2-arm64-arm-64bit · 10 CPUs

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
- **seed time**: 1.19 s, 13.1 MiB after seeding

## Capacity and performance curve

Latencies are of the database call (service operation) in milliseconds; `user p99` adds the wait for a pooled connection.

| users | conns | ops/s | reads/s | writes/s | p50 | p95 | p99 | p99.9 | max | user p99 | success % | conflict retry % | srv CPU | srv RSS MiB | gen CPU | thr CV | invariants |
|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|
| 100 | 100 | 14786.5 | 11440.0 | 3346.5 | 2.989 | 21.862 | 45.794 | 233.812 | 1519.485 | 45.796 | 99.971 | 0.735 | 4.48 | 143.2 | 1.91 | 0.114 | ok |

## Insights

- **Peak throughput**: 14786.5 ops/s at 100 users (p99 45.794 ms).
- **Capacity at p99 ≤ 10 ms**: 0 users (DB latency); 0 users counting pool wait.
- **Capacity at p99 ≤ 50 ms**: 100 users (DB latency); 100 users counting pool wait.
- **Capacity at p99 ≤ 100 ms**: 100 users (DB latency); 100 users counting pool wait.
- **Capacity at p99 ≤ 250 ms**: 100 users (DB latency); 100 users counting pool wait.
- **Capacity at p99 ≤ 1000 ms**: 100 users (DB latency); 100 users counting pool wait.
- **Ops per server core-second**: 100→3301.
- **Read-your-writes violations**: none.
- **Business invariants**: all stages ok.
- **Offline integrity check** (elitesql check): ok in 6.47 s.
  - right after killing the server (no clean close): exit 3, 0 errors, 204878 warnings; after one clean open/close: exit 0, 0 errors, 0 warnings.
- **Database size**: 61.7 MiB after the first stage, 61.7 MiB after the last; 9569 paid orders in total.

## p99 latency per operation (ms)

| op | share % | 100 |
|---|---:|---:|
| add_to_cart | 10.1 | 58.54 |
| admin_dashboard | 1.03 | 84.97 |
| browse | 20.56 | 19.62 |
| checkout | 4.11 | 281.49 |
| order_history | 3.06 | 31.84 |
| product_detail | 17.21 | 26.37 |
| recommend | 7.1 | 25.83 |
| relogin | 1.53 | 74.67 |
| restock | 0.31 | 109.00 |
| search_text | 8.12 | 19.91 |
| session_check | 12.1 | 33.44 |
| signup | 0.5 | 49.64 |
| update_cart_item | 3.05 | 42.79 |
| update_profile | 1.01 | 24.39 |
| view_cart | 8.2 | 30.64 |
| write_review | 2.03 | 39.25 |

## Errors and business outcomes at the highest level

```json
{
 "statuses": {
  "ok": 218957,
  "empty_cart": 2758,
  "not_found": 17,
  "conflict_exhausted": 65
 },
 "errors": {
  "conflict_exhausted": {
   "count": 73,
   "first": "[elitesql:9] conflict, retry transaction: products/c12 changed after this transaction began",
   "ops": {
    "checkout": 73
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
