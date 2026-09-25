# Mini-SaaS concurrency simulation — sidecar

Run: `B-2` on 2026-09-23T14:03:23 · commit `92e0290` · macOS-26.6.2-arm64-arm-64bit · 10 CPUs

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
| 100 | 100 | 22075.0 | 17078.5 | 4996.5 | 1.524 | 15.34 | 24.185 | 76.891 | 1101.489 | 24.187 | 99.947 | 0.716 | 5.62 | 178.7 | 3.18 | 0.081 | ok |

## Insights

- **Peak throughput**: 22075.0 ops/s at 100 users (p99 24.185 ms).
- **Capacity at p99 ≤ 10 ms**: 0 users (DB latency); 0 users counting pool wait.
- **Capacity at p99 ≤ 50 ms**: 100 users (DB latency); 100 users counting pool wait.
- **Capacity at p99 ≤ 100 ms**: 100 users (DB latency); 100 users counting pool wait.
- **Capacity at p99 ≤ 250 ms**: 100 users (DB latency); 100 users counting pool wait.
- **Capacity at p99 ≤ 1000 ms**: 100 users (DB latency); 100 users counting pool wait.
- **Ops per server core-second**: 100→3928.
- **Read-your-writes violations**: none.
- **Business invariants**: all stages ok.
- **Offline integrity check** (elitesql check): ok in 8.02 s.
  - right after killing the server (no clean close): exit 3, 0 errors, 271740 warnings; after one clean open/close: exit 0, 0 errors, 0 warnings.
- **Database size**: 84.6 MiB after the first stage, 84.6 MiB after the last; 13975 paid orders in total.

## p99 latency per operation (ms)

| op | share % | 100 |
|---|---:|---:|
| add_to_cart | 10.14 | 25.61 |
| admin_dashboard | 1.02 | 53.96 |
| browse | 20.39 | 4.01 |
| checkout | 4.07 | 242.05 |
| order_history | 3.02 | 13.11 |
| product_detail | 17.21 | 6.65 |
| recommend | 7.13 | 6.09 |
| relogin | 1.53 | 33.83 |
| restock | 0.3 | 52.48 |
| search_text | 8.19 | 5.58 |
| session_check | 12.21 | 22.66 |
| signup | 0.51 | 24.60 |
| update_cart_item | 3.03 | 22.36 |
| update_profile | 1.02 | 20.33 |
| view_cart | 8.19 | 9.66 |
| write_review | 2.02 | 19.42 |

## Errors and business outcomes at the highest level

```json
{
 "statuses": {
  "ok": 326984,
  "empty_cart": 3945,
  "not_found": 21,
  "conflict_exhausted": 175
 },
 "errors": {
  "conflict_exhausted": {
   "count": 217,
   "first": "[elitesql:9] conflict, retry transaction: products/b7 changed after this transaction began",
   "ops": {
    "checkout": 217
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
