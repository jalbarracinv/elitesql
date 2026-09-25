# Mini-SaaS concurrency simulation — sidecar

Run: `A-2` on 2026-09-23T13:03:46 · commit `92e0290` · macOS-26.6.2-arm64-arm-64bit · 10 CPUs

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
- **seed time**: 1.18 s, 13.1 MiB after seeding

## Capacity and performance curve

Latencies are of the database call (service operation) in milliseconds; `user p99` adds the wait for a pooled connection.

| users | conns | ops/s | reads/s | writes/s | p50 | p95 | p99 | p99.9 | max | user p99 | success % | conflict retry % | srv CPU | srv RSS MiB | gen CPU | thr CV | invariants |
|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|
| 100 | 100 | 17730.5 | 13715.6 | 4014.9 | 1.941 | 18.746 | 37.104 | 90.38 | 828.718 | 37.105 | 99.998 | 0.49 | 5.03 | 161.2 | 2.02 | 0.129 | ok |

## Insights

- **Peak throughput**: 17730.5 ops/s at 100 users (p99 37.104 ms).
- **Capacity at p99 ≤ 10 ms**: 0 users (DB latency); 0 users counting pool wait.
- **Capacity at p99 ≤ 50 ms**: 100 users (DB latency); 100 users counting pool wait.
- **Capacity at p99 ≤ 100 ms**: 100 users (DB latency); 100 users counting pool wait.
- **Capacity at p99 ≤ 250 ms**: 100 users (DB latency); 100 users counting pool wait.
- **Capacity at p99 ≤ 1000 ms**: 100 users (DB latency); 100 users counting pool wait.
- **Ops per server core-second**: 100→3525.
- **Read-your-writes violations**: none.
- **Business invariants**: all stages ok.
- **Offline integrity check** (elitesql check): ok in 7.2 s.
  - right after killing the server (no clean close): exit 3, 0 errors, 227736 warnings; after one clean open/close: exit 0, 0 errors, 0 warnings.
- **Database size**: 68.6 MiB after the first stage, 68.6 MiB after the last; 11139 paid orders in total.

## p99 latency per operation (ms)

| op | share % | 100 |
|---|---:|---:|
| add_to_cart | 10.11 | 43.98 |
| admin_dashboard | 1.04 | 114.05 |
| browse | 20.48 | 19.26 |
| checkout | 4.1 | 67.13 |
| order_history | 3.04 | 28.86 |
| product_detail | 17.21 | 26.75 |
| recommend | 7.11 | 18.31 |
| relogin | 1.53 | 60.86 |
| restock | 0.31 | 90.84 |
| search_text | 8.13 | 20.27 |
| session_check | 12.15 | 44.35 |
| signup | 0.5 | 45.25 |
| update_cart_item | 3.04 | 36.77 |
| update_profile | 1.01 | 38.06 |
| view_cart | 8.19 | 24.85 |
| write_review | 2.03 | 36.23 |

## Errors and business outcomes at the highest level

```json
{
 "statuses": {
  "ok": 262641,
  "empty_cart": 3297,
  "not_found": 16,
  "conflict_exhausted": 4
 },
 "errors": {
  "conflict_exhausted": {
   "count": 8,
   "first": "[elitesql:9] conflict, retry transaction: products/c16 changed after this transaction began",
   "ops": {
    "checkout": 8
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
