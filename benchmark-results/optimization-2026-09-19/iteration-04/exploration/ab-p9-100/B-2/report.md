# Mini-SaaS concurrency simulation — sidecar

Run: `B-2` on 2026-09-23T14:45:07 · commit `92e0290` · macOS-26.6.2-arm64-arm-64bit · 10 CPUs

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
- **seed time**: 1.13 s, 13.1 MiB after seeding

## Capacity and performance curve

Latencies are of the database call (service operation) in milliseconds; `user p99` adds the wait for a pooled connection.

| users | conns | ops/s | reads/s | writes/s | p50 | p95 | p99 | p99.9 | max | user p99 | success % | conflict retry % | srv CPU | srv RSS MiB | gen CPU | thr CV | invariants |
|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|
| 100 | 100 | 21036.5 | 16276.3 | 4760.3 | 0.987 | 14.229 | 44.53 | 508.903 | 873.752 | 44.531 | 99.992 | 0.655 | 5.35 | 173.8 | 2.29 | 0.292 | ok |

## Insights

- **Peak throughput**: 21036.5 ops/s at 100 users (p99 44.53 ms).
- **Capacity at p99 ≤ 10 ms**: 0 users (DB latency); 0 users counting pool wait.
- **Capacity at p99 ≤ 50 ms**: 100 users (DB latency); 100 users counting pool wait.
- **Capacity at p99 ≤ 100 ms**: 100 users (DB latency); 100 users counting pool wait.
- **Capacity at p99 ≤ 250 ms**: 100 users (DB latency); 100 users counting pool wait.
- **Capacity at p99 ≤ 1000 ms**: 100 users (DB latency); 100 users counting pool wait.
- **Ops per server core-second**: 100→3932.
- **Read-your-writes violations**: none.
- **Business invariants**: all stages ok.
- **Offline integrity check** (elitesql check): ok in 7.76 s.
  - right after killing the server (no clean close): exit 3, 0 errors, 266474 warnings; after one clean open/close: exit 0, 0 errors, 0 warnings.
- **Database size**: 82.5 MiB after the first stage, 82.5 MiB after the last; 13685 paid orders in total.

## p99 latency per operation (ms)

| op | share % | 100 |
|---|---:|---:|
| add_to_cart | 10.11 | 80.20 |
| admin_dashboard | 1.02 | 186.81 |
| browse | 20.4 | 14.90 |
| checkout | 4.07 | 187.48 |
| order_history | 3.03 | 29.24 |
| product_detail | 17.21 | 24.18 |
| recommend | 7.15 | 22.80 |
| relogin | 1.54 | 87.65 |
| restock | 0.3 | 165.47 |
| search_text | 8.18 | 19.30 |
| session_check | 12.18 | 51.95 |
| signup | 0.51 | 54.61 |
| update_cart_item | 3.03 | 49.61 |
| update_profile | 1.03 | 23.74 |
| view_cart | 8.2 | 24.89 |
| write_review | 2.04 | 61.44 |

## Errors and business outcomes at the highest level

```json
{
 "statuses": {
  "ok": 311660,
  "empty_cart": 3843,
  "not_found": 20,
  "conflict_exhausted": 25
 },
 "errors": {
  "conflict_exhausted": {
   "count": 33,
   "first": "[elitesql:9] conflict, retry transaction: products/b1 changed after this transaction began",
   "ops": {
    "checkout": 30,
    "restock": 3
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
