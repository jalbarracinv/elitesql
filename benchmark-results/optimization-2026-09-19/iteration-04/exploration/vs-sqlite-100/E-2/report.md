# Mini-SaaS concurrency simulation — sidecar

Run: `E-2` on 2026-09-23T14:07:40 · commit `92e0290` · macOS-26.6.2-arm64-arm-64bit · 10 CPUs

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
- **seed time**: 1.14 s, 13.1 MiB after seeding

## Capacity and performance curve

Latencies are of the database call (service operation) in milliseconds; `user p99` adds the wait for a pooled connection.

| users | conns | ops/s | reads/s | writes/s | p50 | p95 | p99 | p99.9 | max | user p99 | success % | conflict retry % | srv CPU | srv RSS MiB | gen CPU | thr CV | invariants |
|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|
| 100 | 100 | 23646.5 | 18290.9 | 5355.6 | 0.825 | 15.226 | 26.414 | 64.093 | 1052.252 | 26.415 | 99.991 | 0.736 | 5.94 | 189.9 | 2.56 | 0.083 | ok |

## Insights

- **Peak throughput**: 23646.5 ops/s at 100 users (p99 26.414 ms).
- **Capacity at p99 ≤ 10 ms**: 0 users (DB latency); 0 users counting pool wait.
- **Capacity at p99 ≤ 50 ms**: 100 users (DB latency); 100 users counting pool wait.
- **Capacity at p99 ≤ 100 ms**: 100 users (DB latency); 100 users counting pool wait.
- **Capacity at p99 ≤ 250 ms**: 100 users (DB latency); 100 users counting pool wait.
- **Capacity at p99 ≤ 1000 ms**: 100 users (DB latency); 100 users counting pool wait.
- **Ops per server core-second**: 100→3981.
- **Read-your-writes violations**: none.
- **Business invariants**: all stages ok.
- **Offline integrity check** (elitesql check): ok in 8.45 s.
  - right after killing the server (no clean close): exit 3, 0 errors, 285468 warnings; after one clean open/close: exit 0, 0 errors, 0 warnings.
- **Database size**: 88.5 MiB after the first stage, 88.5 MiB after the last; 14980 paid orders in total.

## p99 latency per operation (ms)

| op | share % | 100 |
|---|---:|---:|
| add_to_cart | 10.13 | 31.22 |
| admin_dashboard | 1.03 | 93.69 |
| browse | 20.39 | 5.78 |
| checkout | 4.08 | 74.11 |
| order_history | 3.02 | 13.95 |
| product_detail | 17.22 | 9.80 |
| recommend | 7.15 | 9.12 |
| relogin | 1.54 | 40.33 |
| restock | 0.3 | 113.81 |
| search_text | 8.18 | 8.52 |
| session_check | 12.18 | 25.21 |
| signup | 0.5 | 32.25 |
| update_cart_item | 3.03 | 24.86 |
| update_profile | 1.03 | 21.22 |
| view_cart | 8.19 | 10.43 |
| write_review | 2.04 | 26.89 |

## Errors and business outcomes at the highest level

```json
{
 "statuses": {
  "ok": 350359,
  "empty_cart": 4286,
  "not_found": 21,
  "conflict_exhausted": 31
 },
 "errors": {
  "conflict_exhausted": {
   "count": 43,
   "first": "[elitesql:9] conflict, retry transaction: products/c15 changed after this transaction began",
   "ops": {
    "checkout": 40,
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
