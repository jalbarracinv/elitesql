# Mini-SaaS concurrency simulation — sidecar

Run: `A-1` on 2026-09-23T14:18:07 · commit `92e0290` · macOS-26.6.2-arm64-arm-64bit · 10 CPUs

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
| 100 | 100 | 23703.6 | 18342.8 | 5360.8 | 0.854 | 14.854 | 26.343 | 66.605 | 978.377 | 26.344 | 99.992 | 0.745 | 6.02 | 187.0 | 2.57 | 0.072 | ok |

## Insights

- **Peak throughput**: 23703.6 ops/s at 100 users (p99 26.343 ms).
- **Capacity at p99 ≤ 10 ms**: 0 users (DB latency); 0 users counting pool wait.
- **Capacity at p99 ≤ 50 ms**: 100 users (DB latency); 100 users counting pool wait.
- **Capacity at p99 ≤ 100 ms**: 100 users (DB latency); 100 users counting pool wait.
- **Capacity at p99 ≤ 250 ms**: 100 users (DB latency); 100 users counting pool wait.
- **Capacity at p99 ≤ 1000 ms**: 100 users (DB latency); 100 users counting pool wait.
- **Ops per server core-second**: 100→3937.
- **Read-your-writes violations**: none.
- **Business invariants**: all stages ok.
- **Offline integrity check** (elitesql check): ok in 8.34 s.
  - right after killing the server (no clean close): exit 3, 0 errors, 284612 warnings; after one clean open/close: exit 0, 0 errors, 0 warnings.
- **Database size**: 88.2 MiB after the first stage, 88.2 MiB after the last; 14939 paid orders in total.

## p99 latency per operation (ms)

| op | share % | 100 |
|---|---:|---:|
| add_to_cart | 10.11 | 31.26 |
| admin_dashboard | 1.03 | 108.83 |
| browse | 20.4 | 5.95 |
| checkout | 4.07 | 74.78 |
| order_history | 3.02 | 15.02 |
| product_detail | 17.23 | 10.45 |
| recommend | 7.15 | 9.18 |
| relogin | 1.54 | 39.98 |
| restock | 0.3 | 92.24 |
| search_text | 8.18 | 8.61 |
| session_check | 12.16 | 23.79 |
| signup | 0.51 | 28.15 |
| update_cart_item | 3.03 | 25.66 |
| update_profile | 1.02 | 19.91 |
| view_cart | 8.2 | 11.09 |
| write_review | 2.03 | 26.17 |

## Errors and business outcomes at the highest level

```json
{
 "statuses": {
  "ok": 351228,
  "empty_cart": 4276,
  "not_found": 23,
  "conflict_exhausted": 27
 },
 "errors": {
  "conflict_exhausted": {
   "count": 43,
   "first": "[elitesql:9] conflict, retry transaction: products/b7 changed after this transaction began",
   "ops": {
    "checkout": 38,
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
