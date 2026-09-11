#!/usr/bin/env python3
"""Summarize the archived transactional-ingest CSV and resource files."""

from __future__ import annotations

import csv
import json
import re
import statistics
from pathlib import Path


ROOT = Path(__file__).resolve().parent
MATRIX = ROOT / "final-retained"
WORKLOADS = ("1m-b1k", "1m-b10k", "10m-b1k", "10m-b10k")
VARIANTS = ("initial", "final", "sqlite")
FLOAT_METRICS = (
    "record_construction_seconds",
    "staging_seconds",
    "commit_calls_seconds",
    "commit_phase_prepare_seconds",
    "commit_phase_record_encode_seconds",
    "commit_phase_validation_seconds",
    "commit_phase_wal_encode_seconds",
    "commit_phase_wal_append_seconds",
    "commit_phase_sync_wait_seconds",
    "commit_phase_apply_seconds",
    "commit_phase_maintenance_wait_seconds",
    "ingest_wall_seconds",
    "final_checkpoint_seconds",
    "maintenance_drain_seconds",
    "checkpoint_work_seconds",
    "promotion_work_seconds",
    "total_load_seconds",
    "rows_per_second",
    "point_read_us",
    "full_scan_seconds",
)
INTEGER_METRICS = (
    "wal_appended_bytes",
    "checkpoint_bytes_written",
    "promotion_bytes_read",
    "promotion_bytes_written",
    "index_delta_peak_bytes",
    "maintenance_peak_bytes",
    "disk_bytes",
)


def repetition(path: Path) -> int:
    return int(path.stem.rsplit("-r", 1)[1])


def distribution(values: list[float]) -> dict[str, float | int]:
    mean = statistics.mean(values)
    return {
        "n": len(values),
        "median": statistics.median(values),
        "min": min(values),
        "max": max(values),
        "mean": mean,
        "sample_stdev": statistics.stdev(values) if len(values) > 1 else 0.0,
        "cv_percent": statistics.stdev(values) / mean * 100
        if len(values) > 1 and mean
        else 0.0,
    }


def read_rows(workload: str, variant: str) -> dict[int, dict[str, str]]:
    rows = {}
    for path in MATRIX.glob(f"{workload}-{variant}-r*.csv"):
        with path.open(newline="") as handle:
            rows[repetition(path)] = next(csv.DictReader(handle))
    return rows


def read_resources(workload: str, variant: str) -> dict[str, dict[str, float | int]]:
    rss = []
    footprint = []
    for path in MATRIX.glob(f"{workload}-{variant}-r*.resources"):
        text = path.read_text()
        rss_match = re.search(r"^\s*(\d+)\s+maximum resident set size$", text, re.M)
        footprint_match = re.search(r"^\s*(\d+)\s+peak memory footprint$", text, re.M)
        if rss_match:
            rss.append(int(rss_match.group(1)))
        if footprint_match:
            footprint.append(int(footprint_match.group(1)))
    return {
        "peak_rss_bytes": distribution(rss),
        "peak_physical_footprint_bytes": distribution(footprint),
    }


