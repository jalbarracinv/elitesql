# Mini-SaaS concurrency simulation — sidecar

Run: `A-2` on 2026-09-23T13:55:56 · commit `92e0290` · macOS-26.6.2-arm64-arm-64bit · 10 CPUs

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
| 100 | 100 | 23192.1 | 17938.5 | 5253.7 | 1.102 | 15.442 | 27.358 | 70.515 | 984.728 | 27.36 | 99.985 | 0.716 | 6.29 | 215.8 | 2.91 | 0.063 | ok |

## Insights

- **Peak throughput**: 23192.1 ops/s at 100 users (p99 27.358 ms).
- **Capacity at p99 ≤ 10 ms**: 0 users (DB latency); 0 users counting pool wait.
- **Capacity at p99 ≤ 50 ms**: 100 users (DB latency); 100 users counting pool wait.
- **Capacity at p99 ≤ 100 ms**: 100 users (DB latency); 100 users counting pool wait.
- **Capacity at p99 ≤ 250 ms**: 100 users (DB latency); 100 users counting pool wait.
- **Capacity at p99 ≤ 1000 ms**: 100 users (DB latency); 100 users counting pool wait.
- **Ops per server core-second**: 100→3687.
- **Read-your-writes violations**: none.
- **Business invariants**: all stages ok.
- **Offline integrity check** (elitesql check): ok in 8.16 s.
  - right after killing the server (no clean close): exit 3, 0 errors, 281044 warnings; after one clean open/close: exit 0, 0 errors, 0 warnings.
- **Database size**: 87.2 MiB after the first stage, 87.2 MiB after the last; 14681 paid orders in total.

## p99 latency per operation (ms)

| op | share % | 100 |
|---|---:|---:|
| add_to_cart | 10.12 | 28.69 |
| admin_dashboard | 1.02 | 109.37 |
| browse | 20.39 | 9.82 |
| checkout | 4.09 | 86.94 |
| order_history | 3.01 | 19.67 |
| product_detail | 17.22 | 10.69 |
| recommend | 7.15 | 8.04 |
| relogin | 1.54 | 40.04 |
| restock | 0.3 | 52.80 |
| search_text | 8.18 | 11.25 |
| session_check | 12.17 | 26.07 |
| signup | 0.51 | 30.86 |
| update_cart_item | 3.03 | 23.57 |
| update_profile | 1.03 | 18.58 |
| view_cart | 8.2 | 16.12 |
| write_review | 2.03 | 23.61 |

## Errors and business outcomes at the highest level

```json
{
 "statuses": {
  "ok": 343594,
  "empty_cart": 4217,
  "conflict_exhausted": 51,
  "not_found": 20
 },
 "errors": {
  "conflict_exhausted": {
   "count": 68,
   "first": "[elitesql:9] conflict, retry transaction: products/c15 changed after this transaction began",
   "ops": {
    "checkout": 68
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
