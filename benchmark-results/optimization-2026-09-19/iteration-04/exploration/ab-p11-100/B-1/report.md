# Mini-SaaS concurrency simulation — sidecar

Run: `B-1` on 2026-09-23T14:56:33 · commit `92e0290` · macOS-26.6.2-arm64-arm-64bit · 10 CPUs

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
| 100 | 100 | 23404.5 | 18116.2 | 5288.3 | 0.842 | 15.104 | 25.974 | 69.227 | 1032.052 | 25.975 | 99.988 | 0.715 | 5.96 | 185.4 | 2.6 | 0.079 | ok |

## Insights

- **Peak throughput**: 23404.5 ops/s at 100 users (p99 25.974 ms).
- **Capacity at p99 ≤ 10 ms**: 0 users (DB latency); 0 users counting pool wait.
- **Capacity at p99 ≤ 50 ms**: 100 users (DB latency); 100 users counting pool wait.
- **Capacity at p99 ≤ 100 ms**: 100 users (DB latency); 100 users counting pool wait.
- **Capacity at p99 ≤ 250 ms**: 100 users (DB latency); 100 users counting pool wait.
- **Capacity at p99 ≤ 1000 ms**: 100 users (DB latency); 100 users counting pool wait.
- **Ops per server core-second**: 100→3927.
- **Read-your-writes violations**: none.
- **Business invariants**: all stages ok.
- **Offline integrity check** (elitesql check): ok in 8.23 s.
  - right after killing the server (no clean close): exit 3, 0 errors, 283024 warnings; after one clean open/close: exit 0, 0 errors, 0 warnings.
- **Database size**: 87.8 MiB after the first stage, 87.8 MiB after the last; 14814 paid orders in total.

## p99 latency per operation (ms)

| op | share % | 100 |
|---|---:|---:|
| add_to_cart | 10.11 | 29.05 |
| admin_dashboard | 1.03 | 99.13 |
| browse | 20.4 | 6.23 |
| checkout | 4.07 | 78.37 |
| order_history | 3.03 | 14.36 |
| product_detail | 17.23 | 9.72 |
| recommend | 7.15 | 9.27 |
| relogin | 1.53 | 41.43 |
| restock | 0.3 | 92.93 |
| search_text | 8.2 | 9.01 |
| session_check | 12.17 | 23.32 |
| signup | 0.51 | 28.78 |
| update_cart_item | 3.02 | 23.92 |
| update_profile | 1.02 | 19.80 |
| view_cart | 8.2 | 11.74 |
| write_review | 2.03 | 26.11 |

## Errors and business outcomes at the highest level

```json
{
 "statuses": {
  "ok": 346769,
  "empty_cart": 4236,
  "conflict_exhausted": 41,
  "not_found": 22
 },
 "errors": {
  "conflict_exhausted": {
   "count": 53,
   "first": "[elitesql:9] conflict, retry transaction: products/c20 changed after this transaction began",
   "ops": {
    "checkout": 51,
    "restock": 2
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
