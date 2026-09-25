#!/usr/bin/env bash
# Interleaved A/B of saved binaries: before, after, before, after.
set -euo pipefail
AB="$(cd "$(dirname "$0")" && pwd)"
cd /Users/jalbarracin/elitesql
for round in 1 2; do
  for variant in before after; do
    out="$AB/run-$round-$variant"
    rm -rf "$out"
    echo "=== round $round $variant $(date +%T)"
    python3 examples/saas_simulation/sweep.py --transport sidecar \
      --elitesql-bin "$AB/$variant-elitesql" --levels 10,100,500 \
      --duration 20 --out "$out" 2>&1 | tail -12
  done
done
