# Mini-SaaS concurrency simulation — sidecar

Run: `A-3` on 2026-09-23T13:05:04 · commit `92e0290` · macOS-26.6.2-arm64-arm-64bit · 10 CPUs

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
- **seed time**: 1.21 s, 13.1 MiB after seeding

## Capacity and performance curve

Latencies are of the database call (service operation) in milliseconds; `user p99` adds the wait for a pooled connection.

| users | conns | ops/s | reads/s | writes/s | p50 | p95 | p99 | p99.9 | max | user p99 | success % | conflict retry % | srv CPU | srv RSS MiB | gen CPU | thr CV | invariants |
|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|
| 100 | 100 | 17551.5 | 13578.3 | 3973.2 | 2.021 | 19.791 | 37.657 | 83.945 | 886.916 | 37.658 | 99.991 | 0.512 | 5.18 | 154.4 | 1.99 | 0.079 | ok |

## Insights

- **Peak throughput**: 17551.5 ops/s at 100 users (p99 37.657 ms).
- **Capacity at p99 ≤ 10 ms**: 0 users (DB latency); 0 users counting pool wait.
- **Capacity at p99 ≤ 50 ms**: 100 users (DB latency); 100 users counting pool wait.
- **Capacity at p99 ≤ 100 ms**: 100 users (DB latency); 100 users counting pool wait.
- **Capacity at p99 ≤ 250 ms**: 100 users (DB latency); 100 users counting pool wait.
- **Capacity at p99 ≤ 1000 ms**: 100 users (DB latency); 100 users counting pool wait.
- **Ops per server core-second**: 100→3388.
- **Read-your-writes violations**: none.
- **Business invariants**: all stages ok.
- **Offline integrity check** (elitesql check): ok in 6.92 s.
  - right after killing the server (no clean close): exit 3, 0 errors, 220622 warnings; after one clean open/close: exit 0, 0 errors, 0 warnings.
- **Database size**: 66.6 MiB after the first stage, 66.6 MiB after the last; 10663 paid orders in total.

## p99 latency per operation (ms)

| op | share % | 100 |
|---|---:|---:|
| add_to_cart | 10.11 | 42.20 |
| admin_dashboard | 1.03 | 117.22 |
| browse | 20.47 | 19.44 |
| checkout | 4.1 | 80.69 |
| order_history | 3.05 | 32.69 |
| product_detail | 17.21 | 27.69 |
| recommend | 7.13 | 18.99 |
| relogin | 1.52 | 61.72 |
| restock | 0.31 | 76.88 |
| search_text | 8.14 | 22.29 |
| session_check | 12.13 | 42.72 |
| signup | 0.5 | 36.91 |
| update_cart_item | 3.04 | 36.89 |
| update_profile | 1.02 | 36.71 |
| view_cart | 8.2 | 25.84 |
| write_review | 2.04 | 35.91 |

## Errors and business outcomes at the highest level

```json
{
 "statuses": {
  "ok": 259971,
  "empty_cart": 3258,
  "not_found": 18,
  "conflict_exhausted": 25
 },
 "errors": {
  "conflict_exhausted": {
   "count": 28,
   "first": "[elitesql:9] conflict, retry transaction: products/c85 changed after this transaction began",
   "ops": {
    "checkout": 28
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
