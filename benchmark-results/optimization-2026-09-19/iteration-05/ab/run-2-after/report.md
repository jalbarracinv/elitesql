# Mini-SaaS concurrency simulation — sidecar

Run: `run-2-after` on 2026-09-23T23:31:47 · commit `92e0290` · macOS-26.6.2-arm64-arm-64bit · 10 CPUs

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
| 10 | 10 | 16182.2 | 12527.8 | 3654.4 | 0.379 | 1.642 | 4.953 | 10.046 | 551.06 | 4.954 | 100.0 | 0.0 | 4.13 | 191.5 | 1.43 | 0.138 | ok |
| 100 | 100 | 19574.9 | 15138.9 | 4436.1 | 1.324 | 17.334 | 30.769 | 70.595 | 1203.627 | 30.77 | 100.0 | 0.0 | 6.49 | 288.3 | 1.9 | 0.078 | ok |
| 500 | 500 | 13692.2 | 10576.5 | 3115.8 | 2.808 | 125.739 | 196.223 | 1262.589 | 4123.438 | 196.224 | 100.0 | 0.0 | 6.82 | 442.2 | 1.66 | 0.135 | ok |

## Insights

- **Peak throughput**: 19574.9 ops/s at 100 users (p99 30.769 ms).
- **Capacity at p99 ≤ 10 ms**: 10 users (DB latency); 10 users counting pool wait.
- **Capacity at p99 ≤ 50 ms**: 100 users (DB latency); 100 users counting pool wait.
- **Capacity at p99 ≤ 100 ms**: 100 users (DB latency); 100 users counting pool wait.
- **Capacity at p99 ≤ 250 ms**: 500 users (DB latency); 500 users counting pool wait.
- **Capacity at p99 ≤ 1000 ms**: 500 users (DB latency); 500 users counting pool wait.
- **USL fit**: σ (contention) = 0.22, κ (coherency) = 2.51e-04, predicted peak at N ≈ 55.7 (rmse log 0.0166).
- **Ops per server core-second**: 10→3918, 100→3016, 500→2008.
- **Read-your-writes violations**: none.
- **Business invariants**: all stages ok.
- **Offline integrity check** (elitesql check): ok in 17.05 s.
  - right after killing the server (no clean close): exit 3, 0 errors, 699192 warnings; after one clean open/close: exit 0, 0 errors, 0 warnings.
- **Database size**: 86.2 MiB after the first stage, 189.4 MiB after the last; 42511 paid orders in total.

## p99 latency per operation (ms)

| op | share % | 10 | 100 | 500 |
|---|---:|---:|---:|---:|
| add_to_cart | 10.19 | 4.03 | 37.22 | 176.37 |
| admin_dashboard | 1.02 | 13.33 | 122.76 | 3216.91 |
| browse | 20.3 | 1.15 | 18.07 | 22.63 |
| checkout | 3.94 | 4.55 | 44.67 | 112.58 |
| order_history | 3.13 | 3.35 | 23.54 | 27.74 |
| product_detail | 17.38 | 0.64 | 14.36 | 18.86 |
| recommend | 7.06 | 0.78 | 12.05 | 17.98 |
| relogin | 1.55 | 4.57 | 56.75 | 300.51 |
| restock | 0.32 | 3.70 | 26.11 | 75.78 |
| search_text | 8.15 | 0.77 | 12.86 | 18.88 |
| session_check | 12.23 | 3.70 | 29.35 | 172.50 |
| signup | 0.49 | 3.82 | 37.75 | 171.72 |
| update_cart_item | 3.04 | 3.71 | 31.46 | 165.47 |
| update_profile | 1.04 | 3.42 | 24.02 | 163.69 |
| view_cart | 8.14 | 0.59 | 16.74 | 22.51 |
| write_review | 2.01 | 3.56 | 27.52 | 60.95 |

## Errors and business outcomes at the highest level

```json
{
 "statuses": {
  "ok": 270418,
  "empty_cart": 3276,
  "out_of_stock": 133,
  "not_found": 18
 },
 "errors": {}
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
