# Mini-SaaS concurrency simulation — sidecar

Run: `A-1` on 2026-09-23T14:01:17 · commit `92e0290` · macOS-26.6.2-arm64-arm-64bit · 10 CPUs

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
| 100 | 100 | 23877.6 | 18472.7 | 5404.9 | 0.841 | 14.784 | 26.366 | 65.03 | 1004.15 | 26.366 | 99.99 | 0.702 | 6.05 | 187.2 | 2.58 | 0.078 | ok |

## Insights

- **Peak throughput**: 23877.6 ops/s at 100 users (p99 26.366 ms).
- **Capacity at p99 ≤ 10 ms**: 0 users (DB latency); 0 users counting pool wait.
- **Capacity at p99 ≤ 50 ms**: 100 users (DB latency); 100 users counting pool wait.
- **Capacity at p99 ≤ 100 ms**: 100 users (DB latency); 100 users counting pool wait.
- **Capacity at p99 ≤ 250 ms**: 100 users (DB latency); 100 users counting pool wait.
- **Capacity at p99 ≤ 1000 ms**: 100 users (DB latency); 100 users counting pool wait.
- **Ops per server core-second**: 100→3947.
- **Read-your-writes violations**: none.
- **Business invariants**: all stages ok.
- **Offline integrity check** (elitesql check): ok in 8.38 s.
  - right after killing the server (no clean close): exit 3, 0 errors, 287158 warnings; after one clean open/close: exit 0, 0 errors, 0 warnings.
- **Database size**: 89.1 MiB after the first stage, 89.1 MiB after the last; 15101 paid orders in total.

## p99 latency per operation (ms)

| op | share % | 100 |
|---|---:|---:|
| add_to_cart | 10.12 | 33.60 |
| admin_dashboard | 1.03 | 102.50 |
| browse | 20.41 | 5.81 |
| checkout | 4.08 | 76.30 |
| order_history | 3.01 | 13.88 |
| product_detail | 17.22 | 10.16 |
| recommend | 7.14 | 9.24 |
| relogin | 1.54 | 42.51 |
| restock | 0.3 | 90.91 |
| search_text | 8.19 | 8.06 |
| session_check | 12.17 | 22.71 |
| signup | 0.51 | 27.65 |
| update_cart_item | 3.03 | 24.80 |
| update_profile | 1.03 | 20.88 |
| view_cart | 8.18 | 11.15 |
| write_review | 2.04 | 25.86 |

## Errors and business outcomes at the highest level

```json
{
 "statuses": {
  "ok": 353796,
  "empty_cart": 4310,
  "not_found": 22,
  "conflict_exhausted": 36
 },
 "errors": {
  "conflict_exhausted": {
   "count": 54,
   "first": "[elitesql:9] conflict, retry transaction: products/b5 changed after this transaction began",
   "ops": {
    "checkout": 52,
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
