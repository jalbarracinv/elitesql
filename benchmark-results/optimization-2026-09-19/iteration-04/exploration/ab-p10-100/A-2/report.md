# Mini-SaaS concurrency simulation — sidecar

Run: `A-2` on 2026-09-23T14:51:01 · commit `92e0290` · macOS-26.6.2-arm64-arm-64bit · 10 CPUs

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
| 100 | 100 | 22452.7 | 17370.0 | 5082.7 | 0.913 | 15.651 | 27.506 | 75.716 | 960.14 | 27.507 | 99.984 | 0.738 | 5.57 | 182.6 | 2.58 | 0.106 | ok |

## Insights

- **Peak throughput**: 22452.7 ops/s at 100 users (p99 27.506 ms).
- **Capacity at p99 ≤ 10 ms**: 0 users (DB latency); 0 users counting pool wait.
- **Capacity at p99 ≤ 50 ms**: 100 users (DB latency); 100 users counting pool wait.
- **Capacity at p99 ≤ 100 ms**: 100 users (DB latency); 100 users counting pool wait.
- **Capacity at p99 ≤ 250 ms**: 100 users (DB latency); 100 users counting pool wait.
- **Capacity at p99 ≤ 1000 ms**: 100 users (DB latency); 100 users counting pool wait.
- **Ops per server core-second**: 100→4031.
- **Read-your-writes violations**: none.
- **Business invariants**: all stages ok.
- **Offline integrity check** (elitesql check): ok in 8.72 s.
  - right after killing the server (no clean close): exit 3, 0 errors, 278008 warnings; after one clean open/close: exit 0, 0 errors, 0 warnings.
- **Database size**: 86.3 MiB after the first stage, 86.3 MiB after the last; 14463 paid orders in total.

## p99 latency per operation (ms)

| op | share % | 100 |
|---|---:|---:|
| add_to_cart | 10.12 | 33.66 |
| admin_dashboard | 1.02 | 116.96 |
| browse | 20.38 | 6.78 |
| checkout | 4.07 | 97.76 |
| order_history | 3.02 | 16.34 |
| product_detail | 17.23 | 11.64 |
| recommend | 7.15 | 10.85 |
| relogin | 1.54 | 45.20 |
| restock | 0.3 | 99.65 |
| search_text | 8.2 | 9.37 |
| session_check | 12.16 | 25.38 |
| signup | 0.51 | 29.53 |
| update_cart_item | 3.02 | 26.01 |
| update_profile | 1.03 | 20.67 |
| view_cart | 8.2 | 12.35 |
| write_review | 2.04 | 25.83 |

## Errors and business outcomes at the highest level

```json
{
 "statuses": {
  "ok": 332659,
  "empty_cart": 4058,
  "conflict_exhausted": 54,
  "not_found": 20
 },
 "errors": {
  "conflict_exhausted": {
   "count": 75,
   "first": "[elitesql:9] conflict, retry transaction: products/b2 changed after this transaction began",
   "ops": {
    "checkout": 72,
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
