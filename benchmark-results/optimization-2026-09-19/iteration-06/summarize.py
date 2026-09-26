"""One row per level and variant, means of the interleaved runs."""
import csv, glob, json, os, statistics
B = os.path.dirname(os.path.abspath(__file__))
data = {}
for path in sorted(glob.glob(os.path.join(B, "run-*-*/stages.csv"))):
    variant = path.split("/")[-2].split("-")[-1]
    for row in csv.DictReader(open(path)):
        data.setdefault((variant, int(row["users"])), []).append(row)
def m(rows, c):
    vals = [float(r[c]) for r in rows if r.get(c) not in (None, "", "None")]
    return statistics.mean(vals) if vals else float("nan")
print(f"{'users':>5} {'variant':>8} {'ops/s (runs)':>24} {'p50':>7} {'p99':>8} {'succ%':>8} {'srvCPU':>6} {'ops/core-s':>10}")
for users in sorted({u for _, u in data}):
    for variant in ("base", "current", "sqlite"):
        rows = data.get((variant, users), [])
        if not rows: continue
        ops = [float(r["throughput_ops_s"]) for r in rows]
        cpu = m(rows, "server_cpu_cores")
        per = statistics.mean(ops) / cpu if cpu == cpu and cpu > 0 else float("nan")
        print(f"{users:>5} {variant:>8} {statistics.mean(ops):>9.0f} ({' / '.join(f'{o:.0f}' for o in ops)}) "
              f"{m(rows,'p50_ms'):>7.2f} {m(rows,'p99_ms'):>8.1f} {m(rows,'success_rate_pct'):>8.3f} {cpu:>6.2f} {per:>10.0f}")
