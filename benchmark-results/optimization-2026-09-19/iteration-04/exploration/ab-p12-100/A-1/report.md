# Mini-SaaS concurrency simulation — sidecar

Run: `A-1` on 2026-09-23T15:02:09 · commit `92e0290` · macOS-26.6.2-arm64-arm-64bit · 10 CPUs

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
- **seed time**: 1.09 s, 13.1 MiB after seeding

## Capacity and performance curve

Latencies are of the database call (service operation) in milliseconds; `user p99` adds the wait for a pooled connection.

| users | conns | ops/s | reads/s | writes/s | p50 | p95 | p99 | p99.9 | max | user p99 | success % | conflict retry % | srv CPU | srv RSS MiB | gen CPU | thr CV | invariants |
|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|
| 100 | 100 | 22944.4 | 17754.0 | 5190.4 | 0.896 | 15.343 | 26.445 | 70.914 | 1019.561 | 26.446 | 99.979 | 0.707 | 5.88 | 186.5 | 2.63 | 0.075 | ok |

## Insights

- **Peak throughput**: 22944.4 ops/s at 100 users (p99 26.445 ms).
- **Capacity at p99 ≤ 10 ms**: 0 users (DB latency); 0 users counting pool wait.
- **Capacity at p99 ≤ 50 ms**: 100 users (DB latency); 100 users counting pool wait.
- **Capacity at p99 ≤ 100 ms**: 100 users (DB latency); 100 users counting pool wait.
- **Capacity at p99 ≤ 250 ms**: 100 users (DB latency); 100 users counting pool wait.
- **Capacity at p99 ≤ 1000 ms**: 100 users (DB latency); 100 users counting pool wait.
- **Ops per server core-second**: 100→3902.
- **Read-your-writes violations**: [(100, 11)].
- **Business invariants**: all stages ok.
- **Offline integrity check** (elitesql check): ok in 8.29 s.
  - right after killing the server (no clean close): exit 3, 0 errors, 280156 warnings; after one clean open/close: exit 0, 0 errors, 0 warnings.
- **Database size**: 87.0 MiB after the first stage, 87.0 MiB after the last; 14604 paid orders in total.

## p99 latency per operation (ms)

| op | share % | 100 |
|---|---:|---:|
| add_to_cart | 10.12 | 31.75 |
| admin_dashboard | 1.03 | 104.54 |
| browse | 20.41 | 6.68 |
| checkout | 4.07 | 95.80 |
| order_history | 3.02 | 15.92 |
| product_detail | 17.23 | 11.06 |
| recommend | 7.16 | 9.46 |
| relogin | 1.55 | 42.96 |
| restock | 0.3 | 89.02 |
| search_text | 8.19 | 9.67 |
| session_check | 12.17 | 23.67 |
| signup | 0.5 | 26.49 |
| update_cart_item | 3.03 | 24.83 |
| update_profile | 1.03 | 20.20 |
| view_cart | 8.19 | 12.10 |
| write_review | 2.04 | 27.32 |

## Errors and business outcomes at the highest level

```json
{
 "statuses": {
  "ok": 339937,
  "empty_cart": 4137,
  "not_found": 21,
  "conflict_exhausted": 71
 },
 "errors": {
  "conflict_exhausted": {
   "count": 93,
   "first": "[elitesql:9] conflict, retry transaction: products/c11 changed after this transaction began",
   "ops": {
    "checkout": 90,
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
