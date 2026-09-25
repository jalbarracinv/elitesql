# Mini-SaaS concurrency simulation — sidecar

Run: `probe6c-100` on 2026-09-23T14:15:06 · commit `92e0290` · macOS-26.6.2-arm64-arm-64bit · 10 CPUs

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
- **seed time**: 1.11 s, 13.1 MiB after seeding

## Capacity and performance curve

Latencies are of the database call (service operation) in milliseconds; `user p99` adds the wait for a pooled connection.

| users | conns | ops/s | reads/s | writes/s | p50 | p95 | p99 | p99.9 | max | user p99 | success % | conflict retry % | srv CPU | srv RSS MiB | gen CPU | thr CV | invariants |
|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|
| 100 | 100 | 23415.6 | 18112.9 | 5302.7 | 0.869 | 15.151 | 26.841 | 69.691 | 1004.833 | 26.842 | 99.986 | 0.706 | 6.1 | 183.7 | 2.53 | 0.077 | ok |

## Insights

- **Peak throughput**: 23415.6 ops/s at 100 users (p99 26.841 ms).
- **Capacity at p99 ≤ 10 ms**: 0 users (DB latency); 0 users counting pool wait.
- **Capacity at p99 ≤ 50 ms**: 100 users (DB latency); 100 users counting pool wait.
- **Capacity at p99 ≤ 100 ms**: 100 users (DB latency); 100 users counting pool wait.
- **Capacity at p99 ≤ 250 ms**: 100 users (DB latency); 100 users counting pool wait.
- **Capacity at p99 ≤ 1000 ms**: 100 users (DB latency); 100 users counting pool wait.
- **Ops per server core-second**: 100→3839.
- **Read-your-writes violations**: none.
- **Business invariants**: all stages ok.
- **Offline integrity check** (elitesql check): ok in 8.42 s.
  - right after killing the server (no clean close): exit 3, 0 errors, 282874 warnings; after one clean open/close: exit 0, 0 errors, 0 warnings.
- **Database size**: 87.8 MiB after the first stage, 87.8 MiB after the last; 14793 paid orders in total.

## p99 latency per operation (ms)

| op | share % | 100 |
|---|---:|---:|
| add_to_cart | 10.14 | 31.10 |
| admin_dashboard | 1.03 | 120.89 |
| browse | 20.41 | 6.39 |
| checkout | 4.07 | 81.61 |
| order_history | 3.01 | 15.69 |
| product_detail | 17.23 | 10.62 |
| recommend | 7.14 | 10.33 |
| relogin | 1.54 | 42.84 |
| restock | 0.3 | 105.06 |
| search_text | 8.19 | 9.12 |
| session_check | 12.17 | 24.66 |
| signup | 0.51 | 23.23 |
| update_cart_item | 3.02 | 25.31 |
| update_profile | 1.03 | 20.66 |
| view_cart | 8.18 | 11.81 |
| write_review | 2.04 | 26.28 |

## Errors and business outcomes at the highest level

```json
{
 "statuses": {
  "ok": 346943,
  "empty_cart": 4221,
  "conflict_exhausted": 48,
  "not_found": 22
 },
 "errors": {
  "conflict_exhausted": {
   "count": 68,
   "first": "[elitesql:9] conflict, retry transaction: products/c12 changed after this transaction began",
   "ops": {
    "checkout": 60,
    "restock": 8
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
