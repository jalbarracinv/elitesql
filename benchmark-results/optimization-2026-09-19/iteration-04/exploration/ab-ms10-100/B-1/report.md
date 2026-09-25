# Mini-SaaS concurrency simulation — sidecar

Run: `B-1` on 2026-09-23T13:29:10 · commit `92e0290` · macOS-26.6.2-arm64-arm-64bit · 10 CPUs

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
| 100 | 100 | 13522.3 | 10463.5 | 3058.9 | 3.589 | 22.836 | 46.744 | 260.277 | 1267.723 | 46.746 | 99.926 | 0.755 | 4.3 | 136.0 | 1.96 | 0.09 | ok |

## Insights

- **Peak throughput**: 13522.3 ops/s at 100 users (p99 46.744 ms).
- **Capacity at p99 ≤ 10 ms**: 0 users (DB latency); 0 users counting pool wait.
- **Capacity at p99 ≤ 50 ms**: 100 users (DB latency); 100 users counting pool wait.
- **Capacity at p99 ≤ 100 ms**: 100 users (DB latency); 100 users counting pool wait.
- **Capacity at p99 ≤ 250 ms**: 100 users (DB latency); 100 users counting pool wait.
- **Capacity at p99 ≤ 1000 ms**: 100 users (DB latency); 100 users counting pool wait.
- **Ops per server core-second**: 100→3145.
- **Read-your-writes violations**: none.
- **Business invariants**: all stages ok.
- **Offline integrity check** (elitesql check): ok in 6.14 s.
  - right after killing the server (no clean close): exit 3, 0 errors, 192096 warnings; after one clean open/close: exit 0, 0 errors, 0 warnings.
- **Database size**: 56.9 MiB after the first stage, 56.9 MiB after the last; 8616 paid orders in total.

## p99 latency per operation (ms)

| op | share % | 100 |
|---|---:|---:|
| add_to_cart | 10.1 | 53.88 |
| admin_dashboard | 1.02 | 115.86 |
| browse | 20.54 | 17.50 |
| checkout | 4.07 | 495.41 |
| order_history | 3.06 | 26.97 |
| product_detail | 17.24 | 24.07 |
| recommend | 7.13 | 23.69 |
| relogin | 1.55 | 62.55 |
| restock | 0.3 | 97.25 |
| search_text | 8.13 | 16.33 |
| session_check | 12.13 | 30.80 |
| signup | 0.5 | 42.84 |
| update_cart_item | 3.03 | 39.34 |
| update_profile | 1.03 | 23.72 |
| view_cart | 8.15 | 27.11 |
| write_review | 2.03 | 40.04 |

## Errors and business outcomes at the highest level

```json
{
 "statuses": {
  "ok": 200192,
  "empty_cart": 2478,
  "conflict_exhausted": 150,
  "not_found": 15
 },
 "errors": {
  "conflict_exhausted": {
   "count": 187,
   "first": "[elitesql:9] conflict, retry transaction: products/b8 changed after this transaction began",
   "ops": {
    "checkout": 187
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
