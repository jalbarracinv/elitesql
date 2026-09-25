# Mini-SaaS concurrency simulation — sidecar

Run: `B-1` on 2026-09-23T13:58:09 · commit `92e0290` · macOS-26.6.2-arm64-arm-64bit · 10 CPUs

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
- **seed time**: 1.11 s, 13.1 MiB after seeding

## Capacity and performance curve

Latencies are of the database call (service operation) in milliseconds; `user p99` adds the wait for a pooled connection.

| users | conns | ops/s | reads/s | writes/s | p50 | p95 | p99 | p99.9 | max | user p99 | success % | conflict retry % | srv CPU | srv RSS MiB | gen CPU | thr CV | invariants |
|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|
| 100 | 100 | 23557.1 | 18231.2 | 5325.9 | 0.889 | 14.905 | 25.932 | 71.235 | 1022.804 | 25.933 | 99.977 | 0.707 | 6.03 | 186.0 | 2.68 | 0.076 | ok |

## Insights

- **Peak throughput**: 23557.1 ops/s at 100 users (p99 25.932 ms).
- **Capacity at p99 ≤ 10 ms**: 0 users (DB latency); 0 users counting pool wait.
- **Capacity at p99 ≤ 50 ms**: 100 users (DB latency); 100 users counting pool wait.
- **Capacity at p99 ≤ 100 ms**: 100 users (DB latency); 100 users counting pool wait.
- **Capacity at p99 ≤ 250 ms**: 100 users (DB latency); 100 users counting pool wait.
- **Capacity at p99 ≤ 1000 ms**: 100 users (DB latency); 100 users counting pool wait.
- **Ops per server core-second**: 100→3907.
- **Read-your-writes violations**: [(100, 3)].
- **Business invariants**: all stages ok.
- **Offline integrity check** (elitesql check): ok in 8.21 s.
  - right after killing the server (no clean close): exit 3, 0 errors, 283576 warnings; after one clean open/close: exit 0, 0 errors, 0 warnings.
- **Database size**: 87.9 MiB after the first stage, 87.9 MiB after the last; 14821 paid orders in total.

## p99 latency per operation (ms)

| op | share % | 100 |
|---|---:|---:|
| add_to_cart | 10.1 | 30.28 |
| admin_dashboard | 1.02 | 101.86 |
| browse | 20.38 | 5.97 |
| checkout | 4.08 | 106.10 |
| order_history | 3.01 | 15.77 |
| product_detail | 17.24 | 10.09 |
| recommend | 7.15 | 10.16 |
| relogin | 1.54 | 41.90 |
| restock | 0.3 | 84.23 |
| search_text | 8.19 | 8.52 |
| session_check | 12.19 | 23.15 |
| signup | 0.5 | 27.03 |
| update_cart_item | 3.03 | 24.15 |
| update_profile | 1.03 | 20.83 |
| view_cart | 8.21 | 11.34 |
| write_review | 2.04 | 23.01 |

## Errors and business outcomes at the highest level

```json
{
 "statuses": {
  "ok": 348994,
  "empty_cart": 4256,
  "conflict_exhausted": 82,
  "not_found": 24
 },
 "errors": {
  "conflict_exhausted": {
   "count": 113,
   "first": "[elitesql:9] conflict, retry transaction: products/c18 changed after this transaction began",
   "ops": {
    "checkout": 110,
    "restock": 3
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
