# Mini-SaaS concurrency simulation — sidecar

Run: `run-2-before` on 2026-09-23T23:29:34 · commit `92e0290` · macOS-26.6.2-arm64-arm-64bit · 10 CPUs

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
- **seed time**: 1.13 s, 12.9 MiB after seeding

## Capacity and performance curve

Latencies are of the database call (service operation) in milliseconds; `user p99` adds the wait for a pooled connection.

| users | conns | ops/s | reads/s | writes/s | p50 | p95 | p99 | p99.9 | max | user p99 | success % | conflict retry % | srv CPU | srv RSS MiB | gen CPU | thr CV | invariants |
|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|
| 10 | 10 | 15822.5 | 12243.2 | 3579.2 | 0.377 | 1.527 | 4.878 | 9.903 | 531.926 | 4.878 | 100.0 | 0.09 | 3.92 | 174.4 | 1.37 | 0.2 | ok |
| 100 | 100 | 19637.7 | 15190.2 | 4447.4 | 1.292 | 17.134 | 31.567 | 81.438 | 1373.974 | 31.569 | 99.994 | 0.671 | 6.57 | 414.5 | 2.03 | 0.084 | ok |
| 500 | 500 | 13192.1 | 10187.6 | 3004.5 | 2.77 | 136.012 | 214.378 | 847.485 | 3356.367 | 214.379 | 99.91 | 1.049 | 6.81 | 643.8 | 1.81 | 0.095 | ok |

## Insights

- **Peak throughput**: 19637.7 ops/s at 100 users (p99 31.567 ms).
- **Capacity at p99 ≤ 10 ms**: 10 users (DB latency); 10 users counting pool wait.
- **Capacity at p99 ≤ 50 ms**: 100 users (DB latency); 100 users counting pool wait.
- **Capacity at p99 ≤ 100 ms**: 100 users (DB latency); 100 users counting pool wait.
- **Capacity at p99 ≤ 250 ms**: 500 users (DB latency); 500 users counting pool wait.
- **Capacity at p99 ≤ 1000 ms**: 500 users (DB latency); 500 users counting pool wait.
- **USL fit**: σ (contention) = 0.21, κ (coherency) = 3.16e-04, predicted peak at N ≈ 50.0 (rmse log 0.0282).
- **Ops per server core-second**: 10→4036, 100→2989, 500→1937.
- **Read-your-writes violations**: none.
- **Business invariants**: all stages ok.
- **Offline integrity check** (elitesql check): ok in 16.45 s.
  - right after killing the server (no clean close): exit 3, 0 errors, 684680 warnings; after one clean open/close: exit 0, 0 errors, 0 warnings.
- **Database size**: 83.4 MiB after the first stage, 183.6 MiB after the last; 41394 paid orders in total.

## p99 latency per operation (ms)

| op | share % | 10 | 100 | 500 |
|---|---:|---:|---:|---:|
| add_to_cart | 10.22 | 3.93 | 36.93 | 199.71 |
| admin_dashboard | 1.02 | 12.83 | 125.33 | 1987.38 |
| browse | 20.29 | 1.17 | 16.95 | 22.86 |
| checkout | 3.94 | 5.08 | 80.27 | 381.82 |
| order_history | 3.12 | 3.31 | 23.42 | 28.60 |
| product_detail | 17.36 | 0.65 | 13.60 | 20.07 |
| recommend | 7.07 | 0.79 | 10.61 | 19.44 |
| relogin | 1.56 | 4.39 | 53.82 | 326.01 |
| restock | 0.31 | 3.95 | 90.67 | 269.19 |
| search_text | 8.16 | 0.76 | 11.72 | 19.85 |
| session_check | 12.21 | 3.63 | 29.35 | 194.43 |
| signup | 0.49 | 3.86 | 33.11 | 206.29 |
| update_cart_item | 3.05 | 3.67 | 29.45 | 185.92 |
| update_profile | 1.04 | 3.46 | 23.22 | 182.77 |
| view_cart | 8.14 | 0.57 | 15.34 | 25.65 |
| write_review | 2.0 | 3.57 | 30.30 | 76.97 |

## Errors and business outcomes at the highest level

```json
{
 "statuses": {
  "ok": 260386,
  "empty_cart": 3098,
  "conflict_exhausted": 237,
  "out_of_stock": 108,
  "not_found": 13
 },
 "errors": {
  "conflict_exhausted": {
   "count": 284,
   "first": "[elitesql:9] conflict, retry transaction: products/b8 changed after this transaction began",
   "ops": {
    "checkout": 263,
    "restock": 20,
    "write_review": 1
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
