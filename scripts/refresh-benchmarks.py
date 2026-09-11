#!/usr/bin/env python3
"""Run the full benchmark refresh sequentially using prebuilt executables.

Build first with cargo bench --no-run --message-format=json. Each job records
its argv, binary hash, environment observations, stdout, resources and exit
status. Resume skips only successful jobs and requires identical source/binaries.
"""
import argparse
import hashlib
import json
import os
from pathlib import Path
import platform
import re
import shutil
import subprocess
import time


def capture(argv):
    result = subprocess.run(argv, capture_output=True, text=True, timeout=15)
    return {"code": result.returncode, "stdout": result.stdout.strip(), "stderr": result.stderr.strip()}


def observations():
    result = {"utc": time.strftime("%Y-%m-%dT%H:%M:%SZ", time.gmtime())}
    if platform.system() == "Darwin":
        battery = capture(["pmset", "-g", "batt"])
        battery["stdout"] = re.sub(r"\(id=\d+\)", "", battery["stdout"])
        result.update(battery=battery, thermal=capture(["pmset", "-g", "therm"]))
    return result


def digest(path):
    return hashlib.sha256(Path(path).read_bytes()).hexdigest()


def matrix(output, binaries):
    jobs = []

    def add(name, bench, args, csv=True, **details):
        if csv:
            args += ["--csv", str(output / f"{name}.csv")]
        jobs.append(dict(name=name, bench=bench, argv=[binaries[bench], *args], **details))

    for repetition in range(1, 4):
        for rows in ["1m", "10m"]:
            for mode in ["txn", "bulk"]:
                for engine in (["elitesql", "sqlite"] if repetition % 2 else ["sqlite", "elitesql"]):
                    args = ["--rows", rows, "--batch-size", "10k", "--durability", "fast", "--point-reads", "10k", "--full-scans", "3", "--engine", engine]
                    if mode == "bulk":
                        args += ["--bulk-sorted"]
                    add(f"scale-{mode}-{rows}-r{repetition}-{engine}", "scale_vs_sqlite", args, repetition=repetition)
    for repetition in range(1, 6):
        for engine in (["elitesql", "sqlite"] if repetition % 2 else ["sqlite", "elitesql"]):
            add(f"sustained-r{repetition}-{engine}", "scale_vs_sqlite", ["--rows", "1m", "--batch-size", "1k", "--durability", "fast", "--point-reads", "100", "--full-scans", "1", "--engine", engine], repetition=repetition)
    for mode in ["fast", "balanced", "safe"]:
        args = ["--writers", "1,2,4,8,16", "--rows", "40k" if mode == "safe" else "200k", "--batch-size", "10", "--repetitions", "3", "--durability", mode]
        if mode == "safe":
            args += ["--sqlite-sync", "strict", "--safe-group-delay-us", "200"]
        add(f"writers-{mode}", "concurrent_writers", args)
    add("mixed", "concurrent_rw", ["--rows", "100k", "--read-operations", "1m", "--write-rows", "40k", "--batch-size", "10", "--readers", "1,2,4,8,16", "--writers", "0,1,4", "--repetitions", "3"])
    add("contention", "contention_matrix", ["--workloads", "insert,update,delete,identity,foreign-key,derived", "--cache", "warm,cold", "--readers", "16", "--writers", "4", "--rows", "50k", "--read-operations", "100k", "--write-rows", "5k", "--batch-size", "10", "--repetitions", "3"])
    add("wal-preallocation", "wal_preallocation", [str(output / "wal-preallocation.csv")], csv=False)
    for bench in ["vs_sqlite", "sql", "vector"]:
        for repetition in range(1, 2 if bench == "vs_sqlite" else 4):
            name = f"{bench}-r{repetition}"
            baseline = f"refresh-{output.name}-{name}"
            add(name, bench, ["--bench", "--save-baseline", baseline], csv=False, criterion_baseline=baseline, repetition=repetition)
    return jobs


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--build-json", type=Path, required=True)
    parser.add_argument("--output", type=Path, required=True)
    parser.add_argument("--resume", action="store_true")
    args = parser.parse_args()
    root = Path(__file__).resolve().parents[1]
    os.chdir(root)
    output = args.output.resolve()
    binaries = {}
    for line in args.build_json.read_text().splitlines():
        message = json.loads(line)
        if message.get("reason") == "compiler-artifact" and "bench" in message["target"]["kind"] and message.get("executable"):
            binaries[message["target"]["name"]] = message["executable"]
    jobs = matrix(output, binaries)
    files = subprocess.run(["git", "ls-files", "-z", "--cached", "--others", "--exclude-standard"], check=True, capture_output=True).stdout.split(b"\0")
    sources = {}
    for raw in files:
        if raw:
            path = Path(os.fsdecode(raw))
            if path.is_file() and (path.parts[0] in {"crates", "bindings", "scripts", ".github"} or path.name in {"Cargo.toml", "Cargo.lock"}):
                sources[str(path)] = digest(path)
    hashes = {name: digest(binary) for name, binary in binaries.items()}
    if args.resume:
        previous = json.loads((output / "metadata.json").read_text())
        if previous["source_sha256"] != sources or previous["binary_sha256"] != hashes:
            raise RuntimeError("source or binaries changed: create a new output directory")
    else:
        output.mkdir(parents=True, exist_ok=False)
        patch = subprocess.run(["git", "diff", "--binary", "HEAD"], check=True, capture_output=True).stdout
        for filename in capture(["git", "ls-files", "--others", "--exclude-standard"])["stdout"].splitlines():
            if filename in sources:
                diff = subprocess.run(["git", "diff", "--no-index", "--binary", "--", "/dev/null", filename], capture_output=True)
                if diff.returncode not in [0, 1]:
                    raise RuntimeError(diff.stderr.decode())
                patch += diff.stdout
        (output / "source.patch").write_bytes(patch)
        metadata = dict(sha=capture(["git", "rev-parse", "HEAD"])["stdout"], git_status=capture(["git", "status", "--short"])["stdout"], source_sha256=sources, binary_sha256=hashes, patch_sha256=hashlib.sha256(patch).hexdigest(), rustc=capture(["rustc", "-Vv"]), cargo=capture(["cargo", "-V"]), platform=platform.platform(), logical_cpus=os.cpu_count(), start=observations(), cache="No cache eviction claimed; cold label in contention means reopen, with actual eviction counters in CSV", allocator="Normal benchmark binaries; no allocation-counter wrapper", environment={key: value for key, value in os.environ.items() if key.startswith("ELITESQL_") or key in ["RUSTFLAGS", "CARGO_ENCODED_RUSTFLAGS", "CARGO_TARGET_DIR"]})
        if platform.system() == "Darwin":
            metadata.update(cpu=capture(["sysctl", "-n", "machdep.cpu.brand_string"]), memory=capture(["sysctl", "-n", "hw.memsize"]), os_build=capture(["sw_vers"]))
        (output / "metadata.json").write_text(json.dumps(metadata, indent=2) + "\n")
        shutil.copyfile(args.build_json, output / "build.jsonl")
    (output / "jobs.json").write_text(json.dumps(jobs, indent=2) + "\n")
    failed = []
    for index, job in enumerate(jobs):
        status_path = output / (job["name"] + ".status.json")
        if args.resume and status_path.exists() and json.loads(status_path.read_text()).get("returncode") == 0:
            continue
        print(f"[{index + 1}/{len(jobs)}] {job['name']}", flush=True)
        status = dict(job, before=observations())
        started = time.monotonic()
        with (output / (job["name"] + ".log")).open("w") as stdout, (output / (job["name"] + ".resources.txt")).open("w") as stderr:
            run = subprocess.run(["/usr/bin/time", "-l" if platform.system() == "Darwin" else "-v", *job["argv"]], stdout=stdout, stderr=stderr)
        status.update(returncode=run.returncode, wall_seconds=time.monotonic() - started, after=observations())
        if run.returncode == 0 and "criterion_baseline" in job:
            for source in (root / "target/criterion").rglob(job["criterion_baseline"]):
                if source.is_dir():
                    destination = output / "criterion" / job["name"] / source.relative_to(root / "target/criterion")
                    shutil.copytree(source, destination, dirs_exist_ok=True)
        status_path.write_text(json.dumps(status, indent=2) + "\n")
        if run.returncode:
            failed.append(job["name"])
            print(f"FAILED: {job['name']} (exit {run.returncode}); retaining logs", flush=True)
    print(f"Finished; failed jobs: {failed}", flush=True)
    raise SystemExit(bool(failed))


if __name__ == "__main__":
    main()