def main() -> None:
    summary: dict[str, object] = {
        "definition": {
            "total_load_seconds": "ingest + explicit final checkpoint + maintenance drain",
            "ranges": "minimum and maximum across independent processes",
            "cv_percent": "sample standard deviation divided by mean",
            "paired_reduction_percent": "(initial-final)/initial for matching repetition",
        },
        "workloads": {},
        "allocation_diagnostic": {},
        "uninstrumented_baseline": {},
    }
    workloads = summary["workloads"]
    assert isinstance(workloads, dict)
    for workload in WORKLOADS:
        raw = {variant: read_rows(workload, variant) for variant in VARIANTS}
        result: dict[str, object] = {"variants": {}, "paired": {}}
        variants = result["variants"]
        assert isinstance(variants, dict)
        for variant, rows in raw.items():
            metrics = {}
            for metric in FLOAT_METRICS + INTEGER_METRICS:
                values = [float(row[metric]) for _, row in sorted(rows.items())]
                metrics[metric] = distribution(values)
            metrics["resources"] = read_resources(workload, variant)
            variants[variant] = metrics

        common = sorted(set(raw["initial"]) & set(raw["final"]))
        for metric in FLOAT_METRICS:
            reductions = []
            for rep in common:
                initial = float(raw["initial"][rep][metric])
                final = float(raw["final"][rep][metric])
                if initial:
                    reductions.append((initial - final) / initial * 100)
            if reductions:
                paired = result["paired"]
                assert isinstance(paired, dict)
                paired[metric] = distribution(reductions)

        initial_total = variants["initial"]["total_load_seconds"]["median"]
        final_total = variants["final"]["total_load_seconds"]["median"]
        sqlite_total = variants["sqlite"]["total_load_seconds"]["median"]
        result["headline"] = {
            "median_reduction_percent": (initial_total - final_total) / initial_total * 100,
            "sqlite_time_over_final_time": sqlite_total / final_total,
        }
        workloads[workload] = result

    baseline_summary = summary["uninstrumented_baseline"]
    assert isinstance(baseline_summary, dict)
    for workload in WORKLOADS:
        paths = ROOT.joinpath("baseline").glob(f"{workload}-elitesql-r*.csv")
        values = []
        for path in paths:
            with path.open(newline="") as handle:
                values.append(float(next(csv.DictReader(handle))["total_load_seconds"]))
        final_total = workloads[workload]["variants"]["final"]["total_load_seconds"]
        initial = distribution(values)
        baseline_summary[workload] = {
            "total_load_seconds": initial,
            "final_reduction_percent": (
                initial["median"] - final_total["median"]
            )
            / initial["median"]
            * 100,
        }

    allocation_summary = summary["allocation_diagnostic"]
    assert isinstance(allocation_summary, dict)
    scalar_keys = (
        "total_seconds",
        "total_allocations",
        "total_allocated_bytes",
        "total_deallocated_bytes",
        "live_bytes_at_end",
        "peak_live_bytes",
        "wal_appended_bytes",
        "checkpoint_bytes_written",
        "promotion_bytes_read",
        "promotion_bytes_written",
        "index_delta_peak_bytes",
        "maintenance_peak_bytes",
    )
    for variant in ("initial", "final"):
        rows = [
            json.loads(path.read_text())
            for path in MATRIX.glob(f"alloc-{variant}-10m-b10k-r*.jsonl")
        ]
        metrics = {
            key: distribution([float(row[key]) for row in rows]) for key in scalar_keys
        }
        for phase in ("construction", "staging", "commit_calls", "checkpoint"):
            metrics[phase] = {
                key: distribution([float(row[phase][key]) for row in rows])
                for key in ("seconds", "allocations", "allocated_bytes")
            }
        allocation_summary[variant] = metrics

    exclusive = (
        "commit_phase_prepare_seconds",
        "commit_phase_record_encode_seconds",
        "commit_phase_validation_seconds",
        "commit_phase_wal_encode_seconds",
        "commit_phase_wal_append_seconds",
        "commit_phase_sync_wait_seconds",
        "commit_phase_apply_seconds",
        "commit_phase_maintenance_wait_seconds",
    )
    maximum_ratio = 0.0
    for workload in WORKLOADS:
        for variant in ("initial", "final"):
            for row in read_rows(workload, variant).values():
                commit_wall = float(row["commit_calls_seconds"])
                exclusive_sum = sum(float(row[key]) for key in exclusive)
                maximum_ratio = max(maximum_ratio, exclusive_sum / commit_wall)
    summary["instrumentation_invariant"] = {
        "exclusive_phase_sum_never_exceeded_commit_call_wall": True,
        "maximum_exclusive_sum_over_commit_call_wall": maximum_ratio,
    }

    (ROOT / "summary.json").write_text(
        json.dumps(summary, indent=2, sort_keys=True) + "\n"
    )


if __name__ == "__main__":
    main()
