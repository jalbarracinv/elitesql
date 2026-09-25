# Mini-SaaS concurrency simulation — sidecar

Run: `probe4-100` on 2026-09-23T13:42:26 · commit `92e0290` · macOS-26.6.2-arm64-arm-64bit · 10 CPUs

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
- **seed time**: 1.17 s, 13.1 MiB after seeding

## Capacity and performance curve

Latencies are of the database call (service operation) in milliseconds; `user p99` adds the wait for a pooled connection.

| users | conns | ops/s | reads/s | writes/s | p50 | p95 | p99 | p99.9 | max | user p99 | success % | conflict retry % | srv CPU | srv RSS MiB | gen CPU | thr CV | invariants |
|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|
| 100 | 100 | 19269.5 | 14913.9 | 4355.7 | 1.33 | 18.812 | 36.04 | 88.32 | 1054.355 | 36.042 | 99.987 | 0.729 | 5.43 | 165.0 | 2.49 | 0.074 | ok |

## Insights

- **Peak throughput**: 19269.5 ops/s at 100 users (p99 36.04 ms).
- **Capacity at p99 ≤ 10 ms**: 0 users (DB latency); 0 users counting pool wait.
- **Capacity at p99 ≤ 50 ms**: 100 users (DB latency); 100 users counting pool wait.
- **Capacity at p99 ≤ 100 ms**: 100 users (DB latency); 100 users counting pool wait.
- **Capacity at p99 ≤ 250 ms**: 100 users (DB latency); 100 users counting pool wait.
- **Capacity at p99 ≤ 1000 ms**: 100 users (DB latency); 100 users counting pool wait.
- **Ops per server core-second**: 100→3549.
- **Read-your-writes violations**: none.
- **Business invariants**: all stages ok.
- **Offline integrity check** (elitesql check): ok in 7.51 s.
  - right after killing the server (no clean close): exit 3, 0 errors, 238458 warnings; after one clean open/close: exit 0, 0 errors, 0 warnings.
- **Database size**: 73.0 MiB after the first stage, 73.0 MiB after the last; 11841 paid orders in total.

## p99 latency per operation (ms)

| op | share % | 100 |
|---|---:|---:|
| add_to_cart | 10.1 | 39.63 |
| admin_dashboard | 1.04 | 133.82 |
| browse | 20.46 | 13.63 |
| checkout | 4.1 | 105.27 |
| order_history | 3.03 | 29.75 |
| product_detail | 17.23 | 14.40 |
| recommend | 7.12 | 10.96 |
| relogin | 1.54 | 50.21 |
| restock | 0.31 | 74.50 |
| search_text | 8.17 | 14.94 |
| session_check | 12.17 | 36.88 |
| signup | 0.5 | 46.20 |
| update_cart_item | 3.03 | 33.40 |
| update_profile | 1.01 | 22.42 |
| view_cart | 8.19 | 24.13 |
| write_review | 2.02 | 32.67 |

## Errors and business outcomes at the highest level

```json
{
 "statuses": {
  "ok": 285426,
  "empty_cart": 3560,
  "conflict_exhausted": 37,
  "not_found": 20
 },
 "errors": {
  "conflict_exhausted": {
   "count": 54,
   "first": "[elitesql:9] conflict, retry transaction: products/c12 changed after this transaction began",
   "ops": {
    "checkout": 53,
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
