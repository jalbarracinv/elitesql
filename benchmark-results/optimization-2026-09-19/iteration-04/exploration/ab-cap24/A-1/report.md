# Mini-SaaS concurrency simulation — sidecar

Run: `A-1` on 2026-09-23T14:12:33 · commit `92e0290` · macOS-26.6.2-arm64-arm-64bit · 10 CPUs

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
| 100 | 100 | 23630.1 | 18281.7 | 5348.3 | 0.872 | 14.98 | 26.108 | 66.954 | 1025.806 | 26.109 | 99.984 | 0.714 | 6.02 | 188.2 | 2.61 | 0.078 | ok |

## Insights

- **Peak throughput**: 23630.1 ops/s at 100 users (p99 26.108 ms).
- **Capacity at p99 ≤ 10 ms**: 0 users (DB latency); 0 users counting pool wait.
- **Capacity at p99 ≤ 50 ms**: 100 users (DB latency); 100 users counting pool wait.
- **Capacity at p99 ≤ 100 ms**: 100 users (DB latency); 100 users counting pool wait.
- **Capacity at p99 ≤ 250 ms**: 100 users (DB latency); 100 users counting pool wait.
- **Capacity at p99 ≤ 1000 ms**: 100 users (DB latency); 100 users counting pool wait.
- **Ops per server core-second**: 100→3925.
- **Read-your-writes violations**: none.
- **Business invariants**: all stages ok.
- **Offline integrity check** (elitesql check): ok in 8.34 s.
  - right after killing the server (no clean close): exit 3, 0 errors, 285248 warnings; after one clean open/close: exit 0, 0 errors, 0 warnings.
- **Database size**: 88.4 MiB after the first stage, 88.4 MiB after the last; 14964 paid orders in total.

## p99 latency per operation (ms)

| op | share % | 100 |
|---|---:|---:|
| add_to_cart | 10.13 | 30.68 |
| admin_dashboard | 1.03 | 105.37 |
| browse | 20.37 | 5.84 |
| checkout | 4.08 | 79.64 |
| order_history | 3.01 | 13.88 |
| product_detail | 17.23 | 9.68 |
| recommend | 7.17 | 9.91 |
| relogin | 1.54 | 43.92 |
| restock | 0.3 | 95.66 |
| search_text | 8.18 | 8.72 |
| session_check | 12.18 | 23.76 |
| signup | 0.51 | 26.57 |
| update_cart_item | 3.02 | 25.77 |
| update_profile | 1.02 | 21.96 |
| view_cart | 8.19 | 10.50 |
| write_review | 2.03 | 24.64 |

## Errors and business outcomes at the highest level

```json
{
 "statuses": {
  "ok": 350110,
  "empty_cart": 4263,
  "not_found": 21,
  "conflict_exhausted": 57
 },
 "errors": {
  "conflict_exhausted": {
   "count": 71,
   "first": "[elitesql:9] conflict, retry transaction: products/d172 changed after this transaction began",
   "ops": {
    "checkout": 66,
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
