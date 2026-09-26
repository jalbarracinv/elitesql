#!/usr/bin/env python3
"""Reproduce the durable Mini-SaaS hypothesis without changing the operation mix.

Build first: cargo build --locked --release -p elitesql-cli -p elitesql-ffi
Then: python3 scripts/prove-durable-advantage.py --output <unused directory>
The JSON gate is deliberately fixed before running: >=2x throughput AND >=2x
better p99 at both 100 and 500 users, every paired repetition favors EliteSQL,
and no correctness failures. A failing hypothesis keeps all its evidence.
"""

import argparse
import importlib.util
import json
import os
from pathlib import Path
import platform
import statistics
import subprocess
import sys
import tempfile
import time

ROOT = Path(__file__).resolve().parents[1]
spec = importlib.util.spec_from_file_location("comparison", ROOT / "scripts/compare-sqlite.py")
comparison = importlib.util.module_from_spec(spec)
spec.loader.exec_module(comparison)


def save(path, value):
    path.write_text(json.dumps(value, indent=2) + "\n")


def cell(values, digits=1):
    return f"{statistics.median(values):,.{digits}f} [{min(values):,.{digits}f}–{max(values):,.{digits}f}]"


def summarize(output, repetitions, levels):
    rows = []
    correct = True
    for users in levels:
        elite, sqlite, paired_ratios = [], [], []
        for repetition in range(1, repetitions + 1):
            pair = []
            for transport in ("sidecar", "sqlite"):
                directory = output / f"r{repetition}-{transport}"
                summary = json.loads((directory / f"stage-{users:05d}/summary.json").read_text())
                check = json.loads((directory / "check.json").read_text())
                ok = (summary["failed_ops"] == 0 and not summary["errors"]
                      and summary["consistency_violations"] == 0
                      and summary["invariants"]["ok"] and check["ok"])
                if transport == "sqlite":
                    actual = summary.get("sqlite_settings", {})
                    ok &= (actual.get("journal_mode") == "wal"
                           and actual.get("synchronous") == 2
                           and actual.get("fullfsync") == 1
                           and actual.get("checkpoint_fullfsync") == 1)
                correct &= ok
                pair.append(summary)
            elite.append(pair[0])
            sqlite.append(pair[1])
            paired_ratios.append(pair[0]["throughput_ops_s"] / pair[1]["throughput_ops_s"])
        e = [s["throughput_ops_s"] for s in elite]
        q = [s["throughput_ops_s"] for s in sqlite]
        ep = [s["user_latency_p99_us"] / 1000 for s in elite]
        qp = [s["user_latency_p99_us"] / 1000 for s in sqlite]
        rows.append(dict(users=users, elite_ops=e, sqlite_ops=q, elite_p99_ms=ep,
                         sqlite_p99_ms=qp, throughput_ratio=statistics.median(e) / statistics.median(q),
                         p99_ratio=statistics.median(qp) / statistics.median(ep),
                         paired_throughput_ratios=paired_ratios))
    target = [row for row in rows if row["users"] in (100, 500)]
    gate = (correct and repetitions >= 3 and len(target) == 2
            and all(row["throughput_ratio"] >= 2 and row["p99_ratio"] >= 2
                    and min(row["paired_throughput_ratios"]) > 1 for row in target))
    save(output / "summary.json", dict(correct=correct, gate_passed=gate, rows=rows))
    lines = ["# Durable Mini-SaaS: EliteSQL vs SQLite", "",
             "Medians [minimum–maximum], not confidence intervals. User p99 includes pool wait.", "",
             "| Users | EliteSQL ops/s | SQLite ops/s | Throughput ratio | EliteSQL p99 ms | SQLite p99 ms |",
             "| ---: | ---: | ---: | ---: | ---: | ---: |"]
    for row in rows:
        lines.append(f"| {row['users']} | {cell(row['elite_ops'])} | {cell(row['sqlite_ops'])} | "
                     f"{row['throughput_ratio']:.2f}× | {cell(row['elite_p99_ms'])} | {cell(row['sqlite_p99_ms'])} |")
    lines += ["", f"Correctness gate: **{correct}**. Predeclared performance gate: **{gate}**.", "",
              "Safe vs WAL/FULL; fullfsync and checkpoint_fullfsync enabled on SQLite. "
              "Automatic checkpoints stay enabled; EliteSQL uses its default resource budgets. "
              "Same baseline service, 20K accounts, 5K products, 15 operations; recommendations excluded. "
              "Text rankings are engine-specific. Sidecar vs embedded SQLite; transport differs. "
              "Ten generator processes, no think time. Fresh database per repetition; levels accumulate.", "",
              "See jobs.json for exact commands, metadata.json for source/binary hashes, each stage's "
              "summary.json/resources.csv/timeseries.csv for latency, CPU, RSS, errors and group-commit "
              "counters; check.json records recovery outside the timed window. Engine counters cover "
              "the entire stage, including ramp and warmup, rather than only the measurement window."]
    (output / "results.md").write_text("\n".join(lines) + "\n")
    print("\n".join(lines[:9]), flush=True)
    return gate


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--output", required=True, type=Path)
    parser.add_argument("--repetitions", type=int, default=3)
    parser.add_argument("--duration", type=float, default=30)
    parser.add_argument("--levels", default="10,100,500")
    parser.add_argument("--summarize-only", action="store_true")
    args = parser.parse_args()
    output = args.output.resolve()
    levels = list(map(int, args.levels.split(",")))
    if args.summarize_only:
        summarize(output, args.repetitions, levels)
        return
    if output.exists() and any(output.iterdir()):
        parser.error("output must be empty; existing evidence will not be overwritten")
    output.mkdir(parents=True, exist_ok=True)
    sources = {name: comparison.refresh.digest(ROOT / name) for name in comparison.source_files()}
    binaries = [ROOT / "target/release/elitesql", ROOT / "target/release/libelitesql.dylib"]
    hashes = {str(path.relative_to(ROOT)): comparison.refresh.digest(path) for path in binaries}
    metadata = dict(git_commit=comparison.refresh.capture(["git", "rev-parse", "HEAD"]),
                    git_status=comparison.refresh.capture(["git", "status", "--short"]),
                    source_sha256=sources, binary_sha256=hashes, platform=platform.platform(),
                    python=sys.version, start=comparison.refresh.observations(),
                    repetitions=args.repetitions, duration=args.duration, levels=levels,
                    hypothesis="durable small transactions: >=2x throughput and >=2x better p99 at 100/500",
                    gate="3+ repetitions, every pair favors EliteSQL, zero failures, RYW and invariants pass")
    save(output / "metadata.json", metadata)
    patch = subprocess.check_output(["git", "diff", "--binary", "HEAD"])
    for name in sources:
        if subprocess.run(["git", "ls-files", "--error-unmatch", name], capture_output=True).returncode:
            patch += subprocess.run(["git", "diff", "--no-index", "--binary", "/dev/null", name],
                                    capture_output=True).stdout
    (output / "source.patch").write_bytes(patch)
    jobs = []
    for repetition in range(1, args.repetitions + 1):
        for transport in (("sidecar", "sqlite") if repetition % 2 else ("sqlite", "sidecar")):
            name = f"r{repetition}-{transport}"
            argv = [sys.executable, str(ROOT / "examples/saas_simulation/sweep.py"),
                    "--transport", transport, "--levels", args.levels, "--duration", str(args.duration),
                    "--warmup", "5", "--ramp", "5", "--think", "0", "--processes", "10",
                    "--durability", "safe", "--products", "5000", "--accounts", "20000",
                    "--scenario", "baseline", "--seed", str(repetition), "--out", str(output / name)]
            jobs.append(dict(name=name, argv=argv))
    save(output / "jobs.json", jobs)
    env = dict(os.environ, SAAS_SIM_DROP_OPS="recommend",
               ELITESQL_LIB=str(binaries[1]), PYTHONPATH=str(ROOT / "bindings/python"))
    for index, job in enumerate(jobs):
        print(f"[{index + 1}/{len(jobs)}] {job['name']}", flush=True)
        started = time.monotonic()
        status = dict(job, before=comparison.refresh.observations())
        with tempfile.TemporaryDirectory(prefix="esql-proof-") as database:
            argv = [*job["argv"], "--db-dir", database]
            status["argv"] = argv
            with (output / f"{job['name']}.log").open("w") as log:
                with (output / f"{job['name']}.resources.txt").open("w") as resources:
                    result = subprocess.run(["/usr/bin/time", "-l" if platform.system() == "Darwin" else "-v",
                                             *argv], stdout=log, stderr=resources, env=env, cwd=ROOT)
        status.update(returncode=result.returncode, seconds=time.monotonic() - started,
                      after=comparison.refresh.observations())
        save(output / f"{job['name']}.status.json", status)
        if result.returncode:
            raise SystemExit(f"job failed: {job['name']}; see retained logs")
    if any(comparison.refresh.digest(ROOT / name) != value for name, value in sources.items()):
        raise SystemExit("sources changed during measurement; results cannot establish the hypothesis")
    if any(comparison.refresh.digest(ROOT / name) != value for name, value in hashes.items()):
        raise SystemExit("binaries changed during measurement")
    metadata["end"] = comparison.refresh.observations()
    metadata["sources_and_binaries_unchanged"] = True
    save(output / "metadata.json", metadata)
    summarize(output, args.repetitions, levels)


if __name__ == "__main__":
    main()
