# Mini-SaaS concurrency simulation — sidecar

Run: `B-2` on 2026-09-23T14:33:07 · commit `92e0290` · macOS-26.6.2-arm64-arm-64bit · 10 CPUs

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
| 100 | 100 | 23753.3 | 18381.7 | 5371.5 | 0.864 | 14.732 | 26.156 | 67.899 | 1103.566 | 26.157 | 99.987 | 0.719 | 6.06 | 188.9 | 2.58 | 0.075 | ok |

## Insights

- **Peak throughput**: 23753.3 ops/s at 100 users (p99 26.156 ms).
- **Capacity at p99 ≤ 10 ms**: 0 users (DB latency); 0 users counting pool wait.
- **Capacity at p99 ≤ 50 ms**: 100 users (DB latency); 100 users counting pool wait.
- **Capacity at p99 ≤ 100 ms**: 100 users (DB latency); 100 users counting pool wait.
- **Capacity at p99 ≤ 250 ms**: 100 users (DB latency); 100 users counting pool wait.
- **Capacity at p99 ≤ 1000 ms**: 100 users (DB latency); 100 users counting pool wait.
- **Ops per server core-second**: 100→3920.
- **Read-your-writes violations**: none.
- **Business invariants**: all stages ok.
- **Offline integrity check** (elitesql check): ok in 8.31 s.
  - right after killing the server (no clean close): exit 3, 0 errors, 282586 warnings; after one clean open/close: exit 0, 0 errors, 0 warnings.
- **Database size**: 87.6 MiB after the first stage, 87.6 MiB after the last; 14769 paid orders in total.

## p99 latency per operation (ms)

| op | share % | 100 |
|---|---:|---:|
| add_to_cart | 10.1 | 31.69 |
| admin_dashboard | 1.02 | 100.80 |
| browse | 20.41 | 5.97 |
| checkout | 4.06 | 82.06 |
| order_history | 3.02 | 15.76 |
| product_detail | 17.23 | 10.04 |
| recommend | 7.16 | 8.76 |
| relogin | 1.54 | 41.09 |
| restock | 0.3 | 90.64 |
| search_text | 8.19 | 8.66 |
| session_check | 12.17 | 23.67 |
| signup | 0.51 | 24.27 |
| update_cart_item | 3.04 | 25.36 |
| update_profile | 1.02 | 19.62 |
| view_cart | 8.18 | 10.62 |
| write_review | 2.04 | 24.09 |

## Errors and business outcomes at the highest level

```json
{
 "statuses": {
  "ok": 351932,
  "empty_cart": 4299,
  "not_found": 22,
  "conflict_exhausted": 46
 },
 "errors": {
  "conflict_exhausted": {
   "count": 64,
   "first": "[elitesql:9] conflict, retry transaction: products/c15 changed after this transaction began",
   "ops": {
    "checkout": 59,
    "restock": 5
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
