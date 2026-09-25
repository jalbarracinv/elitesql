# Mini-SaaS concurrency simulation — sidecar

Run: `A-2` on 2026-09-23T14:02:41 · commit `92e0290` · macOS-26.6.2-arm64-arm-64bit · 10 CPUs

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
- **seed time**: 1.14 s, 13.1 MiB after seeding

## Capacity and performance curve

Latencies are of the database call (service operation) in milliseconds; `user p99` adds the wait for a pooled connection.

| users | conns | ops/s | reads/s | writes/s | p50 | p95 | p99 | p99.9 | max | user p99 | success % | conflict retry % | srv CPU | srv RSS MiB | gen CPU | thr CV | invariants |
|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|
| 100 | 100 | 23690.7 | 18327.1 | 5363.6 | 0.877 | 14.884 | 26.466 | 68.508 | 1059.626 | 26.467 | 99.986 | 0.716 | 6.05 | 188.2 | 2.59 | 0.074 | ok |

## Insights

- **Peak throughput**: 23690.7 ops/s at 100 users (p99 26.466 ms).
- **Capacity at p99 ≤ 10 ms**: 0 users (DB latency); 0 users counting pool wait.
- **Capacity at p99 ≤ 50 ms**: 100 users (DB latency); 100 users counting pool wait.
- **Capacity at p99 ≤ 100 ms**: 100 users (DB latency); 100 users counting pool wait.
- **Capacity at p99 ≤ 250 ms**: 100 users (DB latency); 100 users counting pool wait.
- **Capacity at p99 ≤ 1000 ms**: 100 users (DB latency); 100 users counting pool wait.
- **Ops per server core-second**: 100→3916.
- **Read-your-writes violations**: none.
- **Business invariants**: all stages ok.
- **Offline integrity check** (elitesql check): ok in 8.41 s.
  - right after killing the server (no clean close): exit 3, 0 errors, 284724 warnings; after one clean open/close: exit 0, 0 errors, 0 warnings.
- **Database size**: 88.3 MiB after the first stage, 88.3 MiB after the last; 14921 paid orders in total.

## p99 latency per operation (ms)

| op | share % | 100 |
|---|---:|---:|
| add_to_cart | 10.12 | 31.34 |
| admin_dashboard | 1.03 | 99.22 |
| browse | 20.39 | 5.82 |
| checkout | 4.07 | 83.42 |
| order_history | 3.01 | 14.33 |
| product_detail | 17.23 | 10.04 |
| recommend | 7.16 | 9.94 |
| relogin | 1.54 | 40.47 |
| restock | 0.3 | 102.79 |
| search_text | 8.18 | 8.32 |
| session_check | 12.18 | 24.76 |
| signup | 0.51 | 29.19 |
| update_cart_item | 3.03 | 25.86 |
| update_profile | 1.03 | 21.66 |
| view_cart | 8.19 | 12.04 |
| write_review | 2.04 | 24.22 |

## Errors and business outcomes at the highest level

```json
{
 "statuses": {
  "ok": 350999,
  "empty_cart": 4290,
  "not_found": 22,
  "conflict_exhausted": 49
 },
 "errors": {
  "conflict_exhausted": {
   "count": 65,
   "first": "[elitesql:9] conflict, retry transaction: products/b8 changed after this transaction began",
   "ops": {
    "checkout": 59,
    "restock": 6
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
