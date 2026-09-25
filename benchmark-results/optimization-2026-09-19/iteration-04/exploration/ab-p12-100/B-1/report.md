# Mini-SaaS concurrency simulation — sidecar

Run: `B-1` on 2026-09-23T15:02:51 · commit `92e0290` · macOS-26.6.2-arm64-arm-64bit · 10 CPUs

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
| 100 | 100 | 23531.5 | 18208.9 | 5322.6 | 0.87 | 14.996 | 26.435 | 65.274 | 1063.156 | 26.436 | 99.988 | 0.714 | 5.65 | 229.5 | 2.68 | 0.081 | ok |

## Insights

- **Peak throughput**: 23531.5 ops/s at 100 users (p99 26.435 ms).
- **Capacity at p99 ≤ 10 ms**: 0 users (DB latency); 0 users counting pool wait.
- **Capacity at p99 ≤ 50 ms**: 100 users (DB latency); 100 users counting pool wait.
- **Capacity at p99 ≤ 100 ms**: 100 users (DB latency); 100 users counting pool wait.
- **Capacity at p99 ≤ 250 ms**: 100 users (DB latency); 100 users counting pool wait.
- **Capacity at p99 ≤ 1000 ms**: 100 users (DB latency); 100 users counting pool wait.
- **Ops per server core-second**: 100→4165.
- **Read-your-writes violations**: none.
- **Business invariants**: all stages ok.
- **Offline integrity check** (elitesql check): ok in 8.26 s.
  - right after killing the server (no clean close): exit 3, 0 errors, 283786 warnings; after one clean open/close: exit 0, 0 errors, 0 warnings.
- **Database size**: 88.2 MiB after the first stage, 88.2 MiB after the last; 14865 paid orders in total.

## p99 latency per operation (ms)

| op | share % | 100 |
|---|---:|---:|
| add_to_cart | 10.12 | 31.22 |
| admin_dashboard | 1.02 | 99.11 |
| browse | 20.41 | 6.70 |
| checkout | 4.07 | 82.71 |
| order_history | 3.02 | 14.71 |
| product_detail | 17.24 | 10.60 |
| recommend | 7.15 | 10.16 |
| relogin | 1.53 | 41.68 |
| restock | 0.3 | 77.44 |
| search_text | 8.17 | 9.81 |
| session_check | 12.17 | 24.95 |
| signup | 0.51 | 25.41 |
| update_cart_item | 3.03 | 26.48 |
| update_profile | 1.03 | 21.09 |
| view_cart | 8.2 | 13.07 |
| write_review | 2.04 | 25.71 |

## Errors and business outcomes at the highest level

```json
{
 "statuses": {
  "ok": 348670,
  "empty_cart": 4239,
  "conflict_exhausted": 43,
  "not_found": 20
 },
 "errors": {
  "conflict_exhausted": {
   "count": 58,
   "first": "[elitesql:9] conflict, retry transaction: products/c15 changed after this transaction began",
   "ops": {
    "checkout": 58
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
