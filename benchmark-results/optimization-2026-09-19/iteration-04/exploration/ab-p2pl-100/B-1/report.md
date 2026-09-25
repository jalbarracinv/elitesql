# Mini-SaaS concurrency simulation — sidecar

Run: `B-1` on 2026-09-23T13:12:07 · commit `92e0290` · macOS-26.6.2-arm64-arm-64bit · 10 CPUs

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
| 100 | 100 | 9458.1 | 7324.6 | 2133.5 | 5.424 | 33.986 | 55.999 | 118.438 | 1235.245 | 56.002 | 99.982 | 0.775 | 6.27 | 114.9 | 1.26 | 0.111 | ok |

## Insights

- **Peak throughput**: 9458.1 ops/s at 100 users (p99 55.999 ms).
- **Capacity at p99 ≤ 10 ms**: 0 users (DB latency); 0 users counting pool wait.
- **Capacity at p99 ≤ 50 ms**: 0 users (DB latency); 0 users counting pool wait.
- **Capacity at p99 ≤ 100 ms**: 100 users (DB latency); 100 users counting pool wait.
- **Capacity at p99 ≤ 250 ms**: 100 users (DB latency); 100 users counting pool wait.
- **Capacity at p99 ≤ 1000 ms**: 100 users (DB latency); 100 users counting pool wait.
- **Ops per server core-second**: 100→1508.
- **Read-your-writes violations**: none.
- **Business invariants**: all stages ok.
- **Offline integrity check** (elitesql check): ok in 4.85 s.
  - right after killing the server (no clean close): exit 3, 0 errors, 151962 warnings; after one clean open/close: exit 0, 0 errors, 0 warnings.
- **Database size**: 43.3 MiB after the first stage, 43.3 MiB after the last; 6053 paid orders in total.

## p99 latency per operation (ms)

| op | share % | 100 |
|---|---:|---:|
| add_to_cart | 10.1 | 56.26 |
| admin_dashboard | 1.05 | 96.80 |
| browse | 20.49 | 55.06 |
| checkout | 4.11 | 185.28 |
| order_history | 3.06 | 24.00 |
| product_detail | 17.25 | 21.57 |
| recommend | 7.17 | 20.53 |
| relogin | 1.56 | 74.34 |
| restock | 0.32 | 174.22 |
| search_text | 8.15 | 10.82 |
| session_check | 12.17 | 46.51 |
| signup | 0.49 | 47.73 |
| update_cart_item | 2.94 | 49.61 |
| update_profile | 1.03 | 44.43 |
| view_cart | 8.11 | 20.57 |
| write_review | 2.02 | 44.10 |

## Errors and business outcomes at the highest level

```json
{
 "statuses": {
  "ok": 140082,
  "empty_cart": 1753,
  "not_found": 10,
  "conflict_exhausted": 26
 },
 "errors": {
  "conflict_exhausted": {
   "count": 29,
   "first": "[elitesql:9] conflict, retry transaction: products/c15 changed after this transaction began",
   "ops": {
    "checkout": 26,
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
