# Mini-SaaS concurrency simulation — sidecar

Run: `B-1` on 2026-09-23T14:18:48 · commit `92e0290` · macOS-26.6.2-arm64-arm-64bit · 10 CPUs

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
| 100 | 100 | 25827.6 | 19971.6 | 5856.0 | 0.601 | 14.196 | 23.717 | 59.657 | 995.196 | 23.718 | 99.98 | 0.697 | 5.97 | 242.4 | 2.79 | 0.095 | ok |

## Insights

- **Peak throughput**: 25827.6 ops/s at 100 users (p99 23.717 ms).
- **Capacity at p99 ≤ 10 ms**: 0 users (DB latency); 0 users counting pool wait.
- **Capacity at p99 ≤ 50 ms**: 100 users (DB latency); 100 users counting pool wait.
- **Capacity at p99 ≤ 100 ms**: 100 users (DB latency); 100 users counting pool wait.
- **Capacity at p99 ≤ 250 ms**: 100 users (DB latency); 100 users counting pool wait.
- **Capacity at p99 ≤ 1000 ms**: 100 users (DB latency); 100 users counting pool wait.
- **Ops per server core-second**: 100→4326.
- **Read-your-writes violations**: [(100, 8)].
- **Business invariants**: all stages ok.
- **Offline integrity check** (elitesql check): ok in 9.18 s.
  - right after killing the server (no clean close): exit 3, 0 errors, 311296 warnings; after one clean open/close: exit 0, 0 errors, 0 warnings.
- **Database size**: 97.4 MiB after the first stage, 97.4 MiB after the last; 16662 paid orders in total.

## p99 latency per operation (ms)

| op | share % | 100 |
|---|---:|---:|
| add_to_cart | 10.14 | 25.87 |
| admin_dashboard | 1.02 | 92.59 |
| browse | 20.37 | 4.35 |
| checkout | 4.1 | 72.20 |
| order_history | 2.99 | 9.82 |
| product_detail | 17.25 | 7.50 |
| recommend | 7.15 | 4.93 |
| relogin | 1.53 | 36.47 |
| restock | 0.29 | 107.18 |
| search_text | 8.2 | 1.50 |
| session_check | 12.18 | 20.61 |
| signup | 0.51 | 21.07 |
| update_cart_item | 3.04 | 21.65 |
| update_profile | 1.03 | 20.01 |
| view_cart | 8.18 | 8.02 |
| write_review | 2.04 | 21.93 |

## Errors and business outcomes at the highest level

```json
{
 "statuses": {
  "ok": 382612,
  "empty_cart": 4697,
  "not_found": 28,
  "conflict_exhausted": 77
 },
 "errors": {
  "conflict_exhausted": {
   "count": 112,
   "first": "[elitesql:9] conflict, retry transaction: products/c15 changed after this transaction began",
   "ops": {
    "checkout": 107,
    "restock": 5
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
