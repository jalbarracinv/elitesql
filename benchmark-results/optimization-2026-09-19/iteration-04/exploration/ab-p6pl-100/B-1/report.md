# Mini-SaaS concurrency simulation — sidecar

Run: `B-1` on 2026-09-23T14:01:59 · commit `92e0290` · macOS-26.6.2-arm64-arm-64bit · 10 CPUs

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
| 100 | 100 | 22275.7 | 17231.5 | 5044.2 | 1.538 | 15.172 | 23.593 | 83.877 | 1045.145 | 23.595 | 99.935 | 0.732 | 5.87 | 179.6 | 3.31 | 0.07 | ok |

## Insights

- **Peak throughput**: 22275.7 ops/s at 100 users (p99 23.593 ms).
- **Capacity at p99 ≤ 10 ms**: 0 users (DB latency); 0 users counting pool wait.
- **Capacity at p99 ≤ 50 ms**: 100 users (DB latency); 100 users counting pool wait.
- **Capacity at p99 ≤ 100 ms**: 100 users (DB latency); 100 users counting pool wait.
- **Capacity at p99 ≤ 250 ms**: 100 users (DB latency); 100 users counting pool wait.
- **Capacity at p99 ≤ 1000 ms**: 100 users (DB latency); 100 users counting pool wait.
- **Ops per server core-second**: 100→3795.
- **Read-your-writes violations**: none.
- **Business invariants**: all stages ok.
- **Offline integrity check** (elitesql check): ok in 7.99 s.
  - right after killing the server (no clean close): exit 3, 0 errors, 270728 warnings; after one clean open/close: exit 0, 0 errors, 0 warnings.
- **Database size**: 84.2 MiB after the first stage, 84.2 MiB after the last; 13866 paid orders in total.

## p99 latency per operation (ms)

| op | share % | 100 |
|---|---:|---:|
| add_to_cart | 10.14 | 23.93 |
| admin_dashboard | 1.02 | 54.83 |
| browse | 20.39 | 4.09 |
| checkout | 4.07 | 314.10 |
| order_history | 3.01 | 14.44 |
| product_detail | 17.21 | 6.84 |
| recommend | 7.15 | 5.81 |
| relogin | 1.53 | 32.20 |
| restock | 0.3 | 71.61 |
| search_text | 8.17 | 5.67 |
| session_check | 12.19 | 21.62 |
| signup | 0.51 | 22.57 |
| update_cart_item | 3.04 | 21.83 |
| update_profile | 1.02 | 18.32 |
| view_cart | 8.22 | 10.59 |
| write_review | 2.04 | 18.80 |

## Errors and business outcomes at the highest level

```json
{
 "statuses": {
  "ok": 329922,
  "empty_cart": 3976,
  "not_found": 20,
  "conflict_exhausted": 218
 },
 "errors": {
  "conflict_exhausted": {
   "count": 267,
   "first": "[elitesql:9] conflict, retry transaction: products/b4 changed after this transaction began",
   "ops": {
    "checkout": 266,
    "restock": 1
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
