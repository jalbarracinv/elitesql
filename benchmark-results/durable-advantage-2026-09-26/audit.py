#!/usr/bin/env python3
"""Audit every paired run and pooled control, with conservative true-user p99 bounds.

The original main run sorted latency/wait separately. DB p99 is exact; a
nonnegative pool wait gives DB_p99 <= user_p99 <= DB_p99 + maximum_pool_wait.
Use that upper bound for EliteSQL and the lower bound for SQLite; no latency
claim relies on the erroneous combined percentile. Controls keep raw samples.
"""
import json
import hashlib
from pathlib import Path
import statistics

OUT = Path(__file__).resolve().parent
median = statistics.median


def read(path):
    return json.loads(path.read_text())


def check_run(name, users):
    assert read(OUT / f"{name}.status.json")["returncode"] == 0
    s = read(OUT / name / f"stage-{users:05d}/summary.json")
    assert s["durability"] == "safe" and s["duration_s"] == 30
    assert s["failed_ops"] == 0 and not s["errors"] and s["consistency_violations"] == 0
    assert s["invariants"]["ok"] and read(OUT / name / "check.json")["ok"]
    if s["transport"] == "sqlite":
        p = s["sqlite_settings"]
        assert p["journal_mode"] == "wal" and p["synchronous"] == 2
        assert p["fullfsync"] == 1 and p["checkpoint_fullfsync"] == 1
    return s


assert read(OUT / "metadata.json")["sources_and_binaries_unchanged"]
assert read(OUT / "sqlite-pool-control.json")["correctness_passed"]
rows = []
resources = []
for users in (10, 100, 500):
    engines = {}
    for engine in ("sidecar", "sqlite", "pooled"):
        names = [f"r{r}-{engine}" if engine != "pooled" else f"sqlite-pooled-r{r}" for r in (1, 2, 3)]
        samples = [check_run(name, users) for name in names]
        values = [s["throughput_ops_s"] for s in samples]
        db99 = [s["latency"]["p99_us"] / 1000 for s in samples]
        upper = [(s["latency"]["p99_us"] + s["pool_wait"]["max_us"]) / 1000 for s in samples]
        true99 = [s["user_latency_p99_us"] / 1000 for s in samples] if engine == "pooled" else None
        if engine == "pooled":
            for name, s in zip(names, samples):
                audit = read(OUT / name / f"stage-{users:05d}/user-latency-audit.json")
                assert audit["count"] == s["ops_total_window"]
                assert audit["exact_p99_us"] == s["user_latency_p99_us"]
                for sample, value in audit["raw_sha256"].items():
                    path = OUT / name / f"stage-{users:05d}" / sample
                    assert hashlib.sha256(path.read_bytes()).hexdigest() == value
        engines[engine] = dict(ops_s=values, median_ops_s=median(values), db_p99_ms=db99,
                               user_p99_upper_bound_ms=upper, exact_user_p99_ms=true99)
        cpu = [(s.get("server_cpu_cores") or {}).get("mean", 0)
               + (s.get("clients_cpu_cores") or {}).get("mean", 0) for s in samples]
        rss = [(s.get("server_rss_mib") or {}).get("mean", 0)
               + (s.get("clients_rss_mib") or {}).get("mean", 0) for s in samples]
        resources.append(dict(users=users, engine=engine, median_mean_total_cpu_cores=median(cpu),
                              median_mean_total_rss_mib=median(rss)))
    elite = engines["sidecar"]
    direct = engines["sqlite"]
    pooled = engines["pooled"]
    best_sqlite = max(direct["median_ops_s"], pooled["median_ops_s"])
    throughput = elite["median_ops_s"] / best_sqlite
    paired_throughput = [e / max(s, p) for e, s, p in zip(elite["ops_s"], direct["ops_s"], pooled["ops_s"])]
    # Lower bound for a user-p99 advantage against the better SQLite variant.
    latency_bounds = [min(s, p) / e for e, s, p in zip(elite["user_p99_upper_bound_ms"],
                                                     direct["db_p99_ms"], pooled["exact_user_p99_ms"])]
    rows.append(dict(users=users, engines=engines, advantage_over_best_sqlite=throughput,
                     paired_advantages=paired_throughput, conservative_p99_advantage_bounds=latency_bounds))
target = [r for r in rows if r["users"] in (100, 500)]
passed = all(r["advantage_over_best_sqlite"] >= 2 and min(r["paired_advantages"]) > 1
             and min(r["conservative_p99_advantage_bounds"]) >= 2 for r in target)
result = dict(correctness_passed=True, stricter_gate_passed=passed, rows=rows, resources=resources,
              latency_method="main DB p99 exact; EliteSQL true user p99 conservatively bounded by DB p99 + max pool wait; pooled SQLite exact paired samples")
(OUT / "audit.json").write_text(json.dumps(result, indent=2) + "\n")
print(json.dumps(dict(stricter_gate_passed=passed,
                      rows=[{k:v for k,v in row.items() if k != "engines"} for row in rows]), indent=2))
assert passed, "hypothesis did not pass against best SQLite configuration"
