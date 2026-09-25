# Mini-SaaS concurrency simulation — sidecar

Run: `A-2` on 2026-09-23T13:58:51 · commit `92e0290` · macOS-26.6.2-arm64-arm-64bit · 10 CPUs

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
| 100 | 100 | 21939.3 | 16971.3 | 4967.9 | 1.717 | 15.683 | 29.555 | 63.326 | 810.684 | 29.556 | 99.99 | 0.525 | 6.1 | 180.0 | 2.38 | 0.072 | ok |

## Insights

- **Peak throughput**: 21939.3 ops/s at 100 users (p99 29.555 ms).
- **Capacity at p99 ≤ 10 ms**: 0 users (DB latency); 0 users counting pool wait.
- **Capacity at p99 ≤ 50 ms**: 100 users (DB latency); 100 users counting pool wait.
- **Capacity at p99 ≤ 100 ms**: 100 users (DB latency); 100 users counting pool wait.
- **Capacity at p99 ≤ 250 ms**: 100 users (DB latency); 100 users counting pool wait.
- **Capacity at p99 ≤ 1000 ms**: 100 users (DB latency); 100 users counting pool wait.
- **Ops per server core-second**: 100→3597.
- **Read-your-writes violations**: none.
- **Business invariants**: all stages ok.
- **Offline integrity check** (elitesql check): ok in 7.92 s.
  - right after killing the server (no clean close): exit 3, 0 errors, 268604 warnings; after one clean open/close: exit 0, 0 errors, 0 warnings.
- **Database size**: 83.3 MiB after the first stage, 83.3 MiB after the last; 13821 paid orders in total.

## p99 latency per operation (ms)

| op | share % | 100 |
|---|---:|---:|
| add_to_cart | 10.14 | 32.00 |
| admin_dashboard | 1.02 | 79.41 |
| browse | 20.4 | 15.62 |
| checkout | 4.06 | 63.37 |
| order_history | 3.01 | 26.56 |
| product_detail | 17.23 | 21.71 |
| recommend | 7.14 | 12.90 |
| relogin | 1.54 | 50.83 |
| restock | 0.3 | 52.66 |
| search_text | 8.17 | 18.07 |
| session_check | 12.17 | 35.13 |
| signup | 0.51 | 30.27 |
| update_cart_item | 3.02 | 27.94 |
| update_profile | 1.03 | 27.17 |
| view_cart | 8.21 | 21.28 |
| write_review | 2.04 | 27.90 |

## Errors and business outcomes at the highest level

```json
{
 "statuses": {
  "ok": 325053,
  "empty_cart": 3982,
  "not_found": 21,
  "conflict_exhausted": 33
 },
 "errors": {
  "conflict_exhausted": {
   "count": 38,
   "first": "[elitesql:9] conflict, retry transaction: products/c16 changed after this transaction began",
   "ops": {
    "checkout": 38
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
