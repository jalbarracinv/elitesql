# Mini-SaaS concurrency simulation — sidecar

Run: `A-2` on 2026-09-23T14:32:25 · commit `92e0290` · macOS-26.6.2-arm64-arm-64bit · 10 CPUs

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
| 100 | 100 | 23842.4 | 18448.9 | 5393.5 | 0.845 | 14.72 | 25.444 | 68.201 | 978.69 | 25.445 | 99.99 | 0.717 | 6.11 | 189.2 | 2.55 | 0.08 | ok |

## Insights

- **Peak throughput**: 23842.4 ops/s at 100 users (p99 25.444 ms).
- **Capacity at p99 ≤ 10 ms**: 0 users (DB latency); 0 users counting pool wait.
- **Capacity at p99 ≤ 50 ms**: 100 users (DB latency); 100 users counting pool wait.
- **Capacity at p99 ≤ 100 ms**: 100 users (DB latency); 100 users counting pool wait.
- **Capacity at p99 ≤ 250 ms**: 100 users (DB latency); 100 users counting pool wait.
- **Capacity at p99 ≤ 1000 ms**: 100 users (DB latency); 100 users counting pool wait.
- **Ops per server core-second**: 100→3902.
- **Read-your-writes violations**: none.
- **Business invariants**: all stages ok.
- **Offline integrity check** (elitesql check): ok in 8.41 s.
  - right after killing the server (no clean close): exit 3, 0 errors, 286330 warnings; after one clean open/close: exit 0, 0 errors, 0 warnings.
- **Database size**: 88.7 MiB after the first stage, 88.7 MiB after the last; 15044 paid orders in total.

## p99 latency per operation (ms)

| op | share % | 100 |
|---|---:|---:|
| add_to_cart | 10.12 | 29.40 |
| admin_dashboard | 1.03 | 105.67 |
| browse | 20.42 | 5.97 |
| checkout | 4.08 | 83.45 |
| order_history | 3.01 | 14.87 |
| product_detail | 17.24 | 9.60 |
| recommend | 7.15 | 9.88 |
| relogin | 1.54 | 40.05 |
| restock | 0.3 | 73.99 |
| search_text | 8.18 | 8.65 |
| session_check | 12.16 | 22.99 |
| signup | 0.51 | 28.31 |
| update_cart_item | 3.02 | 23.84 |
| update_profile | 1.03 | 19.90 |
| view_cart | 8.18 | 10.92 |
| write_review | 2.04 | 25.48 |

## Errors and business outcomes at the highest level

```json
{
 "statuses": {
  "ok": 353265,
  "empty_cart": 4315,
  "not_found": 22,
  "conflict_exhausted": 34
 },
 "errors": {
  "conflict_exhausted": {
   "count": 47,
   "first": "[elitesql:9] conflict, retry transaction: products/c12 changed after this transaction began",
   "ops": {
    "checkout": 46,
    "restock": 1
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
