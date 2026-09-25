# Mini-SaaS concurrency simulation — sidecar

Run: `viol-p6-2` on 2026-09-23T16:47:49 · commit `92e0290` · macOS-26.6.2-arm64-arm-64bit · 10 CPUs

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
| 100 | 100 | 23154.5 | 17913.6 | 5240.9 | 0.856 | 15.202 | 27.473 | 73.361 | 1051.549 | 27.474 | 99.983 | 0.698 | 6.01 | 192.8 | 2.61 | 0.077 | ok |

## Insights

- **Peak throughput**: 23154.5 ops/s at 100 users (p99 27.473 ms).
- **Capacity at p99 ≤ 10 ms**: 0 users (DB latency); 0 users counting pool wait.
- **Capacity at p99 ≤ 50 ms**: 100 users (DB latency); 100 users counting pool wait.
- **Capacity at p99 ≤ 100 ms**: 100 users (DB latency); 100 users counting pool wait.
- **Capacity at p99 ≤ 250 ms**: 100 users (DB latency); 100 users counting pool wait.
- **Capacity at p99 ≤ 1000 ms**: 100 users (DB latency); 100 users counting pool wait.
- **Ops per server core-second**: 100→3853.
- **Read-your-writes violations**: [(100, 11)].
- **Business invariants**: all stages ok.
- **Offline integrity check** (elitesql check): ok in 8.28 s.
  - right after killing the server (no clean close): exit 3, 0 errors, 280260 warnings; after one clean open/close: exit 0, 0 errors, 0 warnings.
- **Database size**: 87.0 MiB after the first stage, 87.0 MiB after the last; 14613 paid orders in total.

## p99 latency per operation (ms)

| op | share % | 100 |
|---|---:|---:|
| add_to_cart | 10.13 | 34.73 |
| admin_dashboard | 1.02 | 112.87 |
| browse | 20.4 | 7.08 |
| checkout | 4.07 | 92.07 |
| order_history | 3.02 | 14.74 |
| product_detail | 17.23 | 11.71 |
| recommend | 7.14 | 10.15 |
| relogin | 1.54 | 45.15 |
| restock | 0.29 | 114.26 |
| search_text | 8.2 | 9.81 |
| session_check | 12.16 | 26.02 |
| signup | 0.51 | 30.86 |
| update_cart_item | 3.03 | 25.71 |
| update_profile | 1.02 | 21.20 |
| view_cart | 8.19 | 12.82 |
| write_review | 2.04 | 26.29 |

## Errors and business outcomes at the highest level

```json
{
 "statuses": {
  "ok": 343067,
  "empty_cart": 4172,
  "conflict_exhausted": 58,
  "not_found": 20
 },
 "errors": {
  "conflict_exhausted": {
   "count": 82,
   "first": "[elitesql:9] conflict, retry transaction: products/b5 changed after this transaction began",
   "ops": {
    "checkout": 80,
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
