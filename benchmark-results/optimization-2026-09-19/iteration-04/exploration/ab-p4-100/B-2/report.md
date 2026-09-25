# Mini-SaaS concurrency simulation — sidecar

Run: `B-2` on 2026-09-23T13:40:48 · commit `92e0290` · macOS-26.6.2-arm64-arm-64bit · 10 CPUs

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
- **seed time**: 1.18 s, 13.1 MiB after seeding

## Capacity and performance curve

Latencies are of the database call (service operation) in milliseconds; `user p99` adds the wait for a pooled connection.

| users | conns | ops/s | reads/s | writes/s | p50 | p95 | p99 | p99.9 | max | user p99 | success % | conflict retry % | srv CPU | srv RSS MiB | gen CPU | thr CV | invariants |
|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|
| 100 | 100 | 19342.5 | 14963.9 | 4378.6 | 1.244 | 18.979 | 37.012 | 93.116 | 1280.393 | 37.014 | 99.972 | 0.696 | 5.41 | 165.3 | 2.61 | 0.073 | ok |

## Insights

- **Peak throughput**: 19342.5 ops/s at 100 users (p99 37.012 ms).
- **Capacity at p99 ≤ 10 ms**: 0 users (DB latency); 0 users counting pool wait.
- **Capacity at p99 ≤ 50 ms**: 100 users (DB latency); 100 users counting pool wait.
- **Capacity at p99 ≤ 100 ms**: 100 users (DB latency); 100 users counting pool wait.
- **Capacity at p99 ≤ 250 ms**: 100 users (DB latency); 100 users counting pool wait.
- **Capacity at p99 ≤ 1000 ms**: 100 users (DB latency); 100 users counting pool wait.
- **Ops per server core-second**: 100→3575.
- **Read-your-writes violations**: none.
- **Business invariants**: all stages ok.
- **Offline integrity check** (elitesql check): ok in 7.53 s.
  - right after killing the server (no clean close): exit 3, 0 errors, 242506 warnings; after one clean open/close: exit 0, 0 errors, 0 warnings.
- **Database size**: 74.6 MiB after the first stage, 74.6 MiB after the last; 12066 paid orders in total.

## p99 latency per operation (ms)

| op | share % | 100 |
|---|---:|---:|
| add_to_cart | 10.09 | 39.80 |
| admin_dashboard | 1.04 | 139.64 |
| browse | 20.44 | 14.11 |
| checkout | 4.1 | 123.36 |
| order_history | 3.04 | 30.62 |
| product_detail | 17.23 | 15.41 |
| recommend | 7.09 | 11.21 |
| relogin | 1.54 | 59.72 |
| restock | 0.31 | 81.65 |
| search_text | 8.16 | 17.72 |
| session_check | 12.2 | 36.78 |
| signup | 0.51 | 43.66 |
| update_cart_item | 3.03 | 34.18 |
| update_profile | 1.02 | 23.06 |
| view_cart | 8.18 | 25.74 |
| write_review | 2.04 | 36.16 |

## Errors and business outcomes at the highest level

```json
{
 "statuses": {
  "ok": 286469,
  "empty_cart": 3568,
  "not_found": 20,
  "conflict_exhausted": 81
 },
 "errors": {
  "conflict_exhausted": {
   "count": 94,
   "first": "[elitesql:9] conflict, retry transaction: products/b3 changed after this transaction began",
   "ops": {
    "checkout": 94
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
