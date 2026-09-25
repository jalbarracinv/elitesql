# Mini-SaaS concurrency simulation — sidecar

Run: `B-2` on 2026-09-23T15:04:15 · commit `92e0290` · macOS-26.6.2-arm64-arm-64bit · 10 CPUs

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
- **seed time**: 1.12 s, 13.1 MiB after seeding

## Capacity and performance curve

Latencies are of the database call (service operation) in milliseconds; `user p99` adds the wait for a pooled connection.

| users | conns | ops/s | reads/s | writes/s | p50 | p95 | p99 | p99.9 | max | user p99 | success % | conflict retry % | srv CPU | srv RSS MiB | gen CPU | thr CV | invariants |
|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|
| 100 | 100 | 23280.6 | 18008.1 | 5272.5 | 0.829 | 15.215 | 26.362 | 72.642 | 1059.403 | 26.363 | 99.973 | 0.68 | 5.92 | 239.0 | 2.7 | 0.073 | ok |

## Insights

- **Peak throughput**: 23280.6 ops/s at 100 users (p99 26.362 ms).
- **Capacity at p99 ≤ 10 ms**: 0 users (DB latency); 0 users counting pool wait.
- **Capacity at p99 ≤ 50 ms**: 100 users (DB latency); 100 users counting pool wait.
- **Capacity at p99 ≤ 100 ms**: 100 users (DB latency); 100 users counting pool wait.
- **Capacity at p99 ≤ 250 ms**: 100 users (DB latency); 100 users counting pool wait.
- **Capacity at p99 ≤ 1000 ms**: 100 users (DB latency); 100 users counting pool wait.
- **Ops per server core-second**: 100→3933.
- **Read-your-writes violations**: none.
- **Business invariants**: all stages ok.
- **Offline integrity check** (elitesql check): ok in 8.37 s.
  - right after killing the server (no clean close): exit 3, 0 errors, 282886 warnings; after one clean open/close: exit 0, 0 errors, 0 warnings.
- **Database size**: 87.7 MiB after the first stage, 87.7 MiB after the last; 14787 paid orders in total.

## p99 latency per operation (ms)

| op | share % | 100 |
|---|---:|---:|
| add_to_cart | 10.13 | 30.43 |
| admin_dashboard | 1.03 | 109.99 |
| browse | 20.37 | 6.57 |
| checkout | 4.09 | 109.44 |
| order_history | 3.01 | 16.27 |
| product_detail | 17.22 | 10.95 |
| recommend | 7.15 | 9.48 |
| relogin | 1.53 | 40.41 |
| restock | 0.3 | 74.44 |
| search_text | 8.17 | 9.92 |
| session_check | 12.2 | 24.19 |
| signup | 0.5 | 25.14 |
| update_cart_item | 3.03 | 24.78 |
| update_profile | 1.03 | 19.59 |
| view_cart | 8.2 | 12.62 |
| write_review | 2.03 | 25.49 |

## Errors and business outcomes at the highest level

```json
{
 "statuses": {
  "ok": 344868,
  "conflict_exhausted": 96,
  "empty_cart": 4223,
  "not_found": 22
 },
 "errors": {
  "conflict_exhausted": {
   "count": 125,
   "first": "[elitesql:9] conflict, retry transaction: products/c14 changed after this transaction began",
   "ops": {
    "checkout": 123,
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
