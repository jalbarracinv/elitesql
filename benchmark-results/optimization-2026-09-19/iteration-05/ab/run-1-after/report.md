# Mini-SaaS concurrency simulation — sidecar

Run: `run-1-after` on 2026-09-23T23:27:21 · commit `92e0290` · macOS-26.6.2-arm64-arm-64bit · 10 CPUs

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
| 10 | 10 | 16257.2 | 12578.6 | 3678.6 | 0.376 | 1.634 | 5.003 | 10.117 | 550.35 | 5.004 | 100.0 | 0.0 | 4.11 | 185.9 | 1.43 | 0.141 | ok |
| 100 | 100 | 19787.3 | 15302.9 | 4484.4 | 1.348 | 17.203 | 30.104 | 67.561 | 1307.611 | 30.105 | 100.0 | 0.0 | 6.62 | 279.8 | 1.91 | 0.078 | ok |
| 500 | 500 | 11966.3 | 9244.5 | 2721.8 | 2.418 | 126.309 | 228.286 | 3114.116 | 8483.572 | 228.287 | 99.999 | 0.0 | 7.88 | 418.4 | 1.3 | 0.163 | ok |

## Insights

- **Peak throughput**: 19787.3 ops/s at 100 users (p99 30.104 ms).
- **Capacity at p99 ≤ 10 ms**: 10 users (DB latency); 10 users counting pool wait.
- **Capacity at p99 ≤ 50 ms**: 100 users (DB latency); 100 users counting pool wait.
- **Capacity at p99 ≤ 100 ms**: 100 users (DB latency); 100 users counting pool wait.
- **Capacity at p99 ≤ 250 ms**: 500 users (DB latency); 500 users counting pool wait.
- **Capacity at p99 ≤ 1000 ms**: 500 users (DB latency); 500 users counting pool wait.
- **USL fit**: σ (contention) = 0.21, κ (coherency) = 3.98e-04, predicted peak at N ≈ 44.5 (rmse log 0.029).
- **Ops per server core-second**: 10→3956, 100→2989, 500→1519.
- **Read-your-writes violations**: none.
- **Business invariants**: all stages ok.
- **Offline integrity check** (elitesql check): ok in 16.58 s.
  - right after killing the server (no clean close): exit 3, 0 errors, 685912 warnings; after one clean open/close: exit 0, 0 errors, 0 warnings.
- **Database size**: 86.4 MiB after the first stage, 183.7 MiB after the last; 41608 paid orders in total.

## p99 latency per operation (ms)

| op | share % | 10 | 100 | 500 |
|---|---:|---:|---:|---:|
| add_to_cart | 10.21 | 3.97 | 35.71 | 215.47 |
| admin_dashboard | 1.02 | 12.98 | 126.89 | 4905.53 |
| browse | 20.29 | 1.15 | 16.70 | 34.47 |
| checkout | 3.95 | 4.71 | 44.69 | 120.87 |
| order_history | 3.13 | 3.36 | 23.08 | 45.38 |
| product_detail | 17.36 | 0.65 | 14.45 | 42.65 |
| recommend | 7.06 | 0.79 | 11.92 | 38.15 |
| relogin | 1.55 | 4.52 | 50.17 | 360.14 |
| restock | 0.32 | 3.97 | 26.74 | 97.02 |
| search_text | 8.16 | 0.77 | 11.60 | 28.99 |
| session_check | 12.22 | 3.73 | 29.22 | 208.63 |
| signup | 0.49 | 3.94 | 34.82 | 205.44 |
| update_cart_item | 3.05 | 3.71 | 31.82 | 196.55 |
| update_profile | 1.05 | 3.58 | 23.17 | 203.73 |
| view_cart | 8.13 | 0.58 | 16.19 | 42.80 |
| write_review | 2.01 | 3.65 | 24.14 | 91.06 |

## Errors and business outcomes at the highest level

```json
{
 "statuses": {
  "ok": 236353,
  "empty_cart": 2845,
  "out_of_stock": 110,
  "not_found": 15,
  "error:16": 3
 },
 "errors": {
  "error:16": {
   "count": 3,
   "first": "[elitesql:16] memory limit exceeded: query memory admission timed out",
   "ops": {
    "admin_dashboard": 3
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
