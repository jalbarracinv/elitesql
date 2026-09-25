# Mini-SaaS concurrency simulation — sidecar

Run: `A-1` on 2026-09-23T13:57:28 · commit `92e0290` · macOS-26.6.2-arm64-arm-64bit · 10 CPUs

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
| 100 | 100 | 21521.7 | 16648.0 | 4873.7 | 1.621 | 15.825 | 29.711 | 70.413 | 930.123 | 29.712 | 99.991 | 0.466 | 6.08 | 181.2 | 2.34 | 0.151 | ok |

## Insights

- **Peak throughput**: 21521.7 ops/s at 100 users (p99 29.711 ms).
- **Capacity at p99 ≤ 10 ms**: 0 users (DB latency); 0 users counting pool wait.
- **Capacity at p99 ≤ 50 ms**: 100 users (DB latency); 100 users counting pool wait.
- **Capacity at p99 ≤ 100 ms**: 100 users (DB latency); 100 users counting pool wait.
- **Capacity at p99 ≤ 250 ms**: 100 users (DB latency); 100 users counting pool wait.
- **Capacity at p99 ≤ 1000 ms**: 100 users (DB latency); 100 users counting pool wait.
- **Ops per server core-second**: 100→3540.
- **Read-your-writes violations**: none.
- **Business invariants**: all stages ok.
- **Offline integrity check** (elitesql check): ok in 7.77 s.
  - right after killing the server (no clean close): exit 3, 0 errors, 265164 warnings; after one clean open/close: exit 0, 0 errors, 0 warnings.
- **Database size**: 82.3 MiB after the first stage, 82.3 MiB after the last; 13587 paid orders in total.

## p99 latency per operation (ms)

| op | share % | 100 |
|---|---:|---:|
| add_to_cart | 10.14 | 33.93 |
| admin_dashboard | 1.02 | 78.04 |
| browse | 20.39 | 15.49 |
| checkout | 4.06 | 60.95 |
| order_history | 3.02 | 25.34 |
| product_detail | 17.22 | 21.12 |
| recommend | 7.13 | 14.43 |
| relogin | 1.54 | 50.09 |
| restock | 0.3 | 47.98 |
| search_text | 8.17 | 18.22 |
| session_check | 12.18 | 35.68 |
| signup | 0.51 | 34.90 |
| update_cart_item | 3.03 | 28.46 |
| update_profile | 1.03 | 28.03 |
| view_cart | 8.22 | 21.10 |
| write_review | 2.03 | 28.40 |

## Errors and business outcomes at the highest level

```json
{
 "statuses": {
  "ok": 318846,
  "empty_cart": 3929,
  "not_found": 21,
  "conflict_exhausted": 30
 },
 "errors": {
  "conflict_exhausted": {
   "count": 38,
   "first": "[elitesql:9] conflict, retry transaction: products/c17 changed after this transaction began",
   "ops": {
    "checkout": 38
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
