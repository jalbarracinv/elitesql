# Mini-SaaS concurrency simulation — sidecar

Run: `B-3` on 2026-09-23T13:05:44 · commit `92e0290` · macOS-26.6.2-arm64-arm-64bit · 10 CPUs

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
| 100 | 100 | 10747.6 | 8323.1 | 2424.5 | 3.564 | 31.264 | 67.766 | 136.4 | 1185.906 | 67.768 | 99.989 | 0.732 | 6.02 | 112.0 | 1.39 | 0.091 | ok |

## Insights

- **Peak throughput**: 10747.6 ops/s at 100 users (p99 67.766 ms).
- **Capacity at p99 ≤ 10 ms**: 0 users (DB latency); 0 users counting pool wait.
- **Capacity at p99 ≤ 50 ms**: 0 users (DB latency); 0 users counting pool wait.
- **Capacity at p99 ≤ 100 ms**: 100 users (DB latency); 100 users counting pool wait.
- **Capacity at p99 ≤ 250 ms**: 100 users (DB latency); 100 users counting pool wait.
- **Capacity at p99 ≤ 1000 ms**: 100 users (DB latency); 100 users counting pool wait.
- **Ops per server core-second**: 100→1785.
- **Read-your-writes violations**: none.
- **Business invariants**: all stages ok.
- **Offline integrity check** (elitesql check): ok in 5.09 s.
  - right after killing the server (no clean close): exit 3, 0 errors, 163914 warnings; after one clean open/close: exit 0, 0 errors, 0 warnings.
- **Database size**: 46.9 MiB after the first stage, 46.9 MiB after the last; 6831 paid orders in total.

## p99 latency per operation (ms)

| op | share % | 100 |
|---|---:|---:|
| add_to_cart | 10.02 | 72.81 |
| admin_dashboard | 1.05 | 141.47 |
| browse | 20.43 | 80.12 |
| checkout | 4.08 | 154.62 |
| order_history | 3.04 | 55.85 |
| product_detail | 17.3 | 25.96 |
| recommend | 7.13 | 18.13 |
| relogin | 1.58 | 95.89 |
| restock | 0.32 | 147.35 |
| search_text | 8.13 | 31.43 |
| session_check | 12.21 | 62.46 |
| signup | 0.49 | 70.52 |
| update_cart_item | 2.99 | 58.83 |
| update_profile | 1.03 | 33.20 |
| view_cart | 8.15 | 41.65 |
| write_review | 2.03 | 58.44 |

## Errors and business outcomes at the highest level

```json
{
 "statuses": {
  "ok": 159168,
  "empty_cart": 2018,
  "not_found": 11,
  "conflict_exhausted": 17
 },
 "errors": {
  "conflict_exhausted": {
   "count": 19,
   "first": "[elitesql:9] conflict, retry transaction: products/c12 changed after this transaction began",
   "ops": {
    "checkout": 18,
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
