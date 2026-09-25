# Mini-SaaS concurrency simulation — sidecar

Run: `B-2` on 2026-09-23T13:59:32 · commit `92e0290` · macOS-26.6.2-arm64-arm-64bit · 10 CPUs

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
| 100 | 100 | 23553.4 | 18221.2 | 5332.2 | 0.876 | 15.083 | 26.147 | 67.531 | 1046.449 | 26.148 | 99.988 | 0.71 | 5.87 | 188.4 | 2.6 | 0.088 | ok |

## Insights

- **Peak throughput**: 23553.4 ops/s at 100 users (p99 26.147 ms).
- **Capacity at p99 ≤ 10 ms**: 0 users (DB latency); 0 users counting pool wait.
- **Capacity at p99 ≤ 50 ms**: 100 users (DB latency); 100 users counting pool wait.
- **Capacity at p99 ≤ 100 ms**: 100 users (DB latency); 100 users counting pool wait.
- **Capacity at p99 ≤ 250 ms**: 100 users (DB latency); 100 users counting pool wait.
- **Capacity at p99 ≤ 1000 ms**: 100 users (DB latency); 100 users counting pool wait.
- **Ops per server core-second**: 100→4013.
- **Read-your-writes violations**: none.
- **Business invariants**: all stages ok.
- **Offline integrity check** (elitesql check): ok in 8.27 s.
  - right after killing the server (no clean close): exit 3, 0 errors, 281752 warnings; after one clean open/close: exit 0, 0 errors, 0 warnings.
- **Database size**: 87.4 MiB after the first stage, 87.4 MiB after the last; 14706 paid orders in total.

## p99 latency per operation (ms)

| op | share % | 100 |
|---|---:|---:|
| add_to_cart | 10.13 | 31.60 |
| admin_dashboard | 1.03 | 105.42 |
| browse | 20.37 | 6.09 |
| checkout | 4.07 | 83.79 |
| order_history | 3.01 | 13.52 |
| product_detail | 17.23 | 10.15 |
| recommend | 7.15 | 9.52 |
| relogin | 1.54 | 42.03 |
| restock | 0.3 | 81.16 |
| search_text | 8.19 | 8.78 |
| session_check | 12.18 | 23.33 |
| signup | 0.51 | 25.56 |
| update_cart_item | 3.03 | 24.87 |
| update_profile | 1.03 | 20.08 |
| view_cart | 8.2 | 11.24 |
| write_review | 2.04 | 28.18 |

## Errors and business outcomes at the highest level

```json
{
 "statuses": {
  "ok": 348978,
  "empty_cart": 4261,
  "not_found": 20,
  "conflict_exhausted": 42
 },
 "errors": {
  "conflict_exhausted": {
   "count": 70,
   "first": "[elitesql:9] conflict, retry transaction: products/c18 changed after this transaction began",
   "ops": {
    "checkout": 67,
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
