#!/usr/bin/env python3
"""Capture the exact dirty source used by the archived final binaries."""

from __future__ import annotations

import hashlib
import json
import os
import platform
import subprocess
from pathlib import Path


ROOT = Path(__file__).resolve().parents[2]
OUT = Path(__file__).resolve().parent
SOURCE_ROOTS = {"crates", "bindings", "scripts", ".github"}
ROOT_FILES = {"Cargo.toml", "Cargo.lock", "benchmark.md"}


def run(*args: str, check: bool = True) -> bytes:
    result = subprocess.run(args, cwd=ROOT, check=False, capture_output=True)
    if check and result.returncode:
        raise RuntimeError(result.stderr.decode(errors="replace"))
    return result.stdout


def digest(path: Path) -> str:
    value = hashlib.sha256()
    with path.open("rb") as handle:
        for chunk in iter(lambda: handle.read(1024 * 1024), b""):
            value.update(chunk)
    return value.hexdigest()


def selected(path: Path) -> bool:
    return path.name in ROOT_FILES or (path.parts and path.parts[0] in SOURCE_ROOTS)


def main() -> None:
    names = run("git", "ls-files", "-z", "--cached", "--others", "--exclude-standard")
    paths = [Path(os.fsdecode(raw)) for raw in names.split(b"\0") if raw]
    sources = {
        str(path): digest(ROOT / path)
        for path in paths
        if selected(path) and (ROOT / path).is_file()
    }

    patch = run("git", "diff", "--binary", "HEAD")
    untracked = run("git", "ls-files", "--others", "--exclude-standard").decode().splitlines()
    for name in untracked:
        path = Path(name)
        if name in sources:
            result = subprocess.run(
                ["git", "diff", "--no-index", "--binary", "--", "/dev/null", name],
                cwd=ROOT,
                check=False,
                capture_output=True,
            )
            if result.returncode not in (0, 1):
                raise RuntimeError(result.stderr.decode(errors="replace"))
            patch += result.stdout
    (OUT / "final-source.patch").write_bytes(patch)

    binaries = {}
    for name in ("scale_vs_sqlite", "transaction_ingest_alloc", "concurrent_writers", "sql"):
        path = OUT / "final-retained" / name
        binaries[str(path.relative_to(ROOT))] = digest(path)
    metadata = {
        "base_commit": run("git", "rev-parse", "HEAD").decode().strip(),
        "git_status": run("git", "status", "--short").decode(),
        "source_sha256": sources,
        "source_patch_sha256": hashlib.sha256(patch).hexdigest(),
        "binary_sha256": binaries,
        "rustc": run("rustc", "-Vv").decode(),
        "cargo": run("cargo", "-V").decode().strip(),
        "platform": platform.platform(),
        "logical_cpus": os.cpu_count(),
        "benchmark_allocator": "normal; allocation diagnostic is separate",
        "cache_policy": "no filesystem cache eviction claimed",
    }
    (OUT / "final-source.json").write_text(json.dumps(metadata, indent=2) + "\n")


if __name__ == "__main__":
    main()
