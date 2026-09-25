# Mini-SaaS concurrency simulation — sidecar

Run: `B-1` on 2026-09-23T13:07:47 · commit `92e0290` · macOS-26.6.2-arm64-arm-64bit · 10 CPUs

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
- **seed time**: 1.16 s, 13.1 MiB after seeding

## Capacity and performance curve

Latencies are of the database call (service operation) in milliseconds; `user p99` adds the wait for a pooled connection.

| users | conns | ops/s | reads/s | writes/s | p50 | p95 | p99 | p99.9 | max | user p99 | success % | conflict retry % | srv CPU | srv RSS MiB | gen CPU | thr CV | invariants |
|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|
| 100 | 100 | 17224.3 | 13325.7 | 3898.7 | 1.463 | 20.961 | 43.663 | 109.016 | 1093.654 | 43.665 | 99.981 | 0.701 | 5.32 | 158.2 | 2.28 | 0.068 | ok |

## Insights

- **Peak throughput**: 17224.3 ops/s at 100 users (p99 43.663 ms).
- **Capacity at p99 ≤ 10 ms**: 0 users (DB latency); 0 users counting pool wait.
- **Capacity at p99 ≤ 50 ms**: 100 users (DB latency); 100 users counting pool wait.
- **Capacity at p99 ≤ 100 ms**: 100 users (DB latency); 100 users counting pool wait.
- **Capacity at p99 ≤ 250 ms**: 100 users (DB latency); 100 users counting pool wait.
- **Capacity at p99 ≤ 1000 ms**: 100 users (DB latency); 100 users counting pool wait.
- **Ops per server core-second**: 100→3238.
- **Read-your-writes violations**: none.
- **Business invariants**: all stages ok.
- **Offline integrity check** (elitesql check): ok in 7.75 s.
  - right after killing the server (no clean close): exit 3, 0 errors, 223506 warnings; after one clean open/close: exit 0, 0 errors, 0 warnings.
- **Database size**: 67.6 MiB after the first stage, 67.6 MiB after the last; 10822 paid orders in total.

## p99 latency per operation (ms)

| op | share % | 100 |
|---|---:|---:|
| add_to_cart | 10.1 | 48.19 |
| admin_dashboard | 1.04 | 139.15 |
| browse | 20.49 | 30.17 |
| checkout | 4.1 | 127.56 |
| order_history | 3.05 | 34.72 |
| product_detail | 17.2 | 24.07 |
| recommend | 7.13 | 20.92 |
| relogin | 1.53 | 59.67 |
| restock | 0.31 | 100.68 |
| search_text | 8.13 | 19.30 |
| session_check | 12.15 | 42.98 |
| signup | 0.51 | 48.61 |
| update_cart_item | 3.04 | 38.27 |
| update_profile | 1.01 | 30.97 |
| view_cart | 8.19 | 26.59 |
| write_review | 2.04 | 38.88 |

## Errors and business outcomes at the highest level

```json
{
 "statuses": {
  "ok": 255119,
  "empty_cart": 3179,
  "not_found": 18,
  "conflict_exhausted": 49
 },
 "errors": {
  "conflict_exhausted": {
   "count": 64,
   "first": "[elitesql:9] conflict, retry transaction: products/b4 changed after this transaction began",
   "ops": {
    "checkout": 63,
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
