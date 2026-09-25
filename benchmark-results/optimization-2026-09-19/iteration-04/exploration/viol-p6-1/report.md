# Mini-SaaS concurrency simulation — sidecar

Run: `viol-p6-1` on 2026-09-23T16:47:07 · commit `92e0290` · macOS-26.6.2-arm64-arm-64bit · 10 CPUs

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
| 100 | 100 | 23145.1 | 17907.9 | 5237.2 | 0.878 | 15.264 | 26.966 | 74.361 | 1028.777 | 26.968 | 99.984 | 0.7 | 6.0 | 185.8 | 2.63 | 0.076 | ok |

## Insights

- **Peak throughput**: 23145.1 ops/s at 100 users (p99 26.966 ms).
- **Capacity at p99 ≤ 10 ms**: 0 users (DB latency); 0 users counting pool wait.
- **Capacity at p99 ≤ 50 ms**: 100 users (DB latency); 100 users counting pool wait.
- **Capacity at p99 ≤ 100 ms**: 100 users (DB latency); 100 users counting pool wait.
- **Capacity at p99 ≤ 250 ms**: 100 users (DB latency); 100 users counting pool wait.
- **Capacity at p99 ≤ 1000 ms**: 100 users (DB latency); 100 users counting pool wait.
- **Ops per server core-second**: 100→3858.
- **Read-your-writes violations**: none.
- **Business invariants**: all stages ok.
- **Offline integrity check** (elitesql check): ok in 8.22 s.
  - right after killing the server (no clean close): exit 3, 0 errors, 280830 warnings; after one clean open/close: exit 0, 0 errors, 0 warnings.
- **Database size**: 87.1 MiB after the first stage, 87.1 MiB after the last; 14664 paid orders in total.

## p99 latency per operation (ms)

| op | share % | 100 |
|---|---:|---:|
| add_to_cart | 10.12 | 32.99 |
| admin_dashboard | 1.03 | 104.02 |
| browse | 20.37 | 6.97 |
| checkout | 4.08 | 98.33 |
| order_history | 3.02 | 15.85 |
| product_detail | 17.26 | 11.49 |
| recommend | 7.15 | 10.77 |
| relogin | 1.54 | 46.59 |
| restock | 0.3 | 104.09 |
| search_text | 8.19 | 9.70 |
| session_check | 12.18 | 23.60 |
| signup | 0.51 | 29.49 |
| update_cart_item | 3.02 | 25.00 |
| update_profile | 1.03 | 20.91 |
| view_cart | 8.18 | 12.33 |
| write_review | 2.03 | 27.39 |

## Errors and business outcomes at the highest level

```json
{
 "statuses": {
  "ok": 342922,
  "empty_cart": 4178,
  "conflict_exhausted": 56,
  "not_found": 21
 },
 "errors": {
  "conflict_exhausted": {
   "count": 63,
   "first": "[elitesql:9] conflict, retry transaction: products/c53 changed after this transaction began",
   "ops": {
    "checkout": 59,
    "restock": 4
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
