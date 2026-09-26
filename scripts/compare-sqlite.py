#!/usr/bin/env python3
"""Measure the current EliteSQL tree against SQLite and write benchmark.md.

Build both Rust bench executables and the release CLI/FFI before running.
All measurements run sequentially; successful jobs can be resumed only with
the same sources, binaries and configuration. No historical measurements enter
the report.
"""
import argparse
import csv
import hashlib
import importlib.util
import json
import os
from pathlib import Path
import platform
import sqlite3
import statistics
import subprocess
import sys
import time

ROOT = Path(__file__).resolve().parents[1]
spec = importlib.util.spec_from_file_location("refresh", ROOT / "scripts/refresh-benchmarks.py")
refresh = importlib.util.module_from_spec(spec)
spec.loader.exec_module(refresh)


def read_csv(path):
    with path.open(newline="") as handle:
        return list(csv.DictReader(handle))


def cell(values, digits=3):
    return f"{statistics.median(values):,.{digits}f} [{min(values):,.{digits}f}–{max(values):,.{digits}f}]"


def source_files():
    raw = subprocess.check_output(["git", "ls-files", "-z", "--cached", "--others", "--exclude-standard"])
    return sorted({os.fsdecode(name) for name in raw.split(b"\0") if name and
                   (Path(os.fsdecode(name)).parts[0] in {"crates", "bindings", "scripts", ".github", "examples"}
                    or os.fsdecode(name) in {"Cargo.toml", "Cargo.lock"})
                   and (ROOT / os.fsdecode(name)).is_file()})


def jobs_for(output, binaries, args):
    jobs = []

    def add(name, argv, **extra):
        jobs.append(dict(name=name, argv=list(map(str, argv)), **extra))

    for repetition in range(1, args.repetitions + 1):
        for rows in ("1m", "10m"):
            for batch in ("1k", "10k"):
                for engine in (("elitesql", "sqlite") if repetition % 2 else ("sqlite", "elitesql")):
                    name = f"scale-{rows}-{batch}-r{repetition}-{engine}"
                    add(name, [binaries["scale_vs_sqlite"], "--rows", rows, "--batch-size", batch,
                               "--durability", "fast", "--point-reads", "100k", "--full-scans", "1",
                               "--engine", engine, "--csv", output / f"{name}.csv"])
    for mode in ("fast", "balanced", "safe"):
        argv = [binaries["concurrent_writers"], "--writers", "1,4,8", "--rows",
                "10k" if mode == "safe" else "200k", "--batch-size", "10", "--repetitions",
                args.repetitions, "--durability", mode, "--csv", output / f"writers-{mode}.csv"]
        if mode == "safe":
            argv += ["--sqlite-sync", "strict", "--safe-group-delay-us", "200"]
        add(f"writers-{mode}", argv)
    add("saas-operations", [sys.executable, ROOT / "examples/saas_simulation/ops_cost.py",
                           "--metric", "full-v2", "--scenario", "baseline", "--products", "5000",
                           "--users", "20000", "--iterations", "200", "--heavy-iterations", "40",
                           "--repetitions", args.repetitions, "--out", output / "saas-operations.json"])
    for repetition in range(1, args.repetitions + 1):
        for transport in (("sidecar", "sqlite") if repetition % 2 else ("sqlite", "sidecar")):
            name = f"saas-r{repetition}-{transport}"
            add(name, [sys.executable, ROOT / "examples/saas_simulation/sweep.py", "--transport", transport,
                       "--levels", "10,100,500", "--duration", args.saas_duration, "--warmup", "5",
                       "--ramp", "5", "--think", "0", "--processes", "10", "--scenario", "baseline",
                       "--products", "5000", "--accounts", "20000", "--durability", "balanced",
                       "--seed", repetition, "--elitesql-bin", binaries["cli"], "--out", output / name],
                environment={"SAAS_SIM_DROP_OPS": "recommend"})
    return jobs


