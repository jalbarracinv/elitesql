# Mini-SaaS concurrency simulation — sidecar

Run: `A-1` on 2026-09-23T14:38:52 · commit `92e0290` · macOS-26.6.2-arm64-arm-64bit · 10 CPUs

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
| 100 | 100 | 23596.9 | 18253.5 | 5343.4 | 0.867 | 14.937 | 26.416 | 71.938 | 1115.804 | 26.417 | 99.979 | 0.721 | 6.08 | 201.0 | 2.65 | 0.082 | ok |

## Insights

- **Peak throughput**: 23596.9 ops/s at 100 users (p99 26.416 ms).
- **Capacity at p99 ≤ 10 ms**: 0 users (DB latency); 0 users counting pool wait.
- **Capacity at p99 ≤ 50 ms**: 100 users (DB latency); 100 users counting pool wait.
- **Capacity at p99 ≤ 100 ms**: 100 users (DB latency); 100 users counting pool wait.
- **Capacity at p99 ≤ 250 ms**: 100 users (DB latency); 100 users counting pool wait.
- **Capacity at p99 ≤ 1000 ms**: 100 users (DB latency); 100 users counting pool wait.
- **Ops per server core-second**: 100→3881.
- **Read-your-writes violations**: none.
- **Business invariants**: all stages ok.
- **Offline integrity check** (elitesql check): ok in 8.32 s.
  - right after killing the server (no clean close): exit 3, 0 errors, 284708 warnings; after one clean open/close: exit 0, 0 errors, 0 warnings.
- **Database size**: 88.3 MiB after the first stage, 88.3 MiB after the last; 14928 paid orders in total.

## p99 latency per operation (ms)

| op | share % | 100 |
|---|---:|---:|
| add_to_cart | 10.12 | 30.64 |
| admin_dashboard | 1.03 | 102.39 |
| browse | 20.41 | 6.06 |
| checkout | 4.09 | 103.25 |
| order_history | 3.01 | 14.65 |
| product_detail | 17.21 | 10.22 |
| recommend | 7.16 | 10.15 |
| relogin | 1.54 | 41.64 |
| restock | 0.3 | 84.35 |
| search_text | 8.19 | 8.64 |
| session_check | 12.17 | 23.34 |
| signup | 0.5 | 29.40 |
| update_cart_item | 3.03 | 26.00 |
| update_profile | 1.02 | 19.58 |
| view_cart | 8.18 | 11.61 |
| write_review | 2.04 | 25.90 |

## Errors and business outcomes at the highest level

```json
{
 "statuses": {
  "ok": 349580,
  "empty_cart": 4275,
  "not_found": 23,
  "conflict_exhausted": 75
 },
 "errors": {
  "conflict_exhausted": {
   "count": 87,
   "first": "[elitesql:9] conflict, retry transaction: products/b7 changed after this transaction began",
   "ops": {
    "checkout": 84,
    "restock": 3
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
