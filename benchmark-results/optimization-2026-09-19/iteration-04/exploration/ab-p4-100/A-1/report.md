# Mini-SaaS concurrency simulation — sidecar

Run: `A-1` on 2026-09-23T13:38:46 · commit `92e0290` · macOS-26.6.2-arm64-arm-64bit · 10 CPUs

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
- **seed time**: 1.2 s, 13.1 MiB after seeding

## Capacity and performance curve

Latencies are of the database call (service operation) in milliseconds; `user p99` adds the wait for a pooled connection.

| users | conns | ops/s | reads/s | writes/s | p50 | p95 | p99 | p99.9 | max | user p99 | success % | conflict retry % | srv CPU | srv RSS MiB | gen CPU | thr CV | invariants |
|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|
| 100 | 100 | 17873.7 | 13826.9 | 4046.8 | 2.085 | 19.339 | 36.285 | 78.523 | 918.236 | 36.287 | 99.997 | 0.527 | 5.09 | 160.0 | 2.09 | 0.079 | ok |

## Insights

- **Peak throughput**: 17873.7 ops/s at 100 users (p99 36.285 ms).
- **Capacity at p99 ≤ 10 ms**: 0 users (DB latency); 0 users counting pool wait.
- **Capacity at p99 ≤ 50 ms**: 100 users (DB latency); 100 users counting pool wait.
- **Capacity at p99 ≤ 100 ms**: 100 users (DB latency); 100 users counting pool wait.
- **Capacity at p99 ≤ 250 ms**: 100 users (DB latency); 100 users counting pool wait.
- **Capacity at p99 ≤ 1000 ms**: 100 users (DB latency); 100 users counting pool wait.
- **Ops per server core-second**: 100→3512.
- **Read-your-writes violations**: none.
- **Business invariants**: all stages ok.
- **Offline integrity check** (elitesql check): ok in 7.31 s.
  - right after killing the server (no clean close): exit 3, 0 errors, 227812 warnings; after one clean open/close: exit 0, 0 errors, 0 warnings.
- **Database size**: 68.8 MiB after the first stage, 68.8 MiB after the last; 11130 paid orders in total.

## p99 latency per operation (ms)

| op | share % | 100 |
|---|---:|---:|
| add_to_cart | 10.11 | 42.26 |
| admin_dashboard | 1.04 | 108.81 |
| browse | 20.46 | 18.63 |
| checkout | 4.11 | 63.30 |
| order_history | 3.04 | 30.38 |
| product_detail | 17.21 | 26.31 |
| recommend | 7.11 | 17.48 |
| relogin | 1.53 | 63.38 |
| restock | 0.31 | 84.98 |
| search_text | 8.14 | 19.44 |
| session_check | 12.16 | 43.06 |
| signup | 0.51 | 40.15 |
| update_cart_item | 3.04 | 34.79 |
| update_profile | 1.01 | 31.81 |
| view_cart | 8.19 | 24.26 |
| write_review | 2.03 | 35.91 |

## Errors and business outcomes at the highest level

```json
{
 "statuses": {
  "ok": 264752,
  "empty_cart": 3329,
  "not_found": 18,
  "conflict_exhausted": 7
 },
 "errors": {
  "conflict_exhausted": {
   "count": 12,
   "first": "[elitesql:9] conflict, retry transaction: products/d180 changed after this transaction began",
   "ops": {
    "checkout": 12
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