def report(output, jobs, metadata):
    relative = output.relative_to(ROOT).as_posix()
    link = lambda name: f"[{name}]({relative}/{name})"
    date = metadata["start"]["utc"][:10]
    text = [f"# EliteSQL vs SQLite — {date}", "",
            "Fresh measurements of the current EliteSQL source against SQLite. Every result below pairs both engines; no earlier EliteSQL builds or historical timings are included.", "",
            "## How to read the results", "",
            "Values are medians across independent fresh runs (see the recorded repetition count below). Brackets show the minimum–maximum across runs, not confidence intervals. **Ratio = SQLite time / EliteSQL time**, or equivalently **EliteSQL throughput / SQLite throughput**: above 1 favors EliteSQL; below 1 favors SQLite. Each p99 is the median of per-run p99 values, not a percentile of pooled requests.", "",
            "## Transactional load", "",
            "Normal transactions, deterministic narrow rows, Fast durability on EliteSQL and WAL/synchronous=OFF on SQLite. Total load includes ingestion, final checkpoint and maintenance drain. Neither configuration guarantees a durable sync per commit.", "",
            "| Rows | Rows/transaction | EliteSQL total s [range] | SQLite total s [range] | Ratio |",
            "| ---: | ---: | ---: | ---: | ---: |"]
    scale = []
    for job in jobs:
        if job["name"].startswith("scale-"):
            scale.extend(read_csv(output / (job["name"] + ".csv")))
    summary = {"scale": [], "writers": [], "saas": []}
    for rows in (1_000_000, 10_000_000):
        for batch in (1000, 10000):
            pair = {engine: [row for row in scale if int(row["rows"]) == rows and int(row["batch_size"]) == batch and row["engine"] == engine] for engine in ("EliteSQL", "SQLite")}
            load = {engine: [float(row["total_load_seconds"]) for row in values] for engine, values in pair.items()}
            ratio = statistics.median(load["SQLite"]) / statistics.median(load["EliteSQL"])
            text.append(f"| {rows:,} | {batch:,} | {cell(load['EliteSQL'])} | {cell(load['SQLite'])} | {ratio:.2f}× |")
            summary["scale"].append(dict(rows=rows, batch_size=batch, total_seconds=load, ratio=ratio))
    text += ["", f"Harness: [scale_vs_sqlite.rs](crates/elitesql-core/benches/scale_vs_sqlite.rs). Exact commands and raw CSVs: {link('jobs.json')}.", "",
             "## SQL point reads", "",
             "Both engines execute the same parameterized SELECT returning title, body and score. SQLite uses a prepared statement; EliteSQL uses query_params. Each run performs 100,000 warmed primary-key lookups after the transactional load. Storage-only Db::get timings and API-asymmetric scan timings are excluded.", "",
             "| Rows | Rows/transaction during load | EliteSQL µs/read [range] | SQLite µs/read [range] | Ratio |",
             "| ---: | ---: | ---: | ---: | ---: |"]
    for item in summary["scale"]:
        pair = {engine: [float(row["point_read_sql_us"]) for row in scale if int(row["rows"]) == item["rows"] and int(row["batch_size"]) == item["batch_size"] and row["engine"] == engine] for engine in ("EliteSQL", "SQLite")}
        ratio = statistics.median(pair["SQLite"]) / statistics.median(pair["EliteSQL"])
        text.append(f"| {item['rows']:,} | {item['batch_size']:,} | {cell(pair['EliteSQL'])} | {cell(pair['SQLite'])} | {ratio:.2f}× |")
        item.update(sql_point_us=pair, sql_point_ratio=ratio)
    text += ["", "## Concurrent writers", "",
             "Disjoint ids, ten rows per transaction, one/four/eight writers. Each engine writes 200,000 rows per run in Fast/Balanced and 10,000 in Safe. Final checkpoints are outside this throughput window. Fast maps to SQLite OFF; Balanced maps to NORMAL, with different loss-window contracts. Safe uses FULL plus fullfsync/checkpoint_fullfsync on this Mac and verifies F_FULLFSYNC on both engines.", "",
             "| Profile | Writers | EliteSQL rows/s [range] | SQLite rows/s [range] | Ratio | EliteSQL p99 ms | SQLite p99 ms |",
             "| --- | ---: | ---: | ---: | ---: | ---: | ---: |"]
    for mode in ("fast", "balanced", "safe"):
        values = read_csv(output / f"writers-{mode}.csv")
        engines = sorted({row["engine"] for row in values})
        elite = next(name for name in engines if name.lower().startswith("elite"))
        sqlite = next(name for name in engines if name.lower().startswith("sqlite"))
        for writers in (1, 4, 8):
            pair = {engine: [row for row in values if row["engine"] == engine and int(row["writers"]) == writers] for engine in (elite, sqlite)}
            rates = {engine: [float(row["rows_per_second"]) for row in entries] for engine, entries in pair.items()}
            tails = {engine: statistics.median(float(row["p99_us"]) / 1000 for row in entries) for engine, entries in pair.items()}
            ratio = statistics.median(rates[elite]) / statistics.median(rates[sqlite])
            text.append(f"| {mode.title()} | {writers} | {cell(rates[elite], 0)} | {cell(rates[sqlite], 0)} | {ratio:.2f}× | {tails[elite]:.3f} | {tails[sqlite]:.3f} |")
            summary["writers"].append(dict(profile=mode, writers=writers, throughput=rates, p99_ms=tails, ratio=ratio))
    text += ["", "Raw data: " + ", ".join(link(f"writers-{mode}.csv") for mode in ("fast", "balanced", "safe")) + ".", "",
             "## Mini-SaaS application", "",
             "The same ecommerce service, deterministic seed, 20,000 accounts and 5,000 products, baseline schema on both engines. Fifteen weighted operations cover catalogue browsing, product details, authentication, full-text search, carts, checkout, reviews, orders and administration. Recommendations are excluded from the comparison: EliteSQL's vector search and SQLite's category-based fallback answer different questions. Full-text search uses each engine's native implementation; this is an application comparison, not proof of identical ranking. See [the simulator](examples/saas_simulation/README.md).", "",
             "### Cost of one operation", "",
             "EliteSQL runs embedded through its Python binding; SQLite uses Python sqlite3. Independent account/cart state is prepared outside the timed intervals. There are 200 samples per operation/run, 40 for checkout and admin_dashboard. The underlying full-v2 run retains all sixteen operation measurements; the following weighted total removes recommend and normalizes the remaining weights to their sum.", "",
             "| Operation | EliteSQL µs [range] | SQLite µs [range] | Ratio |",
             "| --- | ---: | ---: | ---: |"]
    ops = json.loads((output / "saas-operations.json").read_text())
    weights = {name: weight for name, weight in ops["weights"].items() if name != "recommend"}
    total = {engine: sum(ops["median_us"][name][engine] * weight for name, weight in weights.items()) / sum(weights.values()) for engine in ("elite", "sqlite")}
    text.append(f"| **Weighted operation** | **{total['elite']:.2f}** | **{total['sqlite']:.2f}** | **{total['sqlite'] / total['elite']:.2f}×** |")
    for name in weights:
        pair = ops["raw_us"][name]
        ratio = statistics.median(pair["sqlite"]) / statistics.median(pair["elite"])
        text.append(f"| {name} | {cell(pair['elite'], 2)} | {cell(pair['sqlite'], 2)} | {ratio:.2f}× |")
    summary["saas_operations"] = dict(weights=weights, divisor=sum(weights.values()), weighted_us=total, ratio=total["sqlite"] / total["elite"])
    text += ["", f"Raw timings and operation weights: {link('saas-operations.json')}.", "",
             "### Concurrent requests", "",
             f"Closed loop, no think time, 10/100/500 virtual users, ten generator processes, up to one connection per user. Each level has a five-second ramp, five-second excluded warmup and {metadata['configuration']['saas_duration']:g} measured seconds. Every repetition starts a fresh database; it then accumulates across the three levels. Engine order alternates between repetitions. EliteSQL runs as a Unix-socket sidecar; SQLite is embedded in the generator processes, so these figures include different transport overheads.", "",
             "| Users | EliteSQL ops/s [range] | SQLite ops/s [range] | Ratio | EliteSQL p99 ms | SQLite p99 ms | EliteSQL success % | SQLite success % |",
             "| ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: |"]
    saas = {transport: [] for transport in ("sidecar", "sqlite")}
    checks = []
    for job in jobs:
        if job["name"].startswith("saas-r"):
            directory = output / job["name"]
            transport = job["name"].rsplit("-", 1)[-1]
            saas[transport].extend(read_csv(directory / "stages.csv"))
            checks.append(json.loads((directory / "check.json").read_text())["ok"])
    for users in (10, 100, 500):
        pair = {engine: [row for row in values if int(row["users"]) == users] for engine, values in saas.items()}
        rates = {engine: [float(row["throughput_ops_s"]) for row in values] for engine, values in pair.items()}
        tails = {engine: statistics.median(float(row["p99_ms"]) for row in values) for engine, values in pair.items()}
        success = {engine: min(float(row["success_rate_pct"]) for row in values) for engine, values in pair.items()}
        ratio = statistics.median(rates["sidecar"]) / statistics.median(rates["sqlite"])
        text.append(f"| {users} | {cell(rates['sidecar'], 0)} | {cell(rates['sqlite'], 0)} | {ratio:.2f}× | {tails['sidecar']:.3f} | {tails['sqlite']:.3f} | {success['sidecar']:.4f} | {success['sqlite']:.4f} |")
        summary["saas"].append(dict(users=users, throughput=rates, p99_ms=tails, minimum_success_pct=success, ratio=ratio))
    all_rows = [row for values in saas.values() for row in values]
    failures = sum(int(row["failed_ops"]) for row in all_rows)
    violations = sum(int(row["consistency_violations"]) for row in all_rows)
    invariants = all(row["invariants_ok"] == "True" for row in all_rows)
    summary["validation"] = dict(failed_ops=failures, consistency_violations=violations, invariants_ok=invariants, offline_checks_ok=all(checks))
    text += ["", f"Validation: {failures:,} failed operations; {violations:,} read-your-writes violations; business invariants {'passed' if invariants else 'FAILED'} at every level; offline integrity checks {'passed' if all(checks) else 'FAILED'} for both engines. Success columns show the lowest rate across repetitions. Conflicts, retries, individual operation p99, maximum latency and resource samples remain in each simulator run's report and stage files.", "",
             "Simulator runs: " + ", ".join(link(job["name"] + "/report.md") for job in jobs if job["name"].startswith("saas-r")) + ".", "",
             "## Environment and reproduction", "",
             f"- {metadata['cpu']['stdout']}, {metadata['logical_cpus']} logical CPUs, {int(metadata['memory']['stdout']) / 2**30:g} GiB RAM; {metadata['platform']}.",
             f"- {metadata['configuration']['repetitions']} repetitions per engine and workload.",
             f"- Source commit: `{metadata['sha']}`. Measured source/binary hashes and local changes: {link('metadata.json')}, {link('source.patch')}.",
             f"- {metadata['rustc']['stdout'].splitlines()[0]}; Python {metadata['python']}. SQLite versions are recorded in the Rust logs (bundled rusqlite) and Python metadata ({metadata['python_sqlite']}); these are separate benchmark suites, not one SQLite build.",
             "- Builds finished before measurement. Jobs ran sequentially on AC power; power/thermal observations are recorded at job boundaries. Caches were warmed or left to the OS; no disk-cold or cache-eviction claim is made.", "",
             "```bash", "# Choose an unused output directory.", "out=benchmark-results/sqlite-comparison-local", "mkdir -p \"$out\"",
             "cargo bench --locked -p elitesql-core --bench scale_vs_sqlite \\\n  --bench concurrent_writers --no-run --message-format=json > \"$out/build.jsonl\"",
             "cargo build --locked --release -p elitesql-ffi -p elitesql-cli",
             "python3 scripts/compare-sqlite.py --output \"$out\" \\\n  --build-json \"$out/build.jsonl\" --repetitions 3 --saas-duration 30", "```", "",
             f"The runner records every command, timestamp, exit status and resource log, verifies source and binary hashes, and regenerates this report and {link('summary.json')}. Use `--resume` only with unchanged sources, binaries and settings; `--summarize-only` regenerates the report from the completed run.", "",
             "## Limits", "",
             "One laptop, one narrow-row load fixture and one small SaaS catalogue. The SaaS windows characterize this local configuration, not maximum supported users or a production latency promise. The load, concurrent-write and SaaS suites have different transaction sizes, timing boundaries and durability settings; compare engines within a row. Throughput gains may coexist with worse latency tails. High concurrency can be limited by the Python generator, transport, checkpoints or scheduling. Per-run reports retain stalls, maximum latency, errors and retries so the medians do not conceal failures.", "",
             "The previous report, including comparisons between EliteSQL builds and historical diagnostics, is preserved as [old_benchmark.md](old_benchmark.md).", ""]
    (ROOT / "benchmark.md").write_text("\n".join(text))
    (output / "summary.json").write_text(json.dumps(summary, indent=2) + "\n")
    return summary["validation"]


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--output", type=Path, required=True)
    parser.add_argument("--build-json", type=Path, required=True)
    parser.add_argument("--repetitions", type=int, default=3)
    parser.add_argument("--saas-duration", type=float, default=30)
    parser.add_argument("--resume", action="store_true")
    parser.add_argument("--summarize-only", action="store_true")
    args = parser.parse_args()
    if args.repetitions < 1 or args.saas_duration <= 0:
        parser.error("repetitions and saas-duration must be positive")
    os.chdir(ROOT)
    output = args.output.resolve()
    output.relative_to(ROOT)
    output.mkdir(parents=True, exist_ok=True)
    binaries = {}
    for line in args.build_json.read_text().splitlines():
        item = json.loads(line)
        if item.get("reason") == "compiler-artifact" and item.get("executable") and "bench" in item["target"]["kind"]:
            binaries[item["target"]["name"]] = item["executable"]
    binaries.update(cli=str(ROOT / "target/release/elitesql"), ffi=str(ROOT / "target/release" / ("libelitesql.dylib" if platform.system() == "Darwin" else "libelitesql.so")))
    sources = {name: refresh.digest(ROOT / name) for name in source_files()}
    hashes = {name: refresh.digest(path) for name, path in binaries.items()}
    configuration = dict(repetitions=args.repetitions, saas_duration=args.saas_duration)
    metadata_path = output / "metadata.json"
    if args.resume or args.summarize_only:
        metadata = json.loads(metadata_path.read_text())
        if (metadata["source_sha256"], metadata["binary_sha256"], metadata["configuration"]) != (sources, hashes, configuration):
            raise RuntimeError("sources, binaries or configuration changed; use a new output directory")
    else:
        if metadata_path.exists():
            raise RuntimeError("output already contains a run; use --resume or a new directory")
        patch = subprocess.check_output(["git", "diff", "--binary", "HEAD", "--", "crates", "bindings", "scripts", ".github", "examples", "Cargo.toml", "Cargo.lock"])
        for name in sources:
            if subprocess.run(["git", "ls-files", "--error-unmatch", name], capture_output=True).returncode:
                result = subprocess.run(["git", "diff", "--no-index", "--binary", "/dev/null", name], capture_output=True)
                if result.returncode not in (0, 1):
                    raise RuntimeError(result.stderr.decode())
                patch += result.stdout
        (output / "source.patch").write_bytes(patch)
        metadata = dict(sha=refresh.capture(["git", "rev-parse", "HEAD"])["stdout"], source_sha256=sources,
                        binary_sha256=hashes, binary_paths=binaries, configuration=configuration,
                        patch_sha256=hashlib.sha256(patch).hexdigest(), platform=platform.platform(),
                        rustc=refresh.capture(["rustc", "-Vv"]), python=platform.python_version(),
                        cpu=refresh.capture(["sysctl", "-n", "machdep.cpu.brand_string"]),
                        memory=refresh.capture(["sysctl", "-n", "hw.memsize"]), logical_cpus=os.cpu_count(),
                        python_sqlite=sqlite3.sqlite_version, start=refresh.observations())
        metadata_path.write_text(json.dumps(metadata, indent=2) + "\n")
    jobs = jobs_for(output, binaries, args)
    (output / "jobs.json").write_text(json.dumps(jobs, indent=2) + "\n")
    env = {key: value for key, value in os.environ.items() if not key.startswith("ELITESQL_") and key != "SAAS_SIM_DROP_OPS"}
    env["ELITESQL_LIB"] = binaries["ffi"]
    for index, job in enumerate(jobs):
        status_path = output / f"{job['name']}.status.json"
        if (args.resume or args.summarize_only) and status_path.exists() and json.loads(status_path.read_text())["returncode"] == 0:
            continue
        if args.summarize_only:
            raise RuntimeError(f"missing successful job: {job['name']}")
        print(f"[{index + 1}/{len(jobs)}] {job['name']}", flush=True)
        status = dict(job, before=refresh.observations())
        started = time.monotonic()
        with (output / f"{job['name']}.log").open("w") as stdout, (output / f"{job['name']}.resources.txt").open("w") as stderr:
            result = subprocess.run(["/usr/bin/time", "-l" if platform.system() == "Darwin" else "-v", *job["argv"]],
                                    stdout=stdout, stderr=stderr, env={**env, **job.get("environment", {})})
        status.update(returncode=result.returncode, seconds=time.monotonic() - started, after=refresh.observations())
        status_path.write_text(json.dumps(status, indent=2) + "\n")
        if result.returncode:
            raise RuntimeError(f"{job['name']} failed; inspect its log and resources file")
    if sources != {name: refresh.digest(ROOT / name) for name in source_files()} or hashes != {name: refresh.digest(path) for name, path in binaries.items()}:
        raise RuntimeError("measured sources or binaries changed during the run")
    validation = report(output, jobs, metadata)
    metadata["end"] = refresh.observations()
    metadata_path.write_text(json.dumps(metadata, indent=2) + "\n")
    print(f"benchmark.md generated; validation: {validation}", flush=True)
    if validation["consistency_violations"] or not validation["invariants_ok"] or not validation["offline_checks_ok"]:
        raise SystemExit("SaaS correctness checks failed; report retains the failures")


if __name__ == "__main__":
    main()
