# Mini-SaaS concurrency simulation — sidecar

Run: `A-1` on 2026-09-23T13:11:27 · commit `92e0290` · macOS-26.6.2-arm64-arm-64bit · 10 CPUs

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
- **seed time**: 1.15 s, 13.1 MiB after seeding

## Capacity and performance curve

Latencies are of the database call (service operation) in milliseconds; `user p99` adds the wait for a pooled connection.

| users | conns | ops/s | reads/s | writes/s | p50 | p95 | p99 | p99.9 | max | user p99 | success % | conflict retry % | srv CPU | srv RSS MiB | gen CPU | thr CV | invariants |
|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|
| 100 | 100 | 18181.7 | 14066.0 | 4115.7 | 2.034 | 18.574 | 35.767 | 81.445 | 915.542 | 35.768 | 99.997 | 0.537 | 5.22 | 160.7 | 2.13 | 0.072 | ok |

## Insights

- **Peak throughput**: 18181.7 ops/s at 100 users (p99 35.767 ms).
- **Capacity at p99 ≤ 10 ms**: 0 users (DB latency); 0 users counting pool wait.
- **Capacity at p99 ≤ 50 ms**: 100 users (DB latency); 100 users counting pool wait.
- **Capacity at p99 ≤ 100 ms**: 100 users (DB latency); 100 users counting pool wait.
- **Capacity at p99 ≤ 250 ms**: 100 users (DB latency); 100 users counting pool wait.
- **Capacity at p99 ≤ 1000 ms**: 100 users (DB latency); 100 users counting pool wait.
- **Ops per server core-second**: 100→3483.
- **Read-your-writes violations**: none.
- **Business invariants**: all stages ok.
- **Offline integrity check** (elitesql check): ok in 7.25 s.
  - right after killing the server (no clean close): exit 3, 0 errors, 231198 warnings; after one clean open/close: exit 0, 0 errors, 0 warnings.
- **Database size**: 69.8 MiB after the first stage, 69.8 MiB after the last; 11376 paid orders in total.

## p99 latency per operation (ms)

| op | share % | 100 |
|---|---:|---:|
| add_to_cart | 10.11 | 41.65 |
| admin_dashboard | 1.04 | 112.87 |
| browse | 20.47 | 16.58 |
| checkout | 4.11 | 71.38 |
| order_history | 3.05 | 28.09 |
| product_detail | 17.18 | 25.11 |
| recommend | 7.11 | 16.12 |
| relogin | 1.53 | 61.66 |
| restock | 0.31 | 76.72 |
| search_text | 8.13 | 19.19 |
| session_check | 12.18 | 41.89 |
| signup | 0.51 | 40.47 |
| update_cart_item | 3.04 | 35.63 |
| update_profile | 1.02 | 32.74 |
| view_cart | 8.2 | 21.78 |
| write_review | 2.03 | 36.96 |

## Errors and business outcomes at the highest level

```json
{
 "statuses": {
  "ok": 269311,
  "empty_cart": 3389,
  "conflict_exhausted": 8,
  "not_found": 17
 },
 "errors": {
  "conflict_exhausted": {
   "count": 9,
   "first": "[elitesql:9] conflict, retry transaction: products/b8 changed after this transaction began",
   "ops": {
    "checkout": 7,
    "restock": 2
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
