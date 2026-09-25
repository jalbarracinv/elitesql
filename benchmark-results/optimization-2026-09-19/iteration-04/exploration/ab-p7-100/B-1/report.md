# Mini-SaaS concurrency simulation — sidecar

Run: `B-1` on 2026-09-23T14:31:43 · commit `92e0290` · macOS-26.6.2-arm64-arm-64bit · 10 CPUs

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
| 100 | 100 | 23740.7 | 18370.5 | 5370.3 | 0.837 | 14.752 | 26.512 | 67.56 | 1000.86 | 26.513 | 99.989 | 0.71 | 6.08 | 183.5 | 2.53 | 0.075 | ok |

## Insights

- **Peak throughput**: 23740.7 ops/s at 100 users (p99 26.512 ms).
- **Capacity at p99 ≤ 10 ms**: 0 users (DB latency); 0 users counting pool wait.
- **Capacity at p99 ≤ 50 ms**: 100 users (DB latency); 100 users counting pool wait.
- **Capacity at p99 ≤ 100 ms**: 100 users (DB latency); 100 users counting pool wait.
- **Capacity at p99 ≤ 250 ms**: 100 users (DB latency); 100 users counting pool wait.
- **Capacity at p99 ≤ 1000 ms**: 100 users (DB latency); 100 users counting pool wait.
- **Ops per server core-second**: 100→3905.
- **Read-your-writes violations**: none.
- **Business invariants**: all stages ok.
- **Offline integrity check** (elitesql check): ok in 8.41 s.
  - right after killing the server (no clean close): exit 3, 0 errors, 285576 warnings; after one clean open/close: exit 0, 0 errors, 0 warnings.
- **Database size**: 88.5 MiB after the first stage, 88.5 MiB after the last; 14972 paid orders in total.

## p99 latency per operation (ms)

| op | share % | 100 |
|---|---:|---:|
| add_to_cart | 10.11 | 31.94 |
| admin_dashboard | 1.03 | 103.80 |
| browse | 20.4 | 6.13 |
| checkout | 4.07 | 80.81 |
| order_history | 3.02 | 14.55 |
| product_detail | 17.23 | 10.30 |
| recommend | 7.15 | 9.83 |
| relogin | 1.54 | 43.44 |
| restock | 0.3 | 92.70 |
| search_text | 8.19 | 8.69 |
| session_check | 12.17 | 24.15 |
| signup | 0.51 | 26.51 |
| update_cart_item | 3.03 | 25.13 |
| update_profile | 1.03 | 19.84 |
| view_cart | 8.2 | 11.48 |
| write_review | 2.03 | 24.94 |

## Errors and business outcomes at the highest level

```json
{
 "statuses": {
  "ok": 351748,
  "empty_cart": 4301,
  "not_found": 24,
  "conflict_exhausted": 38
 },
 "errors": {
  "conflict_exhausted": {
   "count": 52,
   "first": "[elitesql:9] conflict, retry transaction: products/b4 changed after this transaction began",
   "ops": {
    "checkout": 51,
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
