# Mini-SaaS concurrency simulation — sidecar

Run: `B-2` on 2026-09-23T14:40:56 · commit `92e0290` · macOS-26.6.2-arm64-arm-64bit · 10 CPUs

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
| 100 | 100 | 17586.3 | 13604.8 | 3981.5 | 0.607 | 22.001 | 92.504 | 420.212 | 1102.276 | 92.505 | 99.631 | 1.34 | 5.56 | 200.7 | 3.27 | 0.171 | ok |

## Insights

- **Peak throughput**: 17586.3 ops/s at 100 users (p99 92.504 ms).
- **Capacity at p99 ≤ 10 ms**: 0 users (DB latency); 0 users counting pool wait.
- **Capacity at p99 ≤ 50 ms**: 0 users (DB latency); 0 users counting pool wait.
- **Capacity at p99 ≤ 100 ms**: 100 users (DB latency); 100 users counting pool wait.
- **Capacity at p99 ≤ 250 ms**: 100 users (DB latency); 100 users counting pool wait.
- **Capacity at p99 ≤ 1000 ms**: 100 users (DB latency); 100 users counting pool wait.
- **Ops per server core-second**: 100→3163.
- **Read-your-writes violations**: none.
- **Business invariants**: all stages ok.
- **Offline integrity check** (elitesql check): ok in 6.82 s.
  - right after killing the server (no clean close): exit 3, 0 errors, 231506 warnings; after one clean open/close: exit 0, 0 errors, 0 warnings.
- **Database size**: 71.1 MiB after the first stage, 71.1 MiB after the last; 10894 paid orders in total.

## p99 latency per operation (ms)

| op | share % | 100 |
|---|---:|---:|
| add_to_cart | 10.09 | 25.99 |
| admin_dashboard | 1.04 | 100.60 |
| browse | 20.44 | 5.65 |
| checkout | 4.1 | 449.45 |
| order_history | 3.02 | 12.36 |
| product_detail | 17.18 | 8.84 |
| recommend | 7.14 | 8.72 |
| relogin | 1.55 | 28.72 |
| restock | 0.31 | 407.90 |
| search_text | 8.16 | 7.98 |
| session_check | 12.19 | 17.18 |
| signup | 0.51 | 29.56 |
| update_cart_item | 3.03 | 18.94 |
| update_profile | 1.01 | 12.74 |
| view_cart | 8.19 | 10.91 |
| write_review | 2.04 | 141.30 |

## Errors and business outcomes at the highest level

```json
{
 "statuses": {
  "ok": 259758,
  "empty_cart": 3048,
  "conflict_exhausted": 973,
  "not_found": 16
 },
 "errors": {
  "conflict_exhausted": {
   "count": 1151,
   "first": "[elitesql:9] conflict, retry transaction: products/b3 changed after this transaction began",
   "ops": {
    "restock": 147,
    "checkout": 1000,
    "write_review": 4
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
