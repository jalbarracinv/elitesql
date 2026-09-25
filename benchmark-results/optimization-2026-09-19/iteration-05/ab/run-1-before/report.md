# Mini-SaaS concurrency simulation — sidecar

Run: `run-1-before` on 2026-09-23T23:25:07 · commit `92e0290` · macOS-26.6.2-arm64-arm-64bit · 10 CPUs

## Configuration

- **transport**: sidecar
- **levels**: [10, 100, 500]
- **duration**: 20.0
- **warmup**: 5.0
- **ramp**: 5.0
- **think**: [0.0, 0.0]
- **processes**: 0
- **connections**: 0
- **durability**: balanced
- **products**: 5000
- **accounts**: 20000
- **scenario**: baseline
- **seed**: 1
- **fresh_per_stage**: False
- **seed time**: 1.12 s, 12.9 MiB after seeding

## Capacity and performance curve

Latencies are of the database call (service operation) in milliseconds; `user p99` adds the wait for a pooled connection.

| users | conns | ops/s | reads/s | writes/s | p50 | p95 | p99 | p99.9 | max | user p99 | success % | conflict retry % | srv CPU | srv RSS MiB | gen CPU | thr CV | invariants |
|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|
| 10 | 10 | 16211.6 | 12548.0 | 3663.6 | 0.375 | 1.646 | 5.085 | 10.108 | 562.544 | 5.086 | 100.0 | 0.09 | 4.13 | 182.5 | 1.41 | 0.125 | ok |
| 100 | 100 | 19208.6 | 14858.5 | 4350.1 | 1.338 | 17.395 | 31.461 | 83.761 | 1098.13 | 31.462 | 99.984 | 0.68 | 6.52 | 428.2 | 2.08 | 0.077 | ok |
| 500 | 500 | 13371.0 | 10328.0 | 3043.0 | 2.683 | 131.789 | 210.28 | 1102.862 | 3411.52 | 210.281 | 99.925 | 1.018 | 6.85 | 659.0 | 1.79 | 0.095 | ok |

## Insights

- **Peak throughput**: 19208.6 ops/s at 100 users (p99 31.461 ms).
- **Capacity at p99 ≤ 10 ms**: 10 users (DB latency); 10 users counting pool wait.
- **Capacity at p99 ≤ 50 ms**: 100 users (DB latency); 100 users counting pool wait.
- **Capacity at p99 ≤ 100 ms**: 100 users (DB latency); 100 users counting pool wait.
- **Capacity at p99 ≤ 250 ms**: 500 users (DB latency); 500 users counting pool wait.
- **Capacity at p99 ≤ 1000 ms**: 500 users (DB latency); 500 users counting pool wait.
- **USL fit**: σ (contention) = 0.21, κ (coherency) = 3.16e-04, predicted peak at N ≈ 50.0 (rmse log 0.0204).
- **Ops per server core-second**: 10→3925, 100→2946, 500→1952.
- **Read-your-writes violations**: none.
- **Business invariants**: all stages ok.
- **Offline integrity check** (elitesql check): ok in 16.65 s.
  - right after killing the server (no clean close): exit 3, 0 errors, 690508 warnings; after one clean open/close: exit 0, 0 errors, 0 warnings.
- **Database size**: 86.3 MiB after the first stage, 185.2 MiB after the last; 41791 paid orders in total.

## p99 latency per operation (ms)

| op | share % | 10 | 100 | 500 |
|---|---:|---:|---:|---:|
| add_to_cart | 10.2 | 3.93 | 35.91 | 191.97 |
| admin_dashboard | 1.02 | 13.21 | 130.18 | 2840.22 |
| browse | 20.3 | 1.19 | 15.98 | 26.36 |
| checkout | 3.94 | 5.08 | 97.84 | 330.23 |
| order_history | 3.13 | 3.42 | 20.12 | 34.35 |
| product_detail | 17.38 | 0.66 | 13.45 | 22.20 |
| recommend | 7.06 | 0.78 | 10.76 | 20.66 |
| relogin | 1.56 | 4.28 | 49.17 | 323.05 |
| restock | 0.32 | 3.93 | 93.52 | 270.83 |
| search_text | 8.16 | 0.78 | 11.41 | 18.62 |
| session_check | 12.23 | 3.66 | 29.30 | 190.91 |
| signup | 0.49 | 3.80 | 34.64 | 210.85 |
| update_cart_item | 3.04 | 3.64 | 28.06 | 187.42 |
| update_profile | 1.04 | 3.56 | 22.74 | 187.63 |
| view_cart | 8.14 | 0.57 | 15.17 | 26.78 |
| write_review | 2.01 | 3.64 | 29.50 | 82.25 |

## Errors and business outcomes at the highest level

```json
{
 "statuses": {
  "ok": 263926,
  "conflict_exhausted": 200,
  "empty_cart": 3154,
  "out_of_stock": 122,
  "not_found": 17
 },
 "errors": {
  "conflict_exhausted": {
   "count": 264,
   "first": "[elitesql:9] conflict, retry transaction: products/c11 changed after this transaction began",
   "ops": {
    "restock": 26,
    "checkout": 238
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
