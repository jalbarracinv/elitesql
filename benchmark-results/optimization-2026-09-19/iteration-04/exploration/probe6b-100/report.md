# Mini-SaaS concurrency simulation — sidecar

Run: `probe6b-100` on 2026-09-23T14:09:53 · commit `92e0290` · macOS-26.6.2-arm64-arm-64bit · 10 CPUs

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
| 100 | 100 | 22814.7 | 17653.5 | 5161.2 | 0.898 | 14.762 | 26.157 | 82.411 | 945.654 | 26.158 | 99.989 | 0.721 | 5.86 | 192.5 | 2.48 | 0.098 | ok |

## Insights

- **Peak throughput**: 22814.7 ops/s at 100 users (p99 26.157 ms).
- **Capacity at p99 ≤ 10 ms**: 0 users (DB latency); 0 users counting pool wait.
- **Capacity at p99 ≤ 50 ms**: 100 users (DB latency); 100 users counting pool wait.
- **Capacity at p99 ≤ 100 ms**: 100 users (DB latency); 100 users counting pool wait.
- **Capacity at p99 ≤ 250 ms**: 100 users (DB latency); 100 users counting pool wait.
- **Capacity at p99 ≤ 1000 ms**: 100 users (DB latency); 100 users counting pool wait.
- **Ops per server core-second**: 100→3893.
- **Read-your-writes violations**: none.
- **Business invariants**: all stages ok.
- **Offline integrity check** (elitesql check): ok in 8.18 s.
  - right after killing the server (no clean close): exit 3, 0 errors, 277756 warnings; after one clean open/close: exit 0, 0 errors, 0 warnings.
- **Database size**: 86.0 MiB after the first stage, 86.0 MiB after the last; 14458 paid orders in total.

## p99 latency per operation (ms)

| op | share % | 100 |
|---|---:|---:|
| add_to_cart | 10.11 | 31.65 |
| admin_dashboard | 1.03 | 107.37 |
| browse | 20.39 | 5.99 |
| checkout | 4.07 | 83.62 |
| order_history | 3.01 | 13.92 |
| product_detail | 17.22 | 10.56 |
| recommend | 7.16 | 9.19 |
| relogin | 1.54 | 42.21 |
| restock | 0.3 | 117.84 |
| search_text | 8.19 | 9.07 |
| session_check | 12.17 | 24.05 |
| signup | 0.51 | 30.57 |
| update_cart_item | 3.03 | 25.87 |
| update_profile | 1.03 | 20.34 |
| view_cart | 8.2 | 11.14 |
| write_review | 2.03 | 24.86 |

## Errors and business outcomes at the highest level

```json
{
 "statuses": {
  "ok": 338036,
  "empty_cart": 4126,
  "not_found": 20,
  "conflict_exhausted": 38
 },
 "errors": {
  "conflict_exhausted": {
   "count": 58,
   "first": "[elitesql:9] conflict, retry transaction: products/b4 changed after this transaction began",
   "ops": {
    "checkout": 52,
    "restock": 6
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
