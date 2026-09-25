# Mini-SaaS concurrency simulation — sidecar

Run: `B-1` on 2026-09-23T13:23:02 · commit `92e0290` · macOS-26.6.2-arm64-arm-64bit · 10 CPUs

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
| 100 | 100 | 14316.5 | 11084.9 | 3231.6 | 2.191 | 23.532 | 45.099 | 124.482 | 1060.948 | 45.101 | 99.968 | 0.767 | 5.35 | 141.5 | 1.96 | 0.081 | ok |

## Insights

- **Peak throughput**: 14316.5 ops/s at 100 users (p99 45.099 ms).
- **Capacity at p99 ≤ 10 ms**: 0 users (DB latency); 0 users counting pool wait.
- **Capacity at p99 ≤ 50 ms**: 100 users (DB latency); 100 users counting pool wait.
- **Capacity at p99 ≤ 100 ms**: 100 users (DB latency); 100 users counting pool wait.
- **Capacity at p99 ≤ 250 ms**: 100 users (DB latency); 100 users counting pool wait.
- **Capacity at p99 ≤ 1000 ms**: 100 users (DB latency); 100 users counting pool wait.
- **Ops per server core-second**: 100→2676.
- **Read-your-writes violations**: none.
- **Business invariants**: all stages ok.
- **Offline integrity check** (elitesql check): ok in 6.18 s.
  - right after killing the server (no clean close): exit 3, 0 errors, 199226 warnings; after one clean open/close: exit 0, 0 errors, 0 warnings.
- **Database size**: 59.1 MiB after the first stage, 59.1 MiB after the last; 9166 paid orders in total.

## p99 latency per operation (ms)

| op | share % | 100 |
|---|---:|---:|
| add_to_cart | 10.1 | 50.67 |
| admin_dashboard | 1.03 | 143.00 |
| browse | 20.57 | 18.00 |
| checkout | 4.06 | 217.87 |
| order_history | 3.06 | 35.25 |
| product_detail | 17.23 | 17.68 |
| recommend | 7.13 | 13.57 |
| relogin | 1.53 | 73.29 |
| restock | 0.31 | 92.98 |
| search_text | 8.13 | 18.92 |
| session_check | 12.13 | 43.48 |
| signup | 0.5 | 56.28 |
| update_cart_item | 3.04 | 41.97 |
| update_profile | 1.01 | 27.34 |
| view_cart | 8.16 | 29.07 |
| write_review | 2.03 | 44.06 |

## Errors and business outcomes at the highest level

```json
{
 "statuses": {
  "ok": 212033,
  "empty_cart": 2628,
  "not_found": 17,
  "conflict_exhausted": 69
 },
 "errors": {
  "conflict_exhausted": {
   "count": 84,
   "first": "[elitesql:9] conflict, retry transaction: products/c15 changed after this transaction began",
   "ops": {
    "checkout": 83,
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
