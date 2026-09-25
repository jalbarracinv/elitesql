# Mini-SaaS concurrency simulation — sidecar

Run: `probe-100` on 2026-09-23T12:56:59 · commit `92e0290` · macOS-26.6.2-arm64-arm-64bit · 10 CPUs

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
- **seed time**: 1.19 s, 13.1 MiB after seeding

## Capacity and performance curve

Latencies are of the database call (service operation) in milliseconds; `user p99` adds the wait for a pooled connection.

| users | conns | ops/s | reads/s | writes/s | p50 | p95 | p99 | p99.9 | max | user p99 | success % | conflict retry % | srv CPU | srv RSS MiB | gen CPU | thr CV | invariants |
|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|
| 100 | 100 | 16363.6 | 12662.1 | 3701.5 | 1.508 | 21.939 | 47.262 | 124.608 | 1072.583 | 47.265 | 99.976 | 0.721 | 5.27 | 154.6 | 2.27 | 0.086 | ok |

## Insights

- **Peak throughput**: 16363.6 ops/s at 100 users (p99 47.262 ms).
- **Capacity at p99 ≤ 10 ms**: 0 users (DB latency); 0 users counting pool wait.
- **Capacity at p99 ≤ 50 ms**: 100 users (DB latency); 100 users counting pool wait.
- **Capacity at p99 ≤ 100 ms**: 100 users (DB latency); 100 users counting pool wait.
- **Capacity at p99 ≤ 250 ms**: 100 users (DB latency); 100 users counting pool wait.
- **Capacity at p99 ≤ 1000 ms**: 100 users (DB latency); 100 users counting pool wait.
- **Ops per server core-second**: 100→3105.
- **Read-your-writes violations**: none.
- **Business invariants**: all stages ok.
- **Offline integrity check** (elitesql check): ok in 6.97 s.
  - right after killing the server (no clean close): exit 3, 0 errors, 217462 warnings; after one clean open/close: exit 0, 0 errors, 0 warnings.
- **Database size**: 65.8 MiB after the first stage, 65.8 MiB after the last; 10419 paid orders in total.

## p99 latency per operation (ms)

| op | share % | 100 |
|---|---:|---:|
| add_to_cart | 10.1 | 52.03 |
| admin_dashboard | 1.03 | 164.10 |
| browse | 20.51 | 33.91 |
| checkout | 4.1 | 148.85 |
| order_history | 3.05 | 38.27 |
| product_detail | 17.2 | 26.45 |
| recommend | 7.11 | 23.18 |
| relogin | 1.52 | 66.34 |
| restock | 0.31 | 91.54 |
| search_text | 8.15 | 20.32 |
| session_check | 12.11 | 44.96 |
| signup | 0.5 | 53.06 |
| update_cart_item | 3.04 | 40.17 |
| update_profile | 1.01 | 30.17 |
| view_cart | 8.2 | 34.17 |
| write_review | 2.03 | 41.23 |

## Errors and business outcomes at the highest level

```json
{
 "statuses": {
  "ok": 242338,
  "empty_cart": 3040,
  "conflict_exhausted": 60,
  "not_found": 16
 },
 "errors": {
  "conflict_exhausted": {
   "count": 73,
   "first": "[elitesql:9] conflict, retry transaction: products/b8 changed after this transaction began",
   "ops": {
    "checkout": 73
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
