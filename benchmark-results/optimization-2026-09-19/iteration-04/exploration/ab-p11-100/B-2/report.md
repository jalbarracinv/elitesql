# Mini-SaaS concurrency simulation — sidecar

Run: `B-2` on 2026-09-23T14:57:56 · commit `92e0290` · macOS-26.6.2-arm64-arm-64bit · 10 CPUs

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
- **seed time**: 1.11 s, 13.1 MiB after seeding

## Capacity and performance curve

Latencies are of the database call (service operation) in milliseconds; `user p99` adds the wait for a pooled connection.

| users | conns | ops/s | reads/s | writes/s | p50 | p95 | p99 | p99.9 | max | user p99 | success % | conflict retry % | srv CPU | srv RSS MiB | gen CPU | thr CV | invariants |
|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|
| 100 | 100 | 23063.3 | 17845.6 | 5217.7 | 0.89 | 15.293 | 26.65 | 67.353 | 1041.831 | 26.651 | 99.98 | 0.718 | 6.0 | 190.9 | 2.62 | 0.073 | ok |

## Insights

- **Peak throughput**: 23063.3 ops/s at 100 users (p99 26.65 ms).
- **Capacity at p99 ≤ 10 ms**: 0 users (DB latency); 0 users counting pool wait.
- **Capacity at p99 ≤ 50 ms**: 100 users (DB latency); 100 users counting pool wait.
- **Capacity at p99 ≤ 100 ms**: 100 users (DB latency); 100 users counting pool wait.
- **Capacity at p99 ≤ 250 ms**: 100 users (DB latency); 100 users counting pool wait.
- **Capacity at p99 ≤ 1000 ms**: 100 users (DB latency); 100 users counting pool wait.
- **Ops per server core-second**: 100→3844.
- **Read-your-writes violations**: none.
- **Business invariants**: all stages ok.
- **Offline integrity check** (elitesql check): ok in 8.15 s.
  - right after killing the server (no clean close): exit 3, 0 errors, 280128 warnings; after one clean open/close: exit 0, 0 errors, 0 warnings.
- **Database size**: 86.9 MiB after the first stage, 86.9 MiB after the last; 14591 paid orders in total.

## p99 latency per operation (ms)

| op | share % | 100 |
|---|---:|---:|
| add_to_cart | 10.11 | 31.31 |
| admin_dashboard | 1.02 | 102.40 |
| browse | 20.4 | 6.93 |
| checkout | 4.07 | 82.37 |
| order_history | 3.02 | 15.89 |
| product_detail | 17.25 | 10.82 |
| recommend | 7.15 | 9.80 |
| relogin | 1.54 | 41.96 |
| restock | 0.3 | 82.96 |
| search_text | 8.19 | 8.95 |
| session_check | 12.16 | 24.80 |
| signup | 0.51 | 31.63 |
| update_cart_item | 3.04 | 24.25 |
| update_profile | 1.03 | 21.03 |
| view_cart | 8.21 | 12.39 |
| write_review | 2.04 | 24.13 |

## Errors and business outcomes at the highest level

```json
{
 "statuses": {
  "ok": 341703,
  "empty_cart": 4157,
  "not_found": 21,
  "conflict_exhausted": 68
 },
 "errors": {
  "conflict_exhausted": {
   "count": 87,
   "first": "[elitesql:9] conflict, retry transaction: products/b3 changed after this transaction began",
   "ops": {
    "checkout": 82,
    "restock": 5
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
