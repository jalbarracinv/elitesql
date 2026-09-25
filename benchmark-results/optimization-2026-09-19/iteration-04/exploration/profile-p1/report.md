# Mini-SaaS concurrency simulation — sidecar

Run: `profile-p1` on 2026-09-23T12:54:07 · commit `92e0290` · macOS-26.6.2-arm64-arm-64bit · 10 CPUs

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
- **seed time**: 1.18 s, 13.1 MiB after seeding

## Capacity and performance curve

Latencies are of the database call (service operation) in milliseconds; `user p99` adds the wait for a pooled connection.

| users | conns | ops/s | reads/s | writes/s | p50 | p95 | p99 | p99.9 | max | user p99 | success % | conflict retry % | srv CPU | srv RSS MiB | gen CPU | thr CV | invariants |
|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|
| 100 | 100 | 12620.6 | 9764.2 | 2856.4 | 2.26 | 28.319 | 56.308 | 225.79 | 1607.331 | 56.311 | 99.962 | 0.74 | 4.12 | 156.7 | 1.84 | 0.269 | ok |

## Insights

- **Peak throughput**: 12620.6 ops/s at 100 users (p99 56.308 ms).
- **Capacity at p99 ≤ 10 ms**: 0 users (DB latency); 0 users counting pool wait.
- **Capacity at p99 ≤ 50 ms**: 0 users (DB latency); 0 users counting pool wait.
- **Capacity at p99 ≤ 100 ms**: 100 users (DB latency); 100 users counting pool wait.
- **Capacity at p99 ≤ 250 ms**: 100 users (DB latency); 100 users counting pool wait.
- **Capacity at p99 ≤ 1000 ms**: 100 users (DB latency); 100 users counting pool wait.
- **Ops per server core-second**: 100→3063.
- **Read-your-writes violations**: none.
- **Business invariants**: all stages ok.
- **Offline integrity check** (elitesql check): ok in 6.9 s.
  - right after killing the server (no clean close): exit 3, 0 errors, 218892 warnings; after one clean open/close: exit 0, 0 errors, 0 warnings.
- **Database size**: 66.3 MiB after the first stage, 66.3 MiB after the last; 10489 paid orders in total.

## p99 latency per operation (ms)

| op | share % | 100 |
|---|---:|---:|
| add_to_cart | 10.09 | 63.40 |
| admin_dashboard | 1.04 | 205.93 |
| browse | 20.49 | 32.96 |
| checkout | 4.09 | 260.96 |
| order_history | 3.05 | 42.35 |
| product_detail | 17.17 | 29.55 |
| recommend | 7.12 | 23.21 |
| relogin | 1.53 | 86.60 |
| restock | 0.31 | 105.64 |
| search_text | 8.15 | 19.23 |
| session_check | 12.14 | 51.63 |
| signup | 0.5 | 69.09 |
| update_cart_item | 3.05 | 47.12 |
| update_profile | 1.02 | 37.48 |
| view_cart | 8.21 | 33.35 |
| write_review | 2.04 | 46.87 |

## Errors and business outcomes at the highest level

```json
{
 "statuses": {
  "ok": 249186,
  "empty_cart": 3112,
  "not_found": 18,
  "conflict_exhausted": 97
 },
 "errors": {
  "conflict_exhausted": {
   "count": 114,
   "first": "[elitesql:9] conflict, retry transaction: products/b4 changed after this transaction began",
   "ops": {
    "checkout": 113,
    "restock": 1
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
