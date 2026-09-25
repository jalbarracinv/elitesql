# Mini-SaaS concurrency simulation — sidecar

Run: `B-1` on 2026-09-23T14:50:19 · commit `92e0290` · macOS-26.6.2-arm64-arm-64bit · 10 CPUs

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
| 100 | 100 | 21847.3 | 16897.1 | 4950.2 | 1.374 | 16.471 | 26.052 | 85.741 | 1291.642 | 26.052 | 99.972 | 0.778 | 6.24 | 182.0 | 2.67 | 0.16 | ok |

## Insights

- **Peak throughput**: 21847.3 ops/s at 100 users (p99 26.052 ms).
- **Capacity at p99 ≤ 10 ms**: 0 users (DB latency); 0 users counting pool wait.
- **Capacity at p99 ≤ 50 ms**: 100 users (DB latency); 100 users counting pool wait.
- **Capacity at p99 ≤ 100 ms**: 100 users (DB latency); 100 users counting pool wait.
- **Capacity at p99 ≤ 250 ms**: 100 users (DB latency); 100 users counting pool wait.
- **Capacity at p99 ≤ 1000 ms**: 100 users (DB latency); 100 users counting pool wait.
- **Ops per server core-second**: 100→3501.
- **Read-your-writes violations**: none.
- **Business invariants**: all stages ok.
- **Offline integrity check** (elitesql check): ok in 7.95 s.
  - right after killing the server (no clean close): exit 3, 0 errors, 270454 warnings; after one clean open/close: exit 0, 0 errors, 0 warnings.
- **Database size**: 84.0 MiB after the first stage, 84.0 MiB after the last; 13903 paid orders in total.

## p99 latency per operation (ms)

| op | share % | 100 |
|---|---:|---:|
| add_to_cart | 10.14 | 27.33 |
| admin_dashboard | 1.03 | 50.29 |
| browse | 20.4 | 5.11 |
| checkout | 4.06 | 130.63 |
| order_history | 3.0 | 8.82 |
| product_detail | 17.22 | 7.78 |
| recommend | 7.15 | 7.40 |
| relogin | 1.54 | 36.45 |
| restock | 0.3 | 78.20 |
| search_text | 8.2 | 5.16 |
| session_check | 12.17 | 23.21 |
| signup | 0.52 | 25.17 |
| update_cart_item | 3.03 | 23.09 |
| update_profile | 1.03 | 22.41 |
| view_cart | 8.18 | 7.88 |
| write_review | 2.03 | 20.50 |

## Errors and business outcomes at the highest level

```json
{
 "statuses": {
  "ok": 323669,
  "empty_cart": 3930,
  "conflict_exhausted": 91,
  "not_found": 20
 },
 "errors": {
  "conflict_exhausted": {
   "count": 126,
   "first": "[elitesql:9] conflict, retry transaction: products/c13 changed after this transaction began",
   "ops": {
    "checkout": 122,
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
