# Mini-SaaS concurrency simulation — sidecar

Run: `A-1` on 2026-09-23T13:02:29 · commit `92e0290` · macOS-26.6.2-arm64-arm-64bit · 10 CPUs

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
| 100 | 100 | 17393.3 | 13459.7 | 3933.5 | 1.976 | 19.346 | 38.283 | 94.577 | 992.533 | 38.284 | 99.997 | 0.507 | 4.88 | 157.1 | 2.01 | 0.097 | ok |

## Insights

- **Peak throughput**: 17393.3 ops/s at 100 users (p99 38.283 ms).
- **Capacity at p99 ≤ 10 ms**: 0 users (DB latency); 0 users counting pool wait.
- **Capacity at p99 ≤ 50 ms**: 100 users (DB latency); 100 users counting pool wait.
- **Capacity at p99 ≤ 100 ms**: 100 users (DB latency); 100 users counting pool wait.
- **Capacity at p99 ≤ 250 ms**: 100 users (DB latency); 100 users counting pool wait.
- **Capacity at p99 ≤ 1000 ms**: 100 users (DB latency); 100 users counting pool wait.
- **Ops per server core-second**: 100→3564.
- **Read-your-writes violations**: none.
- **Business invariants**: all stages ok.
- **Offline integrity check** (elitesql check): ok in 7.2 s.
  - right after killing the server (no clean close): exit 3, 0 errors, 224556 warnings; after one clean open/close: exit 0, 0 errors, 0 warnings.
- **Database size**: 67.7 MiB after the first stage, 67.7 MiB after the last; 10935 paid orders in total.

## p99 latency per operation (ms)

| op | share % | 100 |
|---|---:|---:|
| add_to_cart | 10.1 | 44.94 |
| admin_dashboard | 1.04 | 124.25 |
| browse | 20.51 | 19.86 |
| checkout | 4.1 | 73.35 |
| order_history | 3.05 | 31.19 |
| product_detail | 17.19 | 28.49 |
| recommend | 7.11 | 18.37 |
| relogin | 1.52 | 70.72 |
| restock | 0.31 | 72.00 |
| search_text | 8.13 | 21.18 |
| session_check | 12.14 | 47.25 |
| signup | 0.5 | 42.29 |
| update_cart_item | 3.05 | 38.47 |
| update_profile | 1.01 | 40.67 |
| view_cart | 8.21 | 24.86 |
| write_review | 2.02 | 38.00 |

## Errors and business outcomes at the highest level

```json
{
 "statuses": {
  "ok": 257648,
  "empty_cart": 3226,
  "not_found": 17,
  "conflict_exhausted": 8
 },
 "errors": {
  "conflict_exhausted": {
   "count": 14,
   "first": "[elitesql:9] conflict, retry transaction: products/b7 changed after this transaction began",
   "ops": {
    "checkout": 14
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
