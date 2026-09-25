# Mini-SaaS concurrency simulation — sidecar

Run: `B-1` on 2026-09-23T14:11:52 · commit `92e0290` · macOS-26.6.2-arm64-arm-64bit · 10 CPUs

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
| 100 | 100 | 22558.2 | 17451.1 | 5107.1 | 1.738 | 15.172 | 30.122 | 109.79 | 939.865 | 30.123 | 99.968 | 0.75 | 5.79 | 179.3 | 2.54 | 0.076 | ok |

## Insights

- **Peak throughput**: 22558.2 ops/s at 100 users (p99 30.122 ms).
- **Capacity at p99 ≤ 10 ms**: 0 users (DB latency); 0 users counting pool wait.
- **Capacity at p99 ≤ 50 ms**: 100 users (DB latency); 100 users counting pool wait.
- **Capacity at p99 ≤ 100 ms**: 100 users (DB latency); 100 users counting pool wait.
- **Capacity at p99 ≤ 250 ms**: 100 users (DB latency); 100 users counting pool wait.
- **Capacity at p99 ≤ 1000 ms**: 100 users (DB latency); 100 users counting pool wait.
- **Ops per server core-second**: 100→3896.
- **Read-your-writes violations**: none.
- **Business invariants**: all stages ok.
- **Offline integrity check** (elitesql check): ok in 7.99 s.
  - right after killing the server (no clean close): exit 3, 0 errors, 273000 warnings; after one clean open/close: exit 0, 0 errors, 0 warnings.
- **Database size**: 84.8 MiB after the first stage, 84.8 MiB after the last; 14096 paid orders in total.

## p99 latency per operation (ms)

| op | share % | 100 |
|---|---:|---:|
| add_to_cart | 10.12 | 41.05 |
| admin_dashboard | 1.03 | 61.58 |
| browse | 20.38 | 11.19 |
| checkout | 4.08 | 197.53 |
| order_history | 3.02 | 19.70 |
| product_detail | 17.24 | 18.36 |
| recommend | 7.15 | 18.13 |
| relogin | 1.55 | 42.16 |
| restock | 0.3 | 114.18 |
| search_text | 8.18 | 11.67 |
| session_check | 12.18 | 20.28 |
| signup | 0.51 | 34.41 |
| update_cart_item | 3.02 | 28.58 |
| update_profile | 1.03 | 17.32 |
| view_cart | 8.2 | 17.47 |
| write_review | 2.03 | 24.71 |

## Errors and business outcomes at the highest level

```json
{
 "statuses": {
  "ok": 334159,
  "empty_cart": 4084,
  "conflict_exhausted": 109,
  "not_found": 21
 },
 "errors": {
  "conflict_exhausted": {
   "count": 133,
   "first": "[elitesql:9] conflict, retry transaction: products/b8 changed after this transaction began",
   "ops": {
    "checkout": 131,
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
