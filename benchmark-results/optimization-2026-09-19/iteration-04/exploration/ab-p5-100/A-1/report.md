# Mini-SaaS concurrency simulation — sidecar

Run: `A-1` on 2026-09-23T13:49:06 · commit `92e0290` · macOS-26.6.2-arm64-arm-64bit · 10 CPUs

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
- **seed time**: 1.17 s, 13.1 MiB after seeding

## Capacity and performance curve

Latencies are of the database call (service operation) in milliseconds; `user p99` adds the wait for a pooled connection.

| users | conns | ops/s | reads/s | writes/s | p50 | p95 | p99 | p99.9 | max | user p99 | success % | conflict retry % | srv CPU | srv RSS MiB | gen CPU | thr CV | invariants |
|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|
| 100 | 100 | 18443.7 | 14266.5 | 4177.2 | 1.236 | 19.553 | 39.661 | 119.411 | 1673.59 | 39.663 | 99.983 | 0.718 | 5.06 | 164.3 | 2.41 | 0.174 | ok |

## Insights

- **Peak throughput**: 18443.7 ops/s at 100 users (p99 39.661 ms).
- **Capacity at p99 ≤ 10 ms**: 0 users (DB latency); 0 users counting pool wait.
- **Capacity at p99 ≤ 50 ms**: 100 users (DB latency); 100 users counting pool wait.
- **Capacity at p99 ≤ 100 ms**: 100 users (DB latency); 100 users counting pool wait.
- **Capacity at p99 ≤ 250 ms**: 100 users (DB latency); 100 users counting pool wait.
- **Capacity at p99 ≤ 1000 ms**: 100 users (DB latency); 100 users counting pool wait.
- **Ops per server core-second**: 100→3645.
- **Read-your-writes violations**: none.
- **Business invariants**: all stages ok.
- **Offline integrity check** (elitesql check): ok in 7.44 s.
  - right after killing the server (no clean close): exit 3, 0 errors, 237490 warnings; after one clean open/close: exit 0, 0 errors, 0 warnings.
- **Database size**: 73.0 MiB after the first stage, 73.0 MiB after the last; 11760 paid orders in total.

## p99 latency per operation (ms)

| op | share % | 100 |
|---|---:|---:|
| add_to_cart | 10.11 | 46.64 |
| admin_dashboard | 1.04 | 131.00 |
| browse | 20.45 | 12.48 |
| checkout | 4.1 | 148.52 |
| order_history | 3.04 | 26.50 |
| product_detail | 17.18 | 26.38 |
| recommend | 7.12 | 26.12 |
| relogin | 1.54 | 55.98 |
| restock | 0.31 | 93.03 |
| search_text | 8.15 | 14.30 |
| session_check | 12.19 | 37.97 |
| signup | 0.51 | 42.35 |
| update_cart_item | 3.03 | 40.40 |
| update_profile | 1.03 | 31.96 |
| view_cart | 8.19 | 19.89 |
| write_review | 2.03 | 34.09 |

## Errors and business outcomes at the highest level

```json
{
 "statuses": {
  "ok": 273161,
  "empty_cart": 3428,
  "not_found": 19,
  "conflict_exhausted": 48
 },
 "errors": {
  "conflict_exhausted": {
   "count": 62,
   "first": "[elitesql:9] conflict, retry transaction: products/b2 changed after this transaction began",
   "ops": {
    "checkout": 61,
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
