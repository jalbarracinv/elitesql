# Mini-SaaS concurrency simulation — sidecar

Run: `A-2` on 2026-09-23T15:03:33 · commit `92e0290` · macOS-26.6.2-arm64-arm-64bit · 10 CPUs

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
| 100 | 100 | 23200.0 | 17953.7 | 5246.3 | 0.852 | 15.184 | 27.058 | 72.814 | 1031.621 | 27.059 | 99.986 | 0.705 | 5.98 | 191.6 | 2.59 | 0.084 | ok |

## Insights

- **Peak throughput**: 23200.0 ops/s at 100 users (p99 27.058 ms).
- **Capacity at p99 ≤ 10 ms**: 0 users (DB latency); 0 users counting pool wait.
- **Capacity at p99 ≤ 50 ms**: 100 users (DB latency); 100 users counting pool wait.
- **Capacity at p99 ≤ 100 ms**: 100 users (DB latency); 100 users counting pool wait.
- **Capacity at p99 ≤ 250 ms**: 100 users (DB latency); 100 users counting pool wait.
- **Capacity at p99 ≤ 1000 ms**: 100 users (DB latency); 100 users counting pool wait.
- **Ops per server core-second**: 100→3880.
- **Read-your-writes violations**: [(100, 11)].
- **Business invariants**: all stages ok.
- **Offline integrity check** (elitesql check): ok in 8.26 s.
  - right after killing the server (no clean close): exit 3, 0 errors, 280710 warnings; after one clean open/close: exit 0, 0 errors, 0 warnings.
- **Database size**: 87.1 MiB after the first stage, 87.1 MiB after the last; 14637 paid orders in total.

## p99 latency per operation (ms)

| op | share % | 100 |
|---|---:|---:|
| add_to_cart | 10.12 | 34.27 |
| admin_dashboard | 1.02 | 118.47 |
| browse | 20.4 | 6.68 |
| checkout | 4.07 | 87.39 |
| order_history | 3.01 | 15.44 |
| product_detail | 17.23 | 11.40 |
| recommend | 7.16 | 10.50 |
| relogin | 1.54 | 43.24 |
| restock | 0.3 | 108.50 |
| search_text | 8.19 | 9.52 |
| session_check | 12.18 | 24.69 |
| signup | 0.5 | 25.52 |
| update_cart_item | 3.02 | 26.50 |
| update_profile | 1.03 | 20.20 |
| view_cart | 8.19 | 11.36 |
| write_review | 2.03 | 27.34 |

## Errors and business outcomes at the highest level

```json
{
 "statuses": {
  "ok": 343732,
  "empty_cart": 4197,
  "not_found": 22,
  "conflict_exhausted": 49
 },
 "errors": {
  "conflict_exhausted": {
   "count": 72,
   "first": "[elitesql:9] conflict, retry transaction: products/c53 changed after this transaction began",
   "ops": {
    "checkout": 69,
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
