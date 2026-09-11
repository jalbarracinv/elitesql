#!/bin/sh
set -eu

repo=$(CDPATH= cd -- "$(dirname "$0")/../.." && pwd)
root="$repo/benchmark-results/transaction-ingest-2026-09-11"
out="$root/final-retained"

for repetition in 1 2 3; do
    if [ $((repetition % 2)) -eq 1 ]; then
        order="initial final"
    else
        order="final initial"
    fi
    for variant in $order; do
        case "$variant" in
            initial) binary="$root/instrumented-baseline/transaction_ingest_alloc" ;;
            final) binary="$out/transaction_ingest_alloc" ;;
        esac
        prefix="$out/alloc-$variant-10m-b10k-r$repetition"
        ELITESQL_INGEST_ROWS=10000000 ELITESQL_INGEST_BATCH=10000 \
            /usr/bin/time -l -o "$prefix.resources" "$binary" \
            >"$prefix.jsonl" 2>&1
    done
done
