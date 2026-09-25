# Mini-SaaS concurrency simulation — sidecar

Run: `profile-p11` on 2026-09-23T14:59:03 · commit `92e0290` · macOS-26.6.2-arm64-arm-64bit · 10 CPUs

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
| 100 | 100 | 19895.9 | 15398.1 | 4497.8 | 1.209 | 18.269 | 30.478 | 84.567 | 1365.952 | 30.479 | 99.982 | 0.743 | 4.89 | 171.2 | 2.22 | 0.225 | ok |

## Insights

- **Peak throughput**: 19895.9 ops/s at 100 users (p99 30.478 ms).
- **Capacity at p99 ≤ 10 ms**: 0 users (DB latency); 0 users counting pool wait.
- **Capacity at p99 ≤ 50 ms**: 100 users (DB latency); 100 users counting pool wait.
- **Capacity at p99 ≤ 100 ms**: 100 users (DB latency); 100 users counting pool wait.
- **Capacity at p99 ≤ 250 ms**: 100 users (DB latency); 100 users counting pool wait.
- **Capacity at p99 ≤ 1000 ms**: 100 users (DB latency); 100 users counting pool wait.
- **Ops per server core-second**: 100→4069.
- **Read-your-writes violations**: none.
- **Business invariants**: all stages ok.
- **Offline integrity check** (elitesql check): ok in 7.44 s.
  - right after killing the server (no clean close): exit 3, 0 errors, 255636 warnings; after one clean open/close: exit 0, 0 errors, 0 warnings.
- **Database size**: 78.6 MiB after the first stage, 78.6 MiB after the last; 12943 paid orders in total.

## p99 latency per operation (ms)

| op | share % | 100 |
|---|---:|---:|
| add_to_cart | 10.09 | 35.08 |
| admin_dashboard | 1.03 | 138.11 |
| browse | 20.4 | 6.88 |
| checkout | 4.07 | 109.08 |
| order_history | 3.03 | 16.52 |
| product_detail | 17.26 | 10.45 |
| recommend | 7.12 | 9.81 |
| relogin | 1.55 | 48.09 |
| restock | 0.3 | 115.34 |
| search_text | 8.17 | 10.64 |
| session_check | 12.18 | 26.69 |
| signup | 0.51 | 31.91 |
| update_cart_item | 3.03 | 29.44 |
| update_profile | 1.03 | 23.96 |
| view_cart | 8.21 | 12.31 |
| write_review | 2.03 | 29.64 |

## Errors and business outcomes at the highest level

```json
{
 "statuses": {
  "ok": 294710,
  "empty_cart": 3658,
  "conflict_exhausted": 53,
  "not_found": 18
 },
 "errors": {
  "conflict_exhausted": {
   "count": 68,
   "first": "[elitesql:9] conflict, retry transaction: products/c31 changed after this transaction began",
   "ops": {
    "checkout": 65,
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
