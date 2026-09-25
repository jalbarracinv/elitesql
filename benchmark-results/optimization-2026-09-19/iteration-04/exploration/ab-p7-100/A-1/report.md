# Mini-SaaS concurrency simulation — sidecar

Run: `A-1` on 2026-09-23T14:31:00 · commit `92e0290` · macOS-26.6.2-arm64-arm-64bit · 10 CPUs

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
- **seed time**: 1.11 s, 13.1 MiB after seeding

## Capacity and performance curve

Latencies are of the database call (service operation) in milliseconds; `user p99` adds the wait for a pooled connection.

| users | conns | ops/s | reads/s | writes/s | p50 | p95 | p99 | p99.9 | max | user p99 | success % | conflict retry % | srv CPU | srv RSS MiB | gen CPU | thr CV | invariants |
|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|
| 100 | 100 | 23836.5 | 18445.8 | 5390.7 | 0.859 | 14.758 | 25.856 | 66.744 | 929.957 | 25.857 | 99.987 | 0.696 | 6.11 | 191.7 | 2.58 | 0.073 | ok |

## Insights

- **Peak throughput**: 23836.5 ops/s at 100 users (p99 25.856 ms).
- **Capacity at p99 ≤ 10 ms**: 0 users (DB latency); 0 users counting pool wait.
- **Capacity at p99 ≤ 50 ms**: 100 users (DB latency); 100 users counting pool wait.
- **Capacity at p99 ≤ 100 ms**: 100 users (DB latency); 100 users counting pool wait.
- **Capacity at p99 ≤ 250 ms**: 100 users (DB latency); 100 users counting pool wait.
- **Capacity at p99 ≤ 1000 ms**: 100 users (DB latency); 100 users counting pool wait.
- **Ops per server core-second**: 100→3901.
- **Read-your-writes violations**: none.
- **Business invariants**: all stages ok.
- **Offline integrity check** (elitesql check): ok in 8.44 s.
  - right after killing the server (no clean close): exit 3, 0 errors, 285886 warnings; after one clean open/close: exit 0, 0 errors, 0 warnings.
- **Database size**: 88.6 MiB after the first stage, 88.6 MiB after the last; 14992 paid orders in total.

## p99 latency per operation (ms)

| op | share % | 100 |
|---|---:|---:|
| add_to_cart | 10.11 | 31.75 |
| admin_dashboard | 1.03 | 106.46 |
| browse | 20.42 | 6.18 |
| checkout | 4.07 | 79.15 |
| order_history | 3.01 | 15.26 |
| product_detail | 17.23 | 10.25 |
| recommend | 7.15 | 9.47 |
| relogin | 1.54 | 40.22 |
| restock | 0.3 | 86.80 |
| search_text | 8.18 | 8.57 |
| session_check | 12.17 | 22.89 |
| signup | 0.5 | 26.03 |
| update_cart_item | 3.03 | 23.85 |
| update_profile | 1.03 | 19.71 |
| view_cart | 8.19 | 11.61 |
| write_review | 2.03 | 23.82 |

## Errors and business outcomes at the highest level

```json
{
 "statuses": {
  "ok": 353166,
  "empty_cart": 4313,
  "not_found": 21,
  "conflict_exhausted": 48
 },
 "errors": {
  "conflict_exhausted": {
   "count": 73,
   "first": "[elitesql:9] conflict, retry transaction: products/c20 changed after this transaction began",
   "ops": {
    "checkout": 69,
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
