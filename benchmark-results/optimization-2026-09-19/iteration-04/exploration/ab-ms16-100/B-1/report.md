# Mini-SaaS concurrency simulation — sidecar

Run: `B-1` on 2026-09-23T13:30:28 · commit `92e0290` · macOS-26.6.2-arm64-arm-64bit · 10 CPUs

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
- **seed time**: 1.16 s, 13.1 MiB after seeding

## Capacity and performance curve

Latencies are of the database call (service operation) in milliseconds; `user p99` adds the wait for a pooled connection.

| users | conns | ops/s | reads/s | writes/s | p50 | p95 | p99 | p99.9 | max | user p99 | success % | conflict retry % | srv CPU | srv RSS MiB | gen CPU | thr CV | invariants |
|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|
| 100 | 100 | 13602.6 | 10528.9 | 3073.7 | 3.393 | 23.268 | 46.821 | 167.297 | 1568.079 | 46.823 | 99.96 | 0.794 | 4.64 | 136.3 | 1.89 | 0.099 | ok |

## Insights

- **Peak throughput**: 13602.6 ops/s at 100 users (p99 46.821 ms).
- **Capacity at p99 ≤ 10 ms**: 0 users (DB latency); 0 users counting pool wait.
- **Capacity at p99 ≤ 50 ms**: 100 users (DB latency); 100 users counting pool wait.
- **Capacity at p99 ≤ 100 ms**: 100 users (DB latency); 100 users counting pool wait.
- **Capacity at p99 ≤ 250 ms**: 100 users (DB latency); 100 users counting pool wait.
- **Capacity at p99 ≤ 1000 ms**: 100 users (DB latency); 100 users counting pool wait.
- **Ops per server core-second**: 100→2932.
- **Read-your-writes violations**: none.
- **Business invariants**: all stages ok.
- **Offline integrity check** (elitesql check): ok in 6.19 s.
  - right after killing the server (no clean close): exit 3, 0 errors, 194200 warnings; after one clean open/close: exit 0, 0 errors, 0 warnings.
- **Database size**: 57.6 MiB after the first stage, 57.6 MiB after the last; 8803 paid orders in total.

## p99 latency per operation (ms)

| op | share % | 100 |
|---|---:|---:|
| add_to_cart | 10.12 | 55.13 |
| admin_dashboard | 1.02 | 134.41 |
| browse | 20.54 | 16.82 |
| checkout | 4.05 | 327.05 |
| order_history | 3.07 | 34.69 |
| product_detail | 17.2 | 24.40 |
| recommend | 7.12 | 23.04 |
| relogin | 1.54 | 71.36 |
| restock | 0.31 | 92.37 |
| search_text | 8.15 | 17.33 |
| session_check | 12.14 | 35.92 |
| signup | 0.49 | 42.30 |
| update_cart_item | 3.05 | 44.91 |
| update_profile | 1.01 | 24.05 |
| view_cart | 8.17 | 27.38 |
| write_review | 2.02 | 39.31 |

## Errors and business outcomes at the highest level

```json
{
 "statuses": {
  "ok": 201450,
  "conflict_exhausted": 81,
  "empty_cart": 2492,
  "not_found": 16
 },
 "errors": {
  "conflict_exhausted": {
   "count": 104,
   "first": "[elitesql:9] conflict, retry transaction: products/c11 changed after this transaction began",
   "ops": {
    "checkout": 104
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
