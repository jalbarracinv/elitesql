# Mini-SaaS concurrency simulation — sidecar

Run: `B-2` on 2026-09-23T14:51:43 · commit `92e0290` · macOS-26.6.2-arm64-arm-64bit · 10 CPUs

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
| 100 | 100 | 22262.4 | 17222.7 | 5039.7 | 1.333 | 16.931 | 26.543 | 63.711 | 1082.072 | 26.544 | 99.978 | 0.753 | 6.43 | 189.0 | 2.67 | 0.071 | ok |

## Insights

- **Peak throughput**: 22262.4 ops/s at 100 users (p99 26.543 ms).
- **Capacity at p99 ≤ 10 ms**: 0 users (DB latency); 0 users counting pool wait.
- **Capacity at p99 ≤ 50 ms**: 100 users (DB latency); 100 users counting pool wait.
- **Capacity at p99 ≤ 100 ms**: 100 users (DB latency); 100 users counting pool wait.
- **Capacity at p99 ≤ 250 ms**: 100 users (DB latency); 100 users counting pool wait.
- **Capacity at p99 ≤ 1000 ms**: 100 users (DB latency); 100 users counting pool wait.
- **Ops per server core-second**: 100→3462.
- **Read-your-writes violations**: none.
- **Business invariants**: all stages ok.
- **Offline integrity check** (elitesql check): ok in 8.07 s.
  - right after killing the server (no clean close): exit 3, 0 errors, 273166 warnings; after one clean open/close: exit 0, 0 errors, 0 warnings.
- **Database size**: 84.9 MiB after the first stage, 84.9 MiB after the last; 14106 paid orders in total.

## p99 latency per operation (ms)

| op | share % | 100 |
|---|---:|---:|
| add_to_cart | 10.12 | 27.91 |
| admin_dashboard | 1.02 | 56.47 |
| browse | 20.4 | 5.07 |
| checkout | 4.07 | 122.40 |
| order_history | 3.01 | 9.07 |
| product_detail | 17.22 | 7.65 |
| recommend | 7.13 | 7.02 |
| relogin | 1.54 | 36.08 |
| restock | 0.3 | 68.55 |
| search_text | 8.2 | 5.08 |
| session_check | 12.17 | 24.15 |
| signup | 0.51 | 25.22 |
| update_cart_item | 3.03 | 24.81 |
| update_profile | 1.03 | 23.87 |
| view_cart | 8.21 | 8.07 |
| write_review | 2.04 | 20.85 |

## Errors and business outcomes at the highest level

```json
{
 "statuses": {
  "ok": 329820,
  "empty_cart": 4024,
  "conflict_exhausted": 72,
  "not_found": 20
 },
 "errors": {
  "conflict_exhausted": {
   "count": 99,
   "first": "[elitesql:9] conflict, retry transaction: products/d226 changed after this transaction began",
   "ops": {
    "checkout": 98,
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
