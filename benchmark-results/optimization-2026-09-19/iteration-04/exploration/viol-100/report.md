# Mini-SaaS concurrency simulation — sidecar

Run: `viol-100` on 2026-09-23T16:41:06 · commit `92e0290` · macOS-26.6.2-arm64-arm-64bit · 10 CPUs

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
| 100 | 100 | 23022.5 | 17810.4 | 5212.1 | 1.017 | 14.892 | 26.495 | 69.902 | 955.782 | 26.496 | 99.979 | 0.722 | 5.57 | 230.4 | 2.75 | 0.072 | ok |

## Insights

- **Peak throughput**: 23022.5 ops/s at 100 users (p99 26.495 ms).
- **Capacity at p99 ≤ 10 ms**: 0 users (DB latency); 0 users counting pool wait.
- **Capacity at p99 ≤ 50 ms**: 100 users (DB latency); 100 users counting pool wait.
- **Capacity at p99 ≤ 100 ms**: 100 users (DB latency); 100 users counting pool wait.
- **Capacity at p99 ≤ 250 ms**: 100 users (DB latency); 100 users counting pool wait.
- **Capacity at p99 ≤ 1000 ms**: 100 users (DB latency); 100 users counting pool wait.
- **Ops per server core-second**: 100→4133.
- **Read-your-writes violations**: none.
- **Business invariants**: all stages ok.
- **Offline integrity check** (elitesql check): ok in 8.32 s.
  - right after killing the server (no clean close): exit 3, 0 errors, 279848 warnings; after one clean open/close: exit 0, 0 errors, 0 warnings.
- **Database size**: 86.8 MiB after the first stage, 86.8 MiB after the last; 14550 paid orders in total.

## p99 latency per operation (ms)

| op | share % | 100 |
|---|---:|---:|
| add_to_cart | 10.13 | 31.26 |
| admin_dashboard | 1.02 | 77.91 |
| browse | 20.4 | 7.87 |
| checkout | 4.07 | 109.28 |
| order_history | 3.01 | 20.09 |
| product_detail | 17.21 | 12.11 |
| recommend | 7.16 | 9.00 |
| relogin | 1.54 | 44.23 |
| restock | 0.3 | 65.84 |
| search_text | 8.18 | 10.82 |
| session_check | 12.17 | 24.71 |
| signup | 0.51 | 30.60 |
| update_cart_item | 3.03 | 26.02 |
| update_profile | 1.03 | 19.77 |
| view_cart | 8.21 | 15.11 |
| write_review | 2.04 | 25.38 |

## Errors and business outcomes at the highest level

```json
{
 "statuses": {
  "ok": 341087,
  "empty_cart": 4156,
  "not_found": 20,
  "conflict_exhausted": 74
 },
 "errors": {
  "conflict_exhausted": {
   "count": 101,
   "first": "[elitesql:9] conflict, retry transaction: products/b6 changed after this transaction began",
   "ops": {
    "checkout": 101
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
