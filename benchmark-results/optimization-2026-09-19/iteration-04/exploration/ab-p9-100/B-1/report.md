# Mini-SaaS concurrency simulation — sidecar

Run: `B-1` on 2026-09-23T14:43:44 · commit `92e0290` · macOS-26.6.2-arm64-arm-64bit · 10 CPUs

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
| 100 | 100 | 21018.9 | 16261.5 | 4757.5 | 0.939 | 14.174 | 43.792 | 504.436 | 1121.716 | 43.793 | 99.982 | 0.651 | 5.27 | 201.9 | 2.36 | 0.304 | ok |

## Insights

- **Peak throughput**: 21018.9 ops/s at 100 users (p99 43.792 ms).
- **Capacity at p99 ≤ 10 ms**: 0 users (DB latency); 0 users counting pool wait.
- **Capacity at p99 ≤ 50 ms**: 100 users (DB latency); 100 users counting pool wait.
- **Capacity at p99 ≤ 100 ms**: 100 users (DB latency); 100 users counting pool wait.
- **Capacity at p99 ≤ 250 ms**: 100 users (DB latency); 100 users counting pool wait.
- **Capacity at p99 ≤ 1000 ms**: 100 users (DB latency); 100 users counting pool wait.
- **Ops per server core-second**: 100→3988.
- **Read-your-writes violations**: none.
- **Business invariants**: all stages ok.
- **Offline integrity check** (elitesql check): ok in 7.73 s.
  - right after killing the server (no clean close): exit 3, 0 errors, 265924 warnings; after one clean open/close: exit 0, 0 errors, 0 warnings.
- **Database size**: 82.3 MiB after the first stage, 82.3 MiB after the last; 13632 paid orders in total.

## p99 latency per operation (ms)

| op | share % | 100 |
|---|---:|---:|
| add_to_cart | 10.12 | 69.50 |
| admin_dashboard | 1.03 | 158.37 |
| browse | 20.39 | 14.38 |
| checkout | 4.07 | 275.04 |
| order_history | 3.02 | 28.07 |
| product_detail | 17.23 | 24.87 |
| recommend | 7.15 | 20.36 |
| relogin | 1.54 | 86.25 |
| restock | 0.3 | 138.63 |
| search_text | 8.17 | 18.87 |
| session_check | 12.15 | 48.41 |
| signup | 0.51 | 54.91 |
| update_cart_item | 3.03 | 44.02 |
| update_profile | 1.03 | 28.04 |
| view_cart | 8.21 | 26.63 |
| write_review | 2.04 | 80.16 |

## Errors and business outcomes at the highest level

```json
{
 "statuses": {
  "ok": 311402,
  "empty_cart": 3806,
  "conflict_exhausted": 56,
  "not_found": 20
 },
 "errors": {
  "conflict_exhausted": {
   "count": 76,
   "first": "[elitesql:9] conflict, retry transaction: products/b4 changed after this transaction began",
   "ops": {
    "checkout": 76
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
