# Mini-SaaS concurrency simulation — sidecar

Run: `A-2` on 2026-09-23T13:12:44 · commit `92e0290` · macOS-26.6.2-arm64-arm-64bit · 10 CPUs

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
- **seed time**: 1.15 s, 13.1 MiB after seeding

## Capacity and performance curve

Latencies are of the database call (service operation) in milliseconds; `user p99` adds the wait for a pooled connection.

| users | conns | ops/s | reads/s | writes/s | p50 | p95 | p99 | p99.9 | max | user p99 | success % | conflict retry % | srv CPU | srv RSS MiB | gen CPU | thr CV | invariants |
|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|
| 100 | 100 | 18760.1 | 14516.3 | 4243.9 | 1.933 | 18.286 | 35.343 | 80.734 | 897.48 | 35.344 | 99.996 | 0.544 | 5.28 | 165.7 | 2.15 | 0.064 | ok |

## Insights

- **Peak throughput**: 18760.1 ops/s at 100 users (p99 35.343 ms).
- **Capacity at p99 ≤ 10 ms**: 0 users (DB latency); 0 users counting pool wait.
- **Capacity at p99 ≤ 50 ms**: 100 users (DB latency); 100 users counting pool wait.
- **Capacity at p99 ≤ 100 ms**: 100 users (DB latency); 100 users counting pool wait.
- **Capacity at p99 ≤ 250 ms**: 100 users (DB latency); 100 users counting pool wait.
- **Capacity at p99 ≤ 1000 ms**: 100 users (DB latency); 100 users counting pool wait.
- **Ops per server core-second**: 100→3553.
- **Read-your-writes violations**: none.
- **Business invariants**: all stages ok.
- **Offline integrity check** (elitesql check): ok in 7.4 s.
  - right after killing the server (no clean close): exit 3, 0 errors, 236074 warnings; after one clean open/close: exit 0, 0 errors, 0 warnings.
- **Database size**: 72.3 MiB after the first stage, 72.3 MiB after the last; 11699 paid orders in total.

## p99 latency per operation (ms)

| op | share % | 100 |
|---|---:|---:|
| add_to_cart | 10.09 | 40.16 |
| admin_dashboard | 1.05 | 98.22 |
| browse | 20.44 | 16.51 |
| checkout | 4.11 | 75.49 |
| order_history | 3.04 | 29.06 |
| product_detail | 17.21 | 25.96 |
| recommend | 7.1 | 17.66 |
| relogin | 1.54 | 60.90 |
| restock | 0.31 | 73.63 |
| search_text | 8.15 | 21.00 |
| session_check | 12.18 | 42.64 |
| signup | 0.51 | 44.33 |
| update_cart_item | 3.03 | 36.45 |
| update_profile | 1.01 | 33.37 |
| view_cart | 8.21 | 26.26 |
| write_review | 2.02 | 33.75 |

## Errors and business outcomes at the highest level

```json
{
 "statuses": {
  "ok": 277885,
  "empty_cart": 3486,
  "not_found": 19,
  "conflict_exhausted": 12
 },
 "errors": {
  "conflict_exhausted": {
   "count": 12,
   "first": "[elitesql:9] conflict, retry transaction: products/b8 changed after this transaction began",
   "ops": {
    "restock": 1,
    "checkout": 11
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
