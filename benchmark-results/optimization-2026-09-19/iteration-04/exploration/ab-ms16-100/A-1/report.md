# Mini-SaaS concurrency simulation — sidecar

Run: `A-1` on 2026-09-23T13:29:48 · commit `92e0290` · macOS-26.6.2-arm64-arm-64bit · 10 CPUs

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
| 100 | 100 | 15722.9 | 12171.1 | 3551.9 | 2.85 | 20.864 | 42.672 | 124.499 | 1101.281 | 42.674 | 99.968 | 0.653 | 4.77 | 145.3 | 2.02 | 0.075 | ok |

## Insights

- **Peak throughput**: 15722.9 ops/s at 100 users (p99 42.672 ms).
- **Capacity at p99 ≤ 10 ms**: 0 users (DB latency); 0 users counting pool wait.
- **Capacity at p99 ≤ 50 ms**: 100 users (DB latency); 100 users counting pool wait.
- **Capacity at p99 ≤ 100 ms**: 100 users (DB latency); 100 users counting pool wait.
- **Capacity at p99 ≤ 250 ms**: 100 users (DB latency); 100 users counting pool wait.
- **Capacity at p99 ≤ 1000 ms**: 100 users (DB latency); 100 users counting pool wait.
- **Ops per server core-second**: 100→3296.
- **Read-your-writes violations**: none.
- **Business invariants**: all stages ok.
- **Offline integrity check** (elitesql check): ok in 6.51 s.
  - right after killing the server (no clean close): exit 3, 0 errors, 206790 warnings; after one clean open/close: exit 0, 0 errors, 0 warnings.
- **Database size**: 62.3 MiB after the first stage, 62.3 MiB after the last; 9705 paid orders in total.

## p99 latency per operation (ms)

| op | share % | 100 |
|---|---:|---:|
| add_to_cart | 10.1 | 51.75 |
| admin_dashboard | 1.03 | 93.44 |
| browse | 20.54 | 18.27 |
| checkout | 4.1 | 266.71 |
| order_history | 3.07 | 29.69 |
| product_detail | 17.18 | 26.55 |
| recommend | 7.11 | 25.56 |
| relogin | 1.52 | 76.61 |
| restock | 0.31 | 105.40 |
| search_text | 8.16 | 18.59 |
| session_check | 12.14 | 37.84 |
| signup | 0.5 | 47.16 |
| update_cart_item | 3.02 | 40.40 |
| update_profile | 1.01 | 27.73 |
| view_cart | 8.17 | 29.72 |
| write_review | 2.02 | 40.16 |

## Errors and business outcomes at the highest level

```json
{
 "statuses": {
  "ok": 232837,
  "empty_cart": 2915,
  "conflict_exhausted": 76,
  "not_found": 16
 },
 "errors": {
  "conflict_exhausted": {
   "count": 82,
   "first": "[elitesql:9] conflict, retry transaction: products/c18 changed after this transaction began",
   "ops": {
    "checkout": 82
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
