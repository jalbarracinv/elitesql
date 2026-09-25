"""Summarize the interleaved A/B runs: one row per level and variant."""
import csv
import json
import glob
import os
import statistics

AB = os.path.dirname(os.path.abspath(__file__))
COLS = ["throughput_ops_s", "p50_ms", "p99_ms", "success_rate_pct",
        "failed_ops", "conflict_retries", "server_cpu_cores"]

data = {}  # (variant, users) -> list of rows
for path in sorted(glob.glob(os.path.join(AB, "run-*-*/stages.csv"))):
    variant = path.split("/")[-2].split("-")[-1]
    with open(path) as f:
        for row in csv.DictReader(f):
            row["_run"] = path.split("/")[-2]
            stage = os.path.join(os.path.dirname(path), f"stage-{int(row['users']):05d}", "summary.json")
            summary = json.load(open(stage))
            row["exhausted"] = summary.get("errors", {}).get("conflict_exhausted", {}).get("count", 0)
            row["rebased"] = summary.get("engine_stats", {}).get("delta", {}).get("delta_rebased_rows", "-")
            data.setdefault((variant, int(row["users"])), []).append(row)

print(f"{'users':>5} {'variant':>7} {'ops/s (runs)':>22} {'p50':>6} {'p99':>7} "
      f"{'succ%':>7} {'exhausted':>10} {'retries':>9} {'rebased':>9} {'srvCPU':>6}")
for users in sorted({u for _, u in data}):
    for variant in ("before", "after"):
        rows = data.get((variant, users), [])
        if not rows:
            continue
        ops = [float(r["throughput_ops_s"]) for r in rows]
        mean = lambda c: statistics.mean(float(r[c]) for r in rows)
        print(f"{users:>5} {variant:>7} {statistics.mean(ops):>9.0f} ({' / '.join(f'{o:.0f}' for o in ops)})"
              f" {mean('p50_ms'):>6.2f} {mean('p99_ms'):>7.1f} {mean('success_rate_pct'):>7.3f}"
              f" {' / '.join(str(r['exhausted']) for r in rows):>10}"
              f" {' / '.join(r['conflict_retries'] for r in rows):>9}"
              f" {' / '.join(str(r['rebased']) for r in rows):>9} {mean('server_cpu_cores'):>6.2f}")
