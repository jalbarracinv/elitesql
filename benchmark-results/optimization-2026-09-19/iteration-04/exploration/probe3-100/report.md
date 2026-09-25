# Mini-SaaS concurrency simulation — sidecar

Run: `probe3-100` on 2026-09-23T13:32:21 · commit `92e0290` · macOS-26.6.2-arm64-arm-64bit · 10 CPUs

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
- **seed time**: 1.23 s, 13.1 MiB after seeding

## Capacity and performance curve

Latencies are of the database call (service operation) in milliseconds; `user p99` adds the wait for a pooled connection.

| users | conns | ops/s | reads/s | writes/s | p50 | p95 | p99 | p99.9 | max | user p99 | success % | conflict retry % | srv CPU | srv RSS MiB | gen CPU | thr CV | invariants |
|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|
| 100 | 100 | 13321.5 | 10310.9 | 3010.6 | 2.357 | 24.872 | 49.481 | 156.774 | 1060.088 | 49.483 | 99.948 | 0.725 | 5.37 | 136.2 | 1.91 | 0.07 | ok |

## Insights

- **Peak throughput**: 13321.5 ops/s at 100 users (p99 49.481 ms).
- **Capacity at p99 ≤ 10 ms**: 0 users (DB latency); 0 users counting pool wait.
- **Capacity at p99 ≤ 50 ms**: 100 users (DB latency); 100 users counting pool wait.
- **Capacity at p99 ≤ 100 ms**: 100 users (DB latency); 100 users counting pool wait.
- **Capacity at p99 ≤ 250 ms**: 100 users (DB latency); 100 users counting pool wait.
- **Capacity at p99 ≤ 1000 ms**: 100 users (DB latency); 100 users counting pool wait.
- **Ops per server core-second**: 100→2481.
- **Read-your-writes violations**: none.
- **Business invariants**: all stages ok.
- **Offline integrity check** (elitesql check): ok in 6.38 s.
  - right after killing the server (no clean close): exit 3, 0 errors, 189770 warnings; after one clean open/close: exit 0, 0 errors, 0 warnings.
- **Database size**: 56.2 MiB after the first stage, 56.2 MiB after the last; 8509 paid orders in total.

## p99 latency per operation (ms)

| op | share % | 100 |
|---|---:|---:|
| add_to_cart | 10.09 | 55.19 |
| admin_dashboard | 1.03 | 174.57 |
| browse | 20.57 | 19.88 |
| checkout | 4.07 | 346.45 |
| order_history | 3.06 | 39.48 |
| product_detail | 17.2 | 20.61 |
| recommend | 7.12 | 14.41 |
| relogin | 1.55 | 76.92 |
| restock | 0.3 | 78.91 |
| search_text | 8.11 | 23.15 |
| session_check | 12.16 | 47.16 |
| signup | 0.5 | 57.45 |
| update_cart_item | 3.06 | 44.92 |
| update_profile | 1.01 | 27.54 |
| view_cart | 8.15 | 32.86 |
| write_review | 2.02 | 49.06 |

## Errors and business outcomes at the highest level

```json
{
 "statuses": {
  "ok": 197267,
  "empty_cart": 2437,
  "conflict_exhausted": 103,
  "not_found": 15
 },
 "errors": {
  "conflict_exhausted": {
   "count": 121,
   "first": "[elitesql:9] conflict, retry transaction: products/c15 changed after this transaction began",
   "ops": {
    "checkout": 121
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
