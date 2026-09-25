# Mini-SaaS concurrency simulation — sidecar

Run: `B-2` on 2026-09-23T13:04:26 · commit `92e0290` · macOS-26.6.2-arm64-arm-64bit · 10 CPUs

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
- **seed time**: 1.18 s, 13.1 MiB after seeding

## Capacity and performance curve

Latencies are of the database call (service operation) in milliseconds; `user p99` adds the wait for a pooled connection.

| users | conns | ops/s | reads/s | writes/s | p50 | p95 | p99 | p99.9 | max | user p99 | success % | conflict retry % | srv CPU | srv RSS MiB | gen CPU | thr CV | invariants |
|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|
| 100 | 100 | 9784.4 | 7577.3 | 2207.1 | 3.624 | 35.916 | 84.054 | 178.979 | 1106.304 | 84.057 | 99.991 | 0.753 | 5.56 | 117.4 | 1.3 | 0.201 | ok |

## Insights

- **Peak throughput**: 9784.4 ops/s at 100 users (p99 84.054 ms).
- **Capacity at p99 ≤ 10 ms**: 0 users (DB latency); 0 users counting pool wait.
- **Capacity at p99 ≤ 50 ms**: 0 users (DB latency); 0 users counting pool wait.
- **Capacity at p99 ≤ 100 ms**: 100 users (DB latency); 100 users counting pool wait.
- **Capacity at p99 ≤ 250 ms**: 100 users (DB latency); 100 users counting pool wait.
- **Capacity at p99 ≤ 1000 ms**: 100 users (DB latency); 100 users counting pool wait.
- **Ops per server core-second**: 100→1760.
- **Read-your-writes violations**: none.
- **Business invariants**: all stages ok.
- **Offline integrity check** (elitesql check): ok in 5.29 s.
  - right after killing the server (no clean close): exit 3, 0 errors, 157930 warnings; after one clean open/close: exit 0, 0 errors, 0 warnings.
- **Database size**: 45.1 MiB after the first stage, 45.1 MiB after the last; 6445 paid orders in total.

## p99 latency per operation (ms)

| op | share % | 100 |
|---|---:|---:|
| add_to_cart | 10.06 | 91.30 |
| admin_dashboard | 1.04 | 167.26 |
| browse | 20.43 | 98.61 |
| checkout | 4.08 | 197.72 |
| order_history | 3.04 | 60.48 |
| product_detail | 17.29 | 37.01 |
| recommend | 7.15 | 26.35 |
| relogin | 1.56 | 129.46 |
| restock | 0.32 | 169.66 |
| search_text | 8.15 | 31.37 |
| session_check | 12.18 | 76.70 |
| signup | 0.49 | 120.17 |
| update_cart_item | 2.98 | 72.46 |
| update_profile | 1.03 | 44.39 |
| view_cart | 8.15 | 55.93 |
| write_review | 2.03 | 73.77 |

## Errors and business outcomes at the highest level

```json
{
 "statuses": {
  "ok": 144930,
  "empty_cart": 1814,
  "conflict_exhausted": 13,
  "not_found": 9
 },
 "errors": {
  "conflict_exhausted": {
   "count": 18,
   "first": "[elitesql:9] conflict, retry transaction: products/b8 changed after this transaction began",
   "ops": {
    "checkout": 18
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
