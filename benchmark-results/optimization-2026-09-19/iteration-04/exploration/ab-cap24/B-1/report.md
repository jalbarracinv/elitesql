# Mini-SaaS concurrency simulation — sidecar

Run: `B-1` on 2026-09-23T14:13:15 · commit `92e0290` · macOS-26.6.2-arm64-arm-64bit · 10 CPUs

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
| 100 | 100 | 23782.7 | 18409.0 | 5373.7 | 0.949 | 14.782 | 25.999 | 65.822 | 950.665 | 26.0 | 99.987 | 0.714 | 6.08 | 197.6 | 2.58 | 0.078 | ok |

## Insights

- **Peak throughput**: 23782.7 ops/s at 100 users (p99 25.999 ms).
- **Capacity at p99 ≤ 10 ms**: 0 users (DB latency); 0 users counting pool wait.
- **Capacity at p99 ≤ 50 ms**: 100 users (DB latency); 100 users counting pool wait.
- **Capacity at p99 ≤ 100 ms**: 100 users (DB latency); 100 users counting pool wait.
- **Capacity at p99 ≤ 250 ms**: 100 users (DB latency); 100 users counting pool wait.
- **Capacity at p99 ≤ 1000 ms**: 100 users (DB latency); 100 users counting pool wait.
- **Ops per server core-second**: 100→3912.
- **Read-your-writes violations**: none.
- **Business invariants**: all stages ok.
- **Offline integrity check** (elitesql check): ok in 8.35 s.
  - right after killing the server (no clean close): exit 3, 0 errors, 285220 warnings; after one clean open/close: exit 0, 0 errors, 0 warnings.
- **Database size**: 88.5 MiB after the first stage, 88.5 MiB after the last; 14966 paid orders in total.

## p99 latency per operation (ms)

| op | share % | 100 |
|---|---:|---:|
| add_to_cart | 10.1 | 31.98 |
| admin_dashboard | 1.03 | 89.81 |
| browse | 20.42 | 7.41 |
| checkout | 4.07 | 78.43 |
| order_history | 3.01 | 14.07 |
| product_detail | 17.25 | 12.30 |
| recommend | 7.15 | 12.34 |
| relogin | 1.53 | 40.96 |
| restock | 0.3 | 84.01 |
| search_text | 8.18 | 8.97 |
| session_check | 12.18 | 22.43 |
| signup | 0.51 | 28.19 |
| update_cart_item | 3.03 | 25.49 |
| update_profile | 1.02 | 20.00 |
| view_cart | 8.18 | 11.95 |
| write_review | 2.04 | 25.95 |

## Errors and business outcomes at the highest level

```json
{
 "statuses": {
  "ok": 352393,
  "empty_cart": 4281,
  "conflict_exhausted": 46,
  "not_found": 21
 },
 "errors": {
  "conflict_exhausted": {
   "count": 72,
   "first": "[elitesql:9] conflict, retry transaction: products/c15 changed after this transaction began",
   "ops": {
    "checkout": 68,
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
