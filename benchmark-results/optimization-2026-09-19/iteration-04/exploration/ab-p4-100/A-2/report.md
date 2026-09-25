# Mini-SaaS concurrency simulation — sidecar

Run: `A-2` on 2026-09-23T13:40:07 · commit `92e0290` · macOS-26.6.2-arm64-arm-64bit · 10 CPUs

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
- **seed time**: 1.19 s, 13.1 MiB after seeding

## Capacity and performance curve

Latencies are of the database call (service operation) in milliseconds; `user p99` adds the wait for a pooled connection.

| users | conns | ops/s | reads/s | writes/s | p50 | p95 | p99 | p99.9 | max | user p99 | success % | conflict retry % | srv CPU | srv RSS MiB | gen CPU | thr CV | invariants |
|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|
| 100 | 100 | 17496.1 | 13534.3 | 3961.8 | 2.029 | 19.767 | 37.973 | 87.175 | 985.872 | 37.975 | 99.987 | 0.506 | 5.07 | 158.6 | 2.13 | 0.082 | ok |

## Insights

- **Peak throughput**: 17496.1 ops/s at 100 users (p99 37.973 ms).
- **Capacity at p99 ≤ 10 ms**: 0 users (DB latency); 0 users counting pool wait.
- **Capacity at p99 ≤ 50 ms**: 100 users (DB latency); 100 users counting pool wait.
- **Capacity at p99 ≤ 100 ms**: 100 users (DB latency); 100 users counting pool wait.
- **Capacity at p99 ≤ 250 ms**: 100 users (DB latency); 100 users counting pool wait.
- **Capacity at p99 ≤ 1000 ms**: 100 users (DB latency); 100 users counting pool wait.
- **Ops per server core-second**: 100→3451.
- **Read-your-writes violations**: none.
- **Business invariants**: all stages ok.
- **Offline integrity check** (elitesql check): ok in 7.15 s.
  - right after killing the server (no clean close): exit 3, 0 errors, 225436 warnings; after one clean open/close: exit 0, 0 errors, 0 warnings.
- **Database size**: 68.2 MiB after the first stage, 68.2 MiB after the last; 10974 paid orders in total.

## p99 latency per operation (ms)

| op | share % | 100 |
|---|---:|---:|
| add_to_cart | 10.1 | 42.89 |
| admin_dashboard | 1.04 | 116.21 |
| browse | 20.48 | 19.01 |
| checkout | 4.11 | 85.77 |
| order_history | 3.04 | 32.51 |
| product_detail | 17.2 | 28.26 |
| recommend | 7.11 | 16.97 |
| relogin | 1.53 | 66.99 |
| restock | 0.31 | 67.86 |
| search_text | 8.14 | 20.84 |
| session_check | 12.14 | 44.36 |
| signup | 0.51 | 42.12 |
| update_cart_item | 3.04 | 35.81 |
| update_profile | 1.02 | 36.62 |
| view_cart | 8.21 | 23.31 |
| write_review | 2.03 | 35.39 |

## Errors and business outcomes at the highest level

```json
{
 "statuses": {
  "ok": 259134,
  "empty_cart": 3257,
  "conflict_exhausted": 35,
  "not_found": 16
 },
 "errors": {
  "conflict_exhausted": {
   "count": 39,
   "first": "[elitesql:9] conflict, retry transaction: products/b8 changed after this transaction began",
   "ops": {
    "checkout": 39
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
