# Mini-SaaS concurrency simulation — sidecar

Run: `A-2` on 2026-09-23T14:44:25 · commit `92e0290` · macOS-26.6.2-arm64-arm-64bit · 10 CPUs

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
| 100 | 100 | 23637.4 | 18289.7 | 5347.7 | 0.897 | 14.845 | 25.924 | 68.646 | 980.39 | 25.925 | 99.988 | 0.725 | 6.07 | 185.6 | 2.59 | 0.074 | ok |

## Insights

- **Peak throughput**: 23637.4 ops/s at 100 users (p99 25.924 ms).
- **Capacity at p99 ≤ 10 ms**: 0 users (DB latency); 0 users counting pool wait.
- **Capacity at p99 ≤ 50 ms**: 100 users (DB latency); 100 users counting pool wait.
- **Capacity at p99 ≤ 100 ms**: 100 users (DB latency); 100 users counting pool wait.
- **Capacity at p99 ≤ 250 ms**: 100 users (DB latency); 100 users counting pool wait.
- **Capacity at p99 ≤ 1000 ms**: 100 users (DB latency); 100 users counting pool wait.
- **Ops per server core-second**: 100→3894.
- **Read-your-writes violations**: none.
- **Business invariants**: all stages ok.
- **Offline integrity check** (elitesql check): ok in 8.29 s.
  - right after killing the server (no clean close): exit 3, 0 errors, 285052 warnings; after one clean open/close: exit 0, 0 errors, 0 warnings.
- **Database size**: 88.4 MiB after the first stage, 88.4 MiB after the last; 14952 paid orders in total.

## p99 latency per operation (ms)

| op | share % | 100 |
|---|---:|---:|
| add_to_cart | 10.12 | 30.33 |
| admin_dashboard | 1.03 | 113.83 |
| browse | 20.4 | 5.90 |
| checkout | 4.07 | 86.61 |
| order_history | 3.02 | 14.92 |
| product_detail | 17.22 | 10.98 |
| recommend | 7.15 | 10.03 |
| relogin | 1.54 | 40.03 |
| restock | 0.3 | 98.97 |
| search_text | 8.18 | 8.41 |
| session_check | 12.18 | 22.90 |
| signup | 0.5 | 23.38 |
| update_cart_item | 3.03 | 24.81 |
| update_profile | 1.03 | 19.91 |
| view_cart | 8.2 | 11.48 |
| write_review | 2.04 | 25.16 |

## Errors and business outcomes at the highest level

```json
{
 "statuses": {
  "ok": 350230,
  "empty_cart": 4267,
  "not_found": 22,
  "conflict_exhausted": 42
 },
 "errors": {
  "conflict_exhausted": {
   "count": 65,
   "first": "[elitesql:9] conflict, retry transaction: products/b6 changed after this transaction began",
   "ops": {
    "checkout": 61,
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
