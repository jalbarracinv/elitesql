# Mini-SaaS concurrency simulation — sidecar

Run: `viol-fixed-1` on 2026-09-23T16:49:15 · commit `92e0290` · macOS-26.6.2-arm64-arm-64bit · 10 CPUs

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
| 100 | 100 | 22877.3 | 17701.4 | 5175.9 | 0.85 | 14.823 | 26.469 | 77.32 | 979.994 | 26.47 | 99.99 | 0.706 | 5.81 | 184.2 | 2.5 | 0.151 | ok |

## Insights

- **Peak throughput**: 22877.3 ops/s at 100 users (p99 26.469 ms).
- **Capacity at p99 ≤ 10 ms**: 0 users (DB latency); 0 users counting pool wait.
- **Capacity at p99 ≤ 50 ms**: 100 users (DB latency); 100 users counting pool wait.
- **Capacity at p99 ≤ 100 ms**: 100 users (DB latency); 100 users counting pool wait.
- **Capacity at p99 ≤ 250 ms**: 100 users (DB latency); 100 users counting pool wait.
- **Capacity at p99 ≤ 1000 ms**: 100 users (DB latency); 100 users counting pool wait.
- **Ops per server core-second**: 100→3938.
- **Read-your-writes violations**: none.
- **Business invariants**: all stages ok.
- **Offline integrity check** (elitesql check): ok in 8.19 s.
  - right after killing the server (no clean close): exit 3, 0 errors, 279632 warnings; after one clean open/close: exit 0, 0 errors, 0 warnings.
- **Database size**: 86.5 MiB after the first stage, 86.5 MiB after the last; 14586 paid orders in total.

## p99 latency per operation (ms)

| op | share % | 100 |
|---|---:|---:|
| add_to_cart | 10.11 | 32.65 |
| admin_dashboard | 1.03 | 102.70 |
| browse | 20.39 | 6.32 |
| checkout | 4.08 | 73.18 |
| order_history | 3.02 | 15.23 |
| product_detail | 17.25 | 10.30 |
| recommend | 7.14 | 10.11 |
| relogin | 1.54 | 42.47 |
| restock | 0.3 | 123.24 |
| search_text | 8.18 | 9.15 |
| session_check | 12.16 | 24.94 |
| signup | 0.5 | 30.43 |
| update_cart_item | 3.02 | 25.80 |
| update_profile | 1.02 | 21.01 |
| view_cart | 8.21 | 11.91 |
| write_review | 2.04 | 25.65 |

## Errors and business outcomes at the highest level

```json
{
 "statuses": {
  "ok": 338952,
  "empty_cart": 4151,
  "not_found": 20,
  "conflict_exhausted": 36
 },
 "errors": {
  "conflict_exhausted": {
   "count": 58,
   "first": "[elitesql:9] conflict, retry transaction: products/c18 changed after this transaction began",
   "ops": {
    "checkout": 52,
    "restock": 6
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
