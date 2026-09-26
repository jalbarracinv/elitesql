#!/usr/bin/env python3
"""Counterfactual: one SQLite connection with FIFO admission instead of lock contention.

Run only after the paired benchmark finishes. Same service, mix, seed,
durability and timing, but one generator process and one pooled connection.
User latency includes the FIFO pool wait. SQLite checkpoints remain enabled.
"""
import hashlib
import csv
import gzip
import json
import os
from pathlib import Path
import runpy
import subprocess
import sys
import tempfile
import time

OUT = Path(__file__).resolve().parent
ROOT = OUT.parents[1]
helpers = runpy.run_path(str(ROOT / "scripts/prove-durable-advantage.py"))
refresh = helpers["comparison"].refresh
save = helpers["save"]
metadata = json.loads((OUT / "metadata.json").read_text())
assert metadata.get("sources_and_binaries_unchanged"), "finish paired runs before controls"
control = dict(script_sha256=hashlib.sha256(Path(__file__).read_bytes()).hexdigest(),
               start=refresh.observations(), rationale="one SQLite connection, FIFO queue, include pool wait")
save(OUT / "sqlite-pool-control.json", control)
env = dict(os.environ, SAAS_SIM_DROP_OPS="recommend", PYTHONPATH=str(ROOT / "bindings/python"))
jobs = []
for repetition in range(1, 4):
    name = f"sqlite-pooled-r{repetition}"
    assert not (OUT / name).exists(), "do not overwrite completed controls"
    argv = [sys.executable, str(ROOT / "examples/saas_simulation/sweep.py"), "--transport", "sqlite",
            "--levels", "10,100,500", "--duration", "30", "--ramp", "5", "--warmup", "5",
            "--think", "0", "--processes", "1", "--connections", "1", "--durability", "safe",
            "--accounts", "20000", "--products", "5000", "--scenario", "baseline",
            "--seed", str(repetition), "--keep-samples", "--out", str(OUT / name)]
    jobs.append(dict(name=name, argv=argv))
save(OUT / "sqlite-pool-jobs.json", jobs)
for job in jobs:
    print(job["name"], flush=True)
    status = dict(job, before=refresh.observations())
    started = time.monotonic()
    with tempfile.TemporaryDirectory(prefix="esql-pool-control-") as directory:
        argv = [*job["argv"], "--db-dir", directory]
        status["argv"] = argv
        with (OUT / f"{job['name']}.log").open("w") as log:
            with (OUT / f"{job['name']}.resources.txt").open("w") as resources:
                run = subprocess.run(["/usr/bin/time", "-l", *argv], stdout=log, stderr=resources,
                                     cwd=ROOT, env=env)
    status.update(returncode=run.returncode, seconds=time.monotonic() - started,
                  after=refresh.observations())
    save(OUT / f"{job['name']}.status.json", status)
    assert run.returncode == 0, "control failed; retain evidence"
    assert json.loads((OUT / job["name"] / "check.json").read_text())["ok"]
    for users in (10, 100, 500):
        s = json.loads((OUT / job["name"] / f"stage-{users:05d}" / "summary.json").read_text())
        assert s["failed_ops"] == 0 and not s["errors"] and s["consistency_violations"] == 0
        assert s["invariants"]["ok"] and s["sqlite_settings"]["synchronous"] == 2
        assert s["sqlite_settings"]["fullfsync"] == 1
        # The original aggregator sorts latency and pool wait separately
        # before combining them. Recover the true user percentile from paired
        # samples; keep the raw samples and a receipt so this is auditable.
        stage = OUT / job["name"] / f"stage-{users:05d}"
        latencies = []
        sample_hashes = {}
        for sample in stage.glob("samples-*.csv.gz"):
            sample_hashes[sample.name] = refresh.digest(sample)
            with gzip.open(sample, "rt", newline="") as source:
                for row in csv.DictReader(source):
                    if 10000 <= int(row["start_ms"]) < 40000:
                        latencies.append(int(row["latency_us"]) + int(row["pool_wait_us"]))
        assert len(latencies) == s["ops_total_window"]
        sys.path.insert(0, str(ROOT / "examples/saas_simulation"))
        from runner import percentile
        exact = round(percentile(sorted(latencies), .99), 1)
        save(stage / "user-latency-audit.json", dict(raw_sha256=sample_hashes, count=len(latencies),
             original_reported_p99_us=s["user_latency_p99_us"], exact_p99_us=exact,
             window_ms=[10000, 40000], formula="percentile(latency_us + wait_us, .99) before separate sorting"))
        s["user_latency_p99_us"] = exact
        save(stage / "summary.json", s)
    subprocess.run([sys.executable, str(ROOT / "examples/saas_simulation/sweep.py"),
                    "--rebuild-report", str(OUT / job["name"])], check=True,
                   stdout=subprocess.DEVNULL, cwd=ROOT, env=env)
for name, value in metadata["source_sha256"].items():
    assert refresh.digest(ROOT / name) == value, "sources changed during controls"
control.update(end=refresh.observations(), sources_unchanged=True, correctness_passed=True)
save(OUT / "sqlite-pool-control.json", control)
