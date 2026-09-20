"""Paired, warmed browse-page timings with the same seed and SQL on both engines.

Records EXPLAIN, per-offset block timings and engine library identity. Run in
separate processes with ELITESQL_LIB pointing to preserved before/after builds.
The service's projection and ORDER BY are unchanged; equal-price ties need not
choose the same identities on both engines, so the oracle compares prices.
"""

import argparse
import hashlib
import json
import os
from pathlib import Path
import statistics
import tempfile
import time

from ops_cost import build
from saas import schema

SQL = (
    "SELECT id, name, price_cents, stock FROM products WHERE category = ? "
    "ORDER BY price_cents ASC LIMIT ? OFFSET ?"
)


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--products", type=int, default=5000)
    parser.add_argument("--iterations", type=int, default=500)
    parser.add_argument("--repetitions", type=int, default=5)
    parser.add_argument("--out", type=Path, required=True)
    args = parser.parse_args()
    if min(args.products, args.iterations, args.repetitions) < 1:
        parser.error("counts must be positive")
    library = Path(os.environ["ELITESQL_LIB"]).resolve()
    if not library.is_file():
        parser.error("ELITESQL_LIB must name the exact library file")
    offsets = [0, 20, 40, 60, 80, 200, 1000]
    results = {str(offset): {engine: [] for engine in ("elite", "sqlite")} for offset in offsets}
    plans = {}
    with tempfile.TemporaryDirectory(prefix="elitesql-browse-cost-") as temp:
        conns = {}
        try:
            for engine in ("elite", "sqlite"):
                conns[engine] = build(engine, Path(temp), 100, args.products, "compound-index")
                explain = "EXPLAIN " if engine == "elite" else "EXPLAIN QUERY PLAN "
                plans[engine] = conns[engine].execute(explain + SQL, [schema.CATEGORIES[0], 20, 40]).rows
            # Validate every tested category/page. Both stores contain exactly
            # the same seed, but the SQL intentionally leaves price ties open.
            for offset in offsets:
                for category in schema.CATEGORIES:
                    params = [category, 20, offset]
                    left = conns["elite"].execute(SQL, params).rows
                    right = conns["sqlite"].execute(SQL, params).rows
                    assert [r[2] for r in left] == [r[2] for r in right], (category, offset)
            for repetition in range(args.repetitions):
                for offset in offsets:
                    for engine in (("elite", "sqlite") if repetition % 2 == 0 else ("sqlite", "elite")):
                        conn = conns[engine]
                        for n in range(100):
                            conn.execute(SQL, [schema.CATEGORIES[n % len(schema.CATEGORIES)], 20, offset])
                        started = time.perf_counter_ns()
                        for n in range(args.iterations):
                            conn.execute(SQL, [schema.CATEGORIES[n % len(schema.CATEGORIES)], 20, offset])
                        elapsed = (time.perf_counter_ns() - started) / args.iterations / 1000
                        results[str(offset)][engine].append(elapsed)
        finally:
            for conn in conns.values():
                conn.close()
    report = {
        "products": args.products, "iterations": args.iterations,
        "repetitions": args.repetitions, "warmup_per_block": 100,
        "state": "checkpointed, no timed writes", "sql": SQL,
        "scenario": "compound-index", "library": str(library),
        "library_sha256": hashlib.sha256(library.read_bytes()).hexdigest(),
        "plans": plans, "raw_us": results,
    }
    args.out.parent.mkdir(parents=True, exist_ok=True)
    args.out.write_text(json.dumps(report, indent=2) + "\n")
    for offset, engines in results.items():
        print(offset, {engine: round(statistics.median(values), 2) for engine, values in engines.items()})


if __name__ == "__main__":
    main()
