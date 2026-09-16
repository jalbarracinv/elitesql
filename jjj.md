offline check: ok (21.67 s); after kill: 0 errors, 986514 warnings
| users | EliteSQL before ops/s | p99 ms | success | **EliteSQL after ops/s** | **p99 ms** | success | SQLite ops/s | p99 ms | after / SQLite |
|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|
| 10 | 1 188 | 105 | 100.0 % | **6 955** | **10** | 100.0 % | 13 735 | 8 | 0.51x |
| 100 | 610 | 867 | 99.9 % | **4 854** | **141** | 100.0 % | 10 034 | 203 | 0.48x |
| 200 | 523 | 1 534 | 99.8 % | **4 423** | **546** | 99.8 % | 10 089 | 562 | 0.44x |
| 500 | 493 | 3 911 | 99.9 % | **3 675** | **3 109** | 98.8 % | 9 856 | 1 427 | 0.37x |
| 1 000 | 367 | 9 894 | 98.1 % | **1 742** | **5 411** | 98.0 % | 9 796 | 2 735 | 0.18x |
| 2 000 | 262 | 18 247 | 46.1 % | **1 370** | **6 326** | 96.1 % | 9 092 | 5 140 | 0.15x |
| 3 000 | 267 | 18 021 | 50.3 % | **1 664** | **5 619** | 96.9 % | 8 487 | 5 244 | 0.20x |
| 4 000 | 285 | 17 815 | 60.7 % | **1 934** | **5 041** | 98.3 % | 7 869 | 5 432 | 0.25x |
| 5 000 | 272 | 17 682 | 58.1 % | **1 585** | **5 710** | 97.2 % | 7 542 | 5 555 | 0.21x |

      25
- **Peak throughput**: 6955.4 ops/s at 10 users (p99 10.227 ms).
- **Capacity at p99 ≤ 50 ms**: 10 users (DB latency); 10 users counting pool wait.
- **Capacity at p99 ≤ 100 ms**: 10 users (DB latency); 10 users counting pool wait.
- **Capacity at p99 ≤ 250 ms**: 100 users (DB latency); 100 users counting pool wait.
- **Capacity at p99 ≤ 1000 ms**: 200 users (DB latency); 200 users counting pool wait.
- **USL fit**: σ (contention) = 0.43, κ (coherency) = 2.51e-04, predicted peak at N ≈ 47.6 (rmse log 0.3248).