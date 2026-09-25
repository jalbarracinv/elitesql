# Mini-SaaS concurrency simulation — sidecar

Run: `B-1` on 2026-09-23T13:55:14 · commit `92e0290` · macOS-26.6.2-arm64-arm-64bit · 10 CPUs

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
| 100 | 100 | 23347.2 | 18065.2 | 5282.0 | 0.9 | 14.909 | 25.52 | 69.216 | 927.34 | 25.521 | 99.978 | 0.696 | 6.06 | 185.9 | 2.66 | 0.069 | ok |

## Insights

- **Peak throughput**: 23347.2 ops/s at 100 users (p99 25.52 ms).
- **Capacity at p99 ≤ 10 ms**: 0 users (DB latency); 0 users counting pool wait.
- **Capacity at p99 ≤ 50 ms**: 100 users (DB latency); 100 users counting pool wait.
- **Capacity at p99 ≤ 100 ms**: 100 users (DB latency); 100 users counting pool wait.
- **Capacity at p99 ≤ 250 ms**: 100 users (DB latency); 100 users counting pool wait.
- **Capacity at p99 ≤ 1000 ms**: 100 users (DB latency); 100 users counting pool wait.
- **Ops per server core-second**: 100→3853.
- **Read-your-writes violations**: [(100, 11)].
- **Business invariants**: all stages ok.
- **Offline integrity check** (elitesql check): ok in 8.21 s.
  - right after killing the server (no clean close): exit 3, 0 errors, 282914 warnings; after one clean open/close: exit 0, 0 errors, 0 warnings.
- **Database size**: 87.7 MiB after the first stage, 87.7 MiB after the last; 14785 paid orders in total.

## p99 latency per operation (ms)

| op | share % | 100 |
|---|---:|---:|
| add_to_cart | 10.11 | 28.89 |
| admin_dashboard | 1.03 | 95.58 |
| browse | 20.4 | 5.80 |
| checkout | 4.07 | 102.78 |
| order_history | 3.02 | 12.91 |
| product_detail | 17.23 | 9.81 |
| recommend | 7.16 | 9.58 |
| relogin | 1.53 | 40.43 |
| restock | 0.3 | 94.74 |
| search_text | 8.18 | 8.42 |
| session_check | 12.18 | 22.44 |
| signup | 0.51 | 24.25 |
| update_cart_item | 3.03 | 25.62 |
| update_profile | 1.03 | 19.55 |
| view_cart | 8.19 | 11.34 |
| write_review | 2.04 | 23.25 |

## Errors and business outcomes at the highest level

```json
{
 "statuses": {
  "ok": 345894,
  "empty_cart": 4217,
  "not_found": 20,
  "conflict_exhausted": 77
 },
 "errors": {
  "conflict_exhausted": {
   "count": 102,
   "first": "[elitesql:9] conflict, retry transaction: products/b4 changed after this transaction began",
   "ops": {
    "checkout": 99,
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
