# Mini-SaaS concurrency simulation — sqlite

Run: `S-2` on 2026-09-23T14:08:22 · commit `92e0290` · macOS-26.6.2-arm64-arm-64bit · 10 CPUs

## Configuration

- **transport**: sqlite
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
- **seed time**: 0.14 s, 4.6 MiB after seeding

## Capacity and performance curve

Latencies are of the database call (service operation) in milliseconds; `user p99` adds the wait for a pooled connection.

| users | conns | ops/s | reads/s | writes/s | p50 | p95 | p99 | p99.9 | max | user p99 | success % | conflict retry % | srv CPU | srv RSS MiB | gen CPU | thr CV | invariants |
|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|
| 100 | 100 | 30157.2 | 23337.7 | 6819.5 | 0.042 | 1.654 | 45.136 | 702.91 | 3777.407 | 45.136 | 100.0 | 0.0 | None | None | 2.8 | 0.022 | ok |

## Insights

- **Peak throughput**: 30157.2 ops/s at 100 users (p99 45.136 ms).
- **Capacity at p99 ≤ 10 ms**: 0 users (DB latency); 0 users counting pool wait.
- **Capacity at p99 ≤ 50 ms**: 100 users (DB latency); 100 users counting pool wait.
- **Capacity at p99 ≤ 100 ms**: 100 users (DB latency); 100 users counting pool wait.
- **Capacity at p99 ≤ 250 ms**: 100 users (DB latency); 100 users counting pool wait.
- **Capacity at p99 ≤ 1000 ms**: 100 users (DB latency); 100 users counting pool wait.
- **Generator CPU includes the database**: SQLite runs inside the load-generator processes, so `gen CPU` is application plus engine and there is no server column.
- **Read-your-writes violations**: none.
- **Business invariants**: all stages ok.
- **Offline integrity check** (PRAGMA integrity_check): ok in 0.04 s.
- **Database size**: 10.7 MiB after the first stage, 10.7 MiB after the last; 18329 paid orders in total.

## p99 latency per operation (ms)

| op | share % | 100 |
|---|---:|---:|
| add_to_cart | 10.12 | 207.13 |
| admin_dashboard | 1.03 | 4.14 |
| browse | 20.39 | 0.20 |
| checkout | 4.07 | 166.74 |
| order_history | 3.03 | 0.31 |
| product_detail | 17.26 | 0.18 |
| recommend | 7.09 | 0.86 |
| relogin | 1.53 | 262.87 |
| restock | 0.3 | 388.44 |
| search_text | 8.17 | 0.90 |
| session_check | 12.24 | 221.54 |
| signup | 0.5 | 224.48 |
| update_cart_item | 3.02 | 135.42 |
| update_profile | 1.02 | 221.57 |
| view_cart | 8.18 | 0.08 |
| write_review | 2.04 | 139.67 |

## Errors and business outcomes at the highest level

```json
{
 "statuses": {
  "ok": 446867,
  "empty_cart": 5461,
  "not_found": 30
 },
 "errors": {}
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
