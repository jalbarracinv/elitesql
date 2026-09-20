# Mini-SaaS concurrency simulation — sidecar

Run: `before-100` on 2026-09-19T23:25:40 · commit `b4dda86` · macOS-26.6.2-arm64-arm-64bit · 10 CPUs

## Configuration

- **transport**: sidecar
- **levels**: [100]
- **duration**: 20.0
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
| 100 | 100 | 16700.8 | 12924.5 | 3776.3 | 2.107 | 21.062 | 43.42 | 93.974 | 969.394 | 43.422 | 99.999 | 0.491 | 4.91 | 174.2 | 1.97 | 0.089 | ok |

## Insights

- **Peak throughput**: 16700.8 ops/s at 100 users (p99 43.42 ms).
- **Capacity at p99 ≤ 10 ms**: 0 users (DB latency); 0 users counting pool wait.
- **Capacity at p99 ≤ 50 ms**: 100 users (DB latency); 100 users counting pool wait.
- **Capacity at p99 ≤ 100 ms**: 100 users (DB latency); 100 users counting pool wait.
- **Capacity at p99 ≤ 250 ms**: 100 users (DB latency); 100 users counting pool wait.
- **Capacity at p99 ≤ 1000 ms**: 100 users (DB latency); 100 users counting pool wait.
- **Ops per server core-second**: 100→3401.
- **Read-your-writes violations**: none.
- **Business invariants**: all stages ok.
- **Offline integrity check** (elitesql check): ok in 8.16 s.
  - right after killing the server (no clean close): exit 3, 0 errors, 255388 warnings; after one clean open/close: exit 0, 0 errors, 0 warnings.
- **Database size**: 78.2 MiB after the first stage, 78.2 MiB after the last; 12961 paid orders in total.

## p99 latency per operation (ms)

| op | share % | 100 |
|---|---:|---:|
| add_to_cart | 10.13 | 50.71 |
| admin_dashboard | 1.03 | 136.79 |
| browse | 20.41 | 21.72 |
| checkout | 4.07 | 72.15 |
| order_history | 3.03 | 40.12 |
| product_detail | 17.22 | 30.31 |
| recommend | 7.11 | 20.78 |
| relogin | 1.54 | 71.98 |
| restock | 0.3 | 107.41 |
| search_text | 8.17 | 23.46 |
| session_check | 12.2 | 51.51 |
| signup | 0.5 | 48.14 |
| update_cart_item | 3.02 | 43.54 |
| update_profile | 1.02 | 42.90 |
| view_cart | 8.22 | 29.26 |
| write_review | 2.03 | 42.27 |

## Errors and business outcomes at the highest level

```json
{
 "statuses": {
  "ok": 329917,
  "empty_cart": 4070,
  "not_found": 23,
  "conflict_exhausted": 5
 },
 "errors": {
  "conflict_exhausted": {
   "count": 9,
   "first": "[elitesql:9] conflict, retry transaction: products/b7 changed after this transaction began",
   "ops": {
    "checkout": 9
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
