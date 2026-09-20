"""Isolate the write operations that varied in full-v2, using its exact helpers."""
import argparse
import hashlib
import json
import os
from pathlib import Path
import statistics
import sys
import tempfile

ROOT = Path(__file__).resolve().parents[3]
sys.path.insert(0, str(ROOT / "examples/saas_simulation"))
from ops_cost import build, benchmark_operation, operation_definitions

parser = argparse.ArgumentParser(description=__doc__)
parser.add_argument("--out", type=Path, required=True)
args = parser.parse_args()
names = ["add_to_cart", "checkout"]
raw = {n: {e: [] for e in ["elite", "sqlite"]} for n in names}
definitions = operation_definitions()
for repetition in range(6):
    for name in names:
        with tempfile.TemporaryDirectory(prefix="elitesql-write-cost-") as temp:
            for engine in (["elite", "sqlite"] if repetition % 2 == 0 else ["sqlite", "elite"]):
                conn = build(engine, Path(temp), 20000, 5000, "compound-index")
                try:
                    raw[name][engine].append(benchmark_operation(conn, name, definitions[name], 5000, 200))
                finally:
                    conn.close()
library = Path(os.environ["ELITESQL_LIB"])
report = {"products": 5000, "users": 20000, "iterations": 200, "repetitions": 6,
          "scenario": "compound-index", "state": "fresh per operation/repetition; same preparation as full-v2",
          "library_sha256": hashlib.sha256(library.read_bytes()).hexdigest(), "raw_us": raw,
          "median_us": {n: {e: statistics.median(v) for e, v in engines.items()} for n, engines in raw.items()}}
args.out.write_text(json.dumps(report, indent=2) + "\n")
print(json.dumps(report["median_us"], indent=2))
