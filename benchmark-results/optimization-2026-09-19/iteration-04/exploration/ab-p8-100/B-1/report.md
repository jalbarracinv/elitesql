# Mini-SaaS concurrency simulation — sidecar

Run: `B-1` on 2026-09-23T14:39:34 · commit `92e0290` · macOS-26.6.2-arm64-arm-64bit · 10 CPUs

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
| 100 | 100 | 16941.9 | 13112.7 | 3829.3 | 0.622 | 22.798 | 89.765 | 448.745 | 1084.507 | 89.766 | 99.604 | 1.332 | 5.7 | 162.4 | 3.51 | 0.131 | ok |

## Insights

- **Peak throughput**: 16941.9 ops/s at 100 users (p99 89.765 ms).
- **Capacity at p99 ≤ 10 ms**: 0 users (DB latency); 0 users counting pool wait.
- **Capacity at p99 ≤ 50 ms**: 0 users (DB latency); 0 users counting pool wait.
- **Capacity at p99 ≤ 100 ms**: 100 users (DB latency); 100 users counting pool wait.
- **Capacity at p99 ≤ 250 ms**: 100 users (DB latency); 100 users counting pool wait.
- **Capacity at p99 ≤ 1000 ms**: 100 users (DB latency); 100 users counting pool wait.
- **Ops per server core-second**: 100→2972.
- **Read-your-writes violations**: [(100, 6)].
- **Business invariants**: all stages ok.
- **Offline integrity check** (elitesql check): ok in 6.75 s.
  - right after killing the server (no clean close): exit 3, 0 errors, 230096 warnings; after one clean open/close: exit 0, 0 errors, 0 warnings.
- **Database size**: 69.4 MiB after the first stage, 69.4 MiB after the last; 10782 paid orders in total.

## p99 latency per operation (ms)

| op | share % | 100 |
|---|---:|---:|
| add_to_cart | 10.08 | 32.12 |
| admin_dashboard | 1.03 | 108.81 |
| browse | 20.39 | 6.31 |
| checkout | 4.09 | 546.28 |
| order_history | 3.02 | 15.03 |
| product_detail | 17.18 | 10.93 |
| recommend | 7.16 | 9.92 |
| relogin | 1.54 | 40.89 |
| restock | 0.31 | 415.33 |
| search_text | 8.2 | 8.43 |
| session_check | 12.19 | 22.25 |
| signup | 0.52 | 26.00 |
| update_cart_item | 3.01 | 25.07 |
| update_profile | 1.02 | 18.20 |
| view_cart | 8.22 | 14.57 |
| write_review | 2.04 | 136.40 |

## Errors and business outcomes at the highest level

```json
{
 "statuses": {
  "ok": 250269,
  "empty_cart": 2838,
  "conflict_exhausted": 1006,
  "not_found": 16
 },
 "errors": {
  "conflict_exhausted": {
   "count": 1250,
   "first": "[elitesql:9] conflict, retry transaction: products/b1 changed after this transaction began",
   "ops": {
    "restock": 138,
    "checkout": 1110,
    "write_review": 2
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
