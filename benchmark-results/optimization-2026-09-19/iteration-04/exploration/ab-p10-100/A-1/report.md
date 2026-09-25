# Mini-SaaS concurrency simulation — sidecar

Run: `A-1` on 2026-09-23T14:49:37 · commit `92e0290` · macOS-26.6.2-arm64-arm-64bit · 10 CPUs

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
| 100 | 100 | 23576.7 | 18243.5 | 5333.2 | 0.814 | 15.076 | 26.831 | 68.678 | 1069.923 | 26.831 | 99.984 | 0.704 | 6.04 | 189.8 | 2.56 | 0.079 | ok |

## Insights

- **Peak throughput**: 23576.7 ops/s at 100 users (p99 26.831 ms).
- **Capacity at p99 ≤ 10 ms**: 0 users (DB latency); 0 users counting pool wait.
- **Capacity at p99 ≤ 50 ms**: 100 users (DB latency); 100 users counting pool wait.
- **Capacity at p99 ≤ 100 ms**: 100 users (DB latency); 100 users counting pool wait.
- **Capacity at p99 ≤ 250 ms**: 100 users (DB latency); 100 users counting pool wait.
- **Capacity at p99 ≤ 1000 ms**: 100 users (DB latency); 100 users counting pool wait.
- **Ops per server core-second**: 100→3903.
- **Read-your-writes violations**: [(100, 11)].
- **Business invariants**: all stages ok.
- **Offline integrity check** (elitesql check): ok in 8.33 s.
  - right after killing the server (no clean close): exit 3, 0 errors, 284950 warnings; after one clean open/close: exit 0, 0 errors, 0 warnings.
- **Database size**: 88.4 MiB after the first stage, 88.4 MiB after the last; 14935 paid orders in total.

## p99 latency per operation (ms)

| op | share % | 100 |
|---|---:|---:|
| add_to_cart | 10.12 | 32.67 |
| admin_dashboard | 1.03 | 107.86 |
| browse | 20.41 | 6.45 |
| checkout | 4.08 | 86.61 |
| order_history | 3.01 | 15.02 |
| product_detail | 17.23 | 10.88 |
| recommend | 7.16 | 9.74 |
| relogin | 1.53 | 43.38 |
| restock | 0.29 | 88.51 |
| search_text | 8.18 | 8.89 |
| session_check | 12.18 | 25.10 |
| signup | 0.5 | 27.90 |
| update_cart_item | 3.03 | 26.67 |
| update_profile | 1.02 | 21.18 |
| view_cart | 8.19 | 12.19 |
| write_review | 2.04 | 25.65 |

## Errors and business outcomes at the highest level

```json
{
 "statuses": {
  "ok": 349306,
  "empty_cart": 4266,
  "not_found": 21,
  "conflict_exhausted": 58
 },
 "errors": {
  "conflict_exhausted": {
   "count": 79,
   "first": "[elitesql:9] conflict, retry transaction: products/c13 changed after this transaction began",
   "ops": {
    "checkout": 76,
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
