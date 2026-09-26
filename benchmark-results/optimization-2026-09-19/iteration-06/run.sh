#!/usr/bin/env bash
# Interleaved: base (92e0290), current working tree, SQLite; two rounds.
set -uo pipefail
B="$(cd "$(dirname "$0")" && pwd)"
cd /Users/jalbarracin/elitesql
for round in 1 2; do
  for variant in base current sqlite; do
    out="$B/run-$round-$variant"; rm -rf "$out"
    echo "=== round $round $variant $(date +%T)"
    if [ $variant = sqlite ]; then
      python3 examples/saas_simulation/sweep.py --transport sqlite --levels 10,100,500,1000,2000 --duration 30 --out "$out" 2>&1 | grep -E "^ *[0-9]+ |offline|error" 
    else
      python3 examples/saas_simulation/sweep.py --transport sidecar --elitesql-bin "$B/$variant-elitesql" --levels 10,100,500,1000,2000 --duration 30 --out "$out" 2>&1 | grep -E "^ *[0-9]+ |offline|error"
    fi
  done
done
