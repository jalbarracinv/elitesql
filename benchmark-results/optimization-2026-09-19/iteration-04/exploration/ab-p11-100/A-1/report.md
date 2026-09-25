# Mini-SaaS concurrency simulation — sidecar

Run: `A-1` on 2026-09-23T14:55:51 · commit `92e0290` · macOS-26.6.2-arm64-arm-64bit · 10 CPUs

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
- **seed time**: 1.1 s, 13.1 MiB after seeding

## Capacity and performance curve

Latencies are of the database call (service operation) in milliseconds; `user p99` adds the wait for a pooled connection.

| users | conns | ops/s | reads/s | writes/s | p50 | p95 | p99 | p99.9 | max | user p99 | success % | conflict retry % | srv CPU | srv RSS MiB | gen CPU | thr CV | invariants |
|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|
| 100 | 100 | 23175.6 | 17927.3 | 5248.3 | 0.858 | 15.147 | 27.168 | 69.445 | 1049.956 | 27.169 | 99.98 | 0.717 | 6.06 | 188.2 | 2.59 | 0.069 | ok |

## Insights

- **Peak throughput**: 23175.6 ops/s at 100 users (p99 27.168 ms).
- **Capacity at p99 ≤ 10 ms**: 0 users (DB latency); 0 users counting pool wait.
- **Capacity at p99 ≤ 50 ms**: 100 users (DB latency); 100 users counting pool wait.
- **Capacity at p99 ≤ 100 ms**: 100 users (DB latency); 100 users counting pool wait.
- **Capacity at p99 ≤ 250 ms**: 100 users (DB latency); 100 users counting pool wait.
- **Capacity at p99 ≤ 1000 ms**: 100 users (DB latency); 100 users counting pool wait.
- **Ops per server core-second**: 100→3824.
- **Read-your-writes violations**: none.
- **Business invariants**: all stages ok.
- **Offline integrity check** (elitesql check): ok in 8.22 s.
  - right after killing the server (no clean close): exit 3, 0 errors, 280774 warnings; after one clean open/close: exit 0, 0 errors, 0 warnings.
- **Database size**: 87.2 MiB after the first stage, 87.2 MiB after the last; 14613 paid orders in total.

## p99 latency per operation (ms)

| op | share % | 100 |
|---|---:|---:|
| add_to_cart | 10.14 | 32.73 |
| admin_dashboard | 1.02 | 106.73 |
| browse | 20.4 | 6.44 |
| checkout | 4.06 | 86.33 |
| order_history | 3.0 | 15.65 |
| product_detail | 17.23 | 11.01 |
| recommend | 7.15 | 9.16 |
| relogin | 1.54 | 43.54 |
| restock | 0.3 | 99.42 |
| search_text | 8.19 | 8.55 |
| session_check | 12.17 | 24.56 |
| signup | 0.51 | 25.65 |
| update_cart_item | 3.04 | 27.02 |
| update_profile | 1.03 | 19.14 |
| view_cart | 8.19 | 11.84 |
| write_review | 2.04 | 27.30 |

## Errors and business outcomes at the highest level

```json
{
 "statuses": {
  "ok": 343379,
  "empty_cart": 4164,
  "not_found": 20,
  "conflict_exhausted": 71
 },
 "errors": {
  "conflict_exhausted": {
   "count": 95,
   "first": "[elitesql:9] conflict, retry transaction: products/b2 changed after this transaction began",
   "ops": {
    "checkout": 90,
    "restock": 5
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
