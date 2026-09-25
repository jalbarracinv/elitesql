# Mini-SaaS concurrency simulation — sidecar

Run: `B-2` on 2026-09-23T13:24:22 · commit `92e0290` · macOS-26.6.2-arm64-arm-64bit · 10 CPUs

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
| 100 | 100 | 13238.2 | 10243.3 | 2994.9 | 2.296 | 26.012 | 51.476 | 132.612 | 1259.094 | 51.479 | 99.976 | 0.799 | 4.98 | 136.1 | 1.86 | 0.104 | ok |

## Insights

- **Peak throughput**: 13238.2 ops/s at 100 users (p99 51.476 ms).
- **Capacity at p99 ≤ 10 ms**: 0 users (DB latency); 0 users counting pool wait.
- **Capacity at p99 ≤ 50 ms**: 0 users (DB latency); 0 users counting pool wait.
- **Capacity at p99 ≤ 100 ms**: 100 users (DB latency); 100 users counting pool wait.
- **Capacity at p99 ≤ 250 ms**: 100 users (DB latency); 100 users counting pool wait.
- **Capacity at p99 ≤ 1000 ms**: 100 users (DB latency); 100 users counting pool wait.
- **Ops per server core-second**: 100→2658.
- **Read-your-writes violations**: none.
- **Business invariants**: all stages ok.
- **Offline integrity check** (elitesql check): ok in 6.12 s.
  - right after killing the server (no clean close): exit 3, 0 errors, 190288 warnings; after one clean open/close: exit 0, 0 errors, 0 warnings.
- **Database size**: 56.4 MiB after the first stage, 56.4 MiB after the last; 8554 paid orders in total.

## p99 latency per operation (ms)

| op | share % | 100 |
|---|---:|---:|
| add_to_cart | 10.12 | 59.12 |
| admin_dashboard | 1.02 | 145.79 |
| browse | 20.53 | 19.69 |
| checkout | 4.06 | 197.93 |
| order_history | 3.04 | 35.65 |
| product_detail | 17.27 | 21.89 |
| recommend | 7.11 | 17.91 |
| relogin | 1.55 | 96.16 |
| restock | 0.31 | 138.22 |
| search_text | 8.11 | 22.55 |
| session_check | 12.14 | 49.32 |
| signup | 0.49 | 59.68 |
| update_cart_item | 3.05 | 48.88 |
| update_profile | 1.02 | 32.04 |
| view_cart | 8.16 | 33.61 |
| write_review | 2.02 | 44.51 |

## Errors and business outcomes at the highest level

```json
{
 "statuses": {
  "ok": 196073,
  "empty_cart": 2439,
  "not_found": 13,
  "conflict_exhausted": 48
 },
 "errors": {
  "conflict_exhausted": {
   "count": 76,
   "first": "[elitesql:9] conflict, retry transaction: products/c15 changed after this transaction began",
   "ops": {
    "checkout": 73,
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
