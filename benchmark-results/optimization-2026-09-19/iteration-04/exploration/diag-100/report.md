# Mini-SaaS concurrency simulation — sidecar

Run: `diag-100` on 2026-09-23T14:26:35 · commit `92e0290` · macOS-26.6.2-arm64-arm-64bit · 10 CPUs

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
| 100 | 100 | 23495.5 | 18180.3 | 5315.2 | 0.88 | 14.906 | 26.085 | 68.948 | 1009.663 | 26.086 | 99.981 | 0.716 | 6.13 | 189.9 | 2.6 | 0.079 | ok |

## Insights

- **Peak throughput**: 23495.5 ops/s at 100 users (p99 26.085 ms).
- **Capacity at p99 ≤ 10 ms**: 0 users (DB latency); 0 users counting pool wait.
- **Capacity at p99 ≤ 50 ms**: 100 users (DB latency); 100 users counting pool wait.
- **Capacity at p99 ≤ 100 ms**: 100 users (DB latency); 100 users counting pool wait.
- **Capacity at p99 ≤ 250 ms**: 100 users (DB latency); 100 users counting pool wait.
- **Capacity at p99 ≤ 1000 ms**: 100 users (DB latency); 100 users counting pool wait.
- **Ops per server core-second**: 100→3833.
- **Read-your-writes violations**: [(100, 11)].
- **Business invariants**: all stages ok.
- **Offline integrity check** (elitesql check): ok in 8.32 s.
  - right after killing the server (no clean close): exit 3, 0 errors, 283796 warnings; after one clean open/close: exit 0, 0 errors, 0 warnings.
- **Database size**: 88.0 MiB after the first stage, 88.0 MiB after the last; 14866 paid orders in total.

## p99 latency per operation (ms)

| op | share % | 100 |
|---|---:|---:|
| add_to_cart | 10.13 | 30.84 |
| admin_dashboard | 1.03 | 100.76 |
| browse | 20.41 | 5.96 |
| checkout | 4.07 | 91.39 |
| order_history | 3.02 | 15.57 |
| product_detail | 17.23 | 10.67 |
| recommend | 7.16 | 9.52 |
| relogin | 1.54 | 38.97 |
| restock | 0.3 | 100.49 |
| search_text | 8.18 | 8.95 |
| session_check | 12.17 | 22.88 |
| signup | 0.51 | 27.02 |
| update_cart_item | 3.02 | 23.15 |
| update_profile | 1.02 | 20.12 |
| view_cart | 8.19 | 12.87 |
| write_review | 2.04 | 23.88 |

## Errors and business outcomes at the highest level

```json
{
 "statuses": {
  "ok": 348125,
  "empty_cart": 4220,
  "conflict_exhausted": 66,
  "not_found": 22
 },
 "errors": {
  "conflict_exhausted": {
   "count": 92,
   "first": "[elitesql:9] conflict, retry transaction: products/c13 changed after this transaction began",
   "ops": {
    "checkout": 86,
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
