# Mini-SaaS concurrency simulation — sidecar

Run: `B-2` on 2026-09-23T13:56:37 · commit `92e0290` · macOS-26.6.2-arm64-arm-64bit · 10 CPUs

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
| 100 | 100 | 23918.7 | 18508.3 | 5410.4 | 0.815 | 14.791 | 26.044 | 62.709 | 1041.655 | 26.045 | 99.987 | 0.71 | 6.02 | 186.6 | 2.63 | 0.082 | ok |

## Insights

- **Peak throughput**: 23918.7 ops/s at 100 users (p99 26.044 ms).
- **Capacity at p99 ≤ 10 ms**: 0 users (DB latency); 0 users counting pool wait.
- **Capacity at p99 ≤ 50 ms**: 100 users (DB latency); 100 users counting pool wait.
- **Capacity at p99 ≤ 100 ms**: 100 users (DB latency); 100 users counting pool wait.
- **Capacity at p99 ≤ 250 ms**: 100 users (DB latency); 100 users counting pool wait.
- **Capacity at p99 ≤ 1000 ms**: 100 users (DB latency); 100 users counting pool wait.
- **Ops per server core-second**: 100→3973.
- **Read-your-writes violations**: none.
- **Business invariants**: all stages ok.
- **Offline integrity check** (elitesql check): ok in 8.42 s.
  - right after killing the server (no clean close): exit 3, 0 errors, 286664 warnings; after one clean open/close: exit 0, 0 errors, 0 warnings.
- **Database size**: 88.9 MiB after the first stage, 88.9 MiB after the last; 15053 paid orders in total.

## p99 latency per operation (ms)

| op | share % | 100 |
|---|---:|---:|
| add_to_cart | 10.1 | 30.51 |
| admin_dashboard | 1.03 | 98.22 |
| browse | 20.39 | 6.02 |
| checkout | 4.07 | 72.83 |
| order_history | 3.01 | 15.03 |
| product_detail | 17.25 | 10.35 |
| recommend | 7.15 | 9.53 |
| relogin | 1.54 | 39.35 |
| restock | 0.3 | 84.09 |
| search_text | 8.18 | 8.84 |
| session_check | 12.18 | 23.55 |
| signup | 0.51 | 27.25 |
| update_cart_item | 3.03 | 24.55 |
| update_profile | 1.03 | 20.24 |
| view_cart | 8.19 | 11.15 |
| write_review | 2.04 | 24.40 |

## Errors and business outcomes at the highest level

```json
{
 "statuses": {
  "ok": 354391,
  "empty_cart": 4320,
  "not_found": 23,
  "conflict_exhausted": 46
 },
 "errors": {
  "conflict_exhausted": {
   "count": 64,
   "first": "[elitesql:9] conflict, retry transaction: products/b7 changed after this transaction began",
   "ops": {
    "checkout": 60,
    "restock": 4
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
