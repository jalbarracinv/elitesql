# Mini-SaaS concurrency simulation — sidecar

Run: `diag2-100` on 2026-09-23T14:36:48 · commit `92e0290` · macOS-26.6.2-arm64-arm-64bit · 10 CPUs

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
| 100 | 100 | 23704.3 | 18342.0 | 5362.3 | 0.833 | 14.86 | 26.279 | 72.219 | 1041.143 | 26.28 | 99.989 | 0.687 | 6.06 | 191.0 | 2.57 | 0.082 | ok |

## Insights

- **Peak throughput**: 23704.3 ops/s at 100 users (p99 26.279 ms).
- **Capacity at p99 ≤ 10 ms**: 0 users (DB latency); 0 users counting pool wait.
- **Capacity at p99 ≤ 50 ms**: 100 users (DB latency); 100 users counting pool wait.
- **Capacity at p99 ≤ 100 ms**: 100 users (DB latency); 100 users counting pool wait.
- **Capacity at p99 ≤ 250 ms**: 100 users (DB latency); 100 users counting pool wait.
- **Capacity at p99 ≤ 1000 ms**: 100 users (DB latency); 100 users counting pool wait.
- **Ops per server core-second**: 100→3912.
- **Read-your-writes violations**: none.
- **Business invariants**: all stages ok.
- **Offline integrity check** (elitesql check): ok in 8.4 s.
  - right after killing the server (no clean close): exit 3, 0 errors, 285400 warnings; after one clean open/close: exit 0, 0 errors, 0 warnings.
- **Database size**: 88.5 MiB after the first stage, 88.5 MiB after the last; 14968 paid orders in total.

## p99 latency per operation (ms)

| op | share % | 100 |
|---|---:|---:|
| add_to_cart | 10.12 | 31.34 |
| admin_dashboard | 1.03 | 128.02 |
| browse | 20.42 | 6.20 |
| checkout | 4.07 | 77.16 |
| order_history | 3.02 | 13.80 |
| product_detail | 17.24 | 10.92 |
| recommend | 7.15 | 9.60 |
| relogin | 1.54 | 40.79 |
| restock | 0.3 | 133.14 |
| search_text | 8.17 | 8.88 |
| session_check | 12.17 | 23.57 |
| signup | 0.51 | 27.79 |
| update_cart_item | 3.02 | 25.60 |
| update_profile | 1.03 | 20.75 |
| view_cart | 8.18 | 11.38 |
| write_review | 2.04 | 25.95 |

## Errors and business outcomes at the highest level

```json
{
 "statuses": {
  "ok": 351222,
  "empty_cart": 4282,
  "conflict_exhausted": 39,
  "not_found": 21
 },
 "errors": {
  "conflict_exhausted": {
   "count": 60,
   "first": "[elitesql:9] conflict, retry transaction: products/d193 changed after this transaction began",
   "ops": {
    "checkout": 56,
    "restock": 4
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
