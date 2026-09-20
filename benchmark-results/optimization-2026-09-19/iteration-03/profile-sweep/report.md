# Mini-SaaS concurrency simulation — sidecar

Run: `profile-sweep` on 2026-09-19T23:23:35 · commit `b4dda86` · macOS-26.6.2-arm64-arm-64bit · 10 CPUs

## Configuration

- **transport**: sidecar
- **levels**: [100]
- **duration**: 20.0
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
- **seed time**: 1.21 s, 13.1 MiB after seeding

## Capacity and performance curve

Latencies are of the database call (service operation) in milliseconds; `user p99` adds the wait for a pooled connection.

| users | conns | ops/s | reads/s | writes/s | p50 | p95 | p99 | p99.9 | max | user p99 | success % | conflict retry % | srv CPU | srv RSS MiB | gen CPU | thr CV | invariants |
|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|
| 100 | 100 | 16009.6 | 12387.7 | 3621.9 | 2.233 | 21.527 | 42.147 | 107.701 | 976.693 | 42.149 | 99.99 | 0.498 | 4.5 | 170.7 | 1.9 | 0.181 | ok |

## Insights

- **Peak throughput**: 16009.6 ops/s at 100 users (p99 42.147 ms).
- **Capacity at p99 ≤ 10 ms**: 0 users (DB latency); 0 users counting pool wait.
- **Capacity at p99 ≤ 50 ms**: 100 users (DB latency); 100 users counting pool wait.
- **Capacity at p99 ≤ 100 ms**: 100 users (DB latency); 100 users counting pool wait.
- **Capacity at p99 ≤ 250 ms**: 100 users (DB latency); 100 users counting pool wait.
- **Capacity at p99 ≤ 1000 ms**: 100 users (DB latency); 100 users counting pool wait.
- **Ops per server core-second**: 100→3558.
- **Read-your-writes violations**: none.
- **Business invariants**: all stages ok.
- **Offline integrity check** (elitesql check): ok in 7.51 s.
  - right after killing the server (no clean close): exit 3, 0 errors, 239166 warnings; after one clean open/close: exit 0, 0 errors, 0 warnings.
- **Database size**: 73.0 MiB after the first stage, 73.0 MiB after the last; 11880 paid orders in total.

## p99 latency per operation (ms)

| op | share % | 100 |
|---|---:|---:|
| add_to_cart | 10.1 | 48.51 |
| admin_dashboard | 1.05 | 123.98 |
| browse | 20.41 | 20.05 |
| checkout | 4.09 | 88.84 |
| order_history | 3.04 | 34.01 |
| product_detail | 17.2 | 29.98 |
| recommend | 7.13 | 21.07 |
| relogin | 1.54 | 73.55 |
| restock | 0.31 | 75.50 |
| search_text | 8.16 | 22.61 |
| session_check | 12.2 | 49.68 |
| signup | 0.51 | 42.82 |
| update_cart_item | 3.03 | 40.83 |
| update_profile | 1.03 | 38.02 |
| view_cart | 8.19 | 27.91 |
| write_review | 2.03 | 38.58 |

## Errors and business outcomes at the highest level

```json
{
 "statuses": {
  "ok": 316198,
  "empty_cart": 3939,
  "not_found": 22,
  "conflict_exhausted": 33
 },
 "errors": {
  "conflict_exhausted": {
   "count": 35,
   "first": "[elitesql:9] conflict, retry transaction: products/b4 changed after this transaction began",
   "ops": {
    "checkout": 35
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
