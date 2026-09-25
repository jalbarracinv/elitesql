# Mini-SaaS concurrency simulation — sidecar

Run: `B-2` on 2026-09-23T13:51:09 · commit `92e0290` · macOS-26.6.2-arm64-arm-64bit · 10 CPUs

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
- **seed time**: 1.17 s, 13.1 MiB after seeding

## Capacity and performance curve

Latencies are of the database call (service operation) in milliseconds; `user p99` adds the wait for a pooled connection.

| users | conns | ops/s | reads/s | writes/s | p50 | p95 | p99 | p99.9 | max | user p99 | success % | conflict retry % | srv CPU | srv RSS MiB | gen CPU | thr CV | invariants |
|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|
| 100 | 100 | 18576.7 | 14372.9 | 4203.8 | 1.327 | 19.767 | 36.939 | 94.703 | 1195.452 | 36.942 | 99.988 | 0.72 | 5.18 | 161.0 | 2.45 | 0.073 | ok |

## Insights

- **Peak throughput**: 18576.7 ops/s at 100 users (p99 36.939 ms).
- **Capacity at p99 ≤ 10 ms**: 0 users (DB latency); 0 users counting pool wait.
- **Capacity at p99 ≤ 50 ms**: 100 users (DB latency); 100 users counting pool wait.
- **Capacity at p99 ≤ 100 ms**: 100 users (DB latency); 100 users counting pool wait.
- **Capacity at p99 ≤ 250 ms**: 100 users (DB latency); 100 users counting pool wait.
- **Capacity at p99 ≤ 1000 ms**: 100 users (DB latency); 100 users counting pool wait.
- **Ops per server core-second**: 100→3586.
- **Read-your-writes violations**: none.
- **Business invariants**: all stages ok.
- **Offline integrity check** (elitesql check): ok in 7.41 s.
  - right after killing the server (no clean close): exit 3, 0 errors, 235506 warnings; after one clean open/close: exit 0, 0 errors, 0 warnings.
- **Database size**: 71.3 MiB after the first stage, 71.3 MiB after the last; 11635 paid orders in total.

## p99 latency per operation (ms)

| op | share % | 100 |
|---|---:|---:|
| add_to_cart | 10.11 | 41.46 |
| admin_dashboard | 1.05 | 149.36 |
| browse | 20.45 | 13.80 |
| checkout | 4.09 | 101.03 |
| order_history | 3.04 | 26.01 |
| product_detail | 17.21 | 15.93 |
| recommend | 7.1 | 12.51 |
| relogin | 1.54 | 53.97 |
| restock | 0.31 | 72.72 |
| search_text | 8.15 | 15.37 |
| session_check | 12.18 | 36.52 |
| signup | 0.5 | 38.65 |
| update_cart_item | 3.04 | 33.78 |
| update_profile | 1.01 | 23.93 |
| view_cart | 8.2 | 22.87 |
| write_review | 2.03 | 34.80 |

## Errors and business outcomes at the highest level

```json
{
 "statuses": {
  "ok": 275171,
  "empty_cart": 3428,
  "not_found": 19,
  "conflict_exhausted": 33
 },
 "errors": {
  "conflict_exhausted": {
   "count": 49,
   "first": "[elitesql:9] conflict, retry transaction: products/b3 changed after this transaction began",
   "ops": {
    "checkout": 48,
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
