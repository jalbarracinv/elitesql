#!/usr/bin/env python3
"""Capture reproducible inputs and run review measurements sequentially.

Build both binaries first. --baseline-binary can point at the same example
built in an isolated checkout of the audited revision. No cache eviction is
claimed; repetitions alternate baseline/current on warm filesystem caches.
"""
import argparse
import hashlib
import json
import os
from pathlib import Path
import platform
import subprocess
import time


def command(args, **kwargs):
    return subprocess.run(args, check=True, capture_output=True, text=True, **kwargs).stdout.strip()


def main():
    parser = argparse.ArgumentParser()
    parser.add_argument("--output", type=Path, required=True)
    parser.add_argument("--binary", type=Path, default=Path("target/release/examples/audit_performance"))
    parser.add_argument("--baseline-binary", type=Path)
    parser.add_argument("--baseline-sha")
    parser.add_argument("--repetitions", type=int, default=3)
    parser.add_argument("--rows", type=int, default=20000)
    parser.add_argument("--queries", type=int, default=200)
    parser.add_argument("--power-note", required=True, help="Observed power/energy conditions; use unknown when unavailable")
    args = parser.parse_args()
    root = Path(__file__).resolve().parents[1]
    os.chdir(root)
    args.output.mkdir(parents=True, exist_ok=False)
    tracked = subprocess.run(["git", "ls-files", "-z", "--cached", "--others", "--exclude-standard"], check=True, capture_output=True).stdout.split(b"\0")
    source_files = {}
    for raw in tracked:
        if not raw:
            continue
        file = Path(os.fsdecode(raw))
        if file.is_file() and (file.parts[0] in {"crates", "bindings", "scripts", ".github"} or file.name in {"Cargo.toml", "Cargo.lock"}):
            source_files[str(file)] = hashlib.sha256(file.read_bytes()).hexdigest()
    patch = subprocess.run(["git", "diff", "--binary", "HEAD"], check=True, capture_output=True).stdout
    untracked = command(["git", "ls-files", "--others", "--exclude-standard"]).splitlines()
    for file in untracked:
        if file in source_files:
            result = subprocess.run(["git", "diff", "--no-index", "--binary", "--", "/dev/null", file], capture_output=True)
            if result.returncode not in (0, 1):
                raise RuntimeError(result.stderr.decode())
            patch += result.stdout
    (args.output / "source.patch").write_bytes(patch)
    binaries = {"current": args.binary.resolve()}
    if args.baseline_binary:
        binaries["baseline"] = args.baseline_binary.resolve()
    metadata = {
        "sha": command(["git", "rev-parse", "HEAD"]), "status": command(["git", "status", "--short"]),
        "source_sha256": source_files, "patch_sha256": hashlib.sha256(patch).hexdigest(),
        "binary_sha256": {name: hashlib.sha256(binary.read_bytes()).hexdigest() for name, binary in binaries.items()},
        "baseline_sha": args.baseline_sha, "rustc": command(["rustc", "-Vv"]),
        "platform": platform.platform(), "machine": platform.machine(), "logical_cpus": os.cpu_count(),
        "rows": args.rows, "queries": args.queries, "repetitions": args.repetitions,
        "durability": "fast", "query_working_bytes": 1048576,
        "power_note": args.power_note, "cache": "warm filesystem cache; new database for each process; no OS cache eviction",
        "allocation_counter": "global System allocator wrapper; counts all threads, and includes benchmark driver allocations",
        "query_seed": "(iteration * 7919 + rows / 2) % rows", "started_utc": time.strftime("%Y-%m-%dT%H:%M:%SZ", time.gmtime()),
    }
    if platform.system() == "Darwin":
        metadata["hardware"] = command(["sysctl", "-n", "machdep.cpu.brand_string"])
        metadata["physical_memory_bytes"] = command(["sysctl", "-n", "hw.memsize"])
    (args.output / "metadata.json").write_text(json.dumps(metadata, indent=2) + "\n")
    environment = dict(os.environ, ELITESQL_AUDIT_ROWS=str(args.rows), ELITESQL_AUDIT_QUERIES=str(args.queries))
    for repetition in range(args.repetitions):
        order = list(binaries.items())
        if repetition % 2:
            order.reverse()
        for name, binary in order:
            print(f"{name} repetition {repetition + 1}/{args.repetitions}", flush=True)
            with (args.output / f"{name}-{repetition + 1}.jsonl").open("w") as out, (args.output / f"{name}-{repetition + 1}.resources.txt").open("w") as resources:
                measured = ["/usr/bin/time", "-l" if platform.system() == "Darwin" else "-v", str(binary)]
                subprocess.run(measured, check=True, stdout=out, stderr=resources, env=environment)


if __name__ == "__main__":
    main()
