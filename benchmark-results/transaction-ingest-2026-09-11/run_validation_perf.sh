#!/bin/sh
set -eu

repo=$(CDPATH= cd -- "$(dirname "$0")/../.." && pwd)
out="$repo/benchmark-results/transaction-ingest-2026-09-11/final-retained"
writers="$out/concurrent_writers"

"$writers" --writers 1,4 --rows 100000 --batch-size 10 \
    --repetitions 3 --durability fast --sqlite-sync ordinary \
    --csv "$out/writers-fast.csv" >"$out/writers-fast.stdout" 2>"$out/writers-fast.stderr"
"$writers" --writers 1,4 --rows 100000 --batch-size 10 \
    --repetitions 3 --durability balanced --sqlite-sync ordinary \
    --csv "$out/writers-balanced.csv" >"$out/writers-balanced.stdout" 2>"$out/writers-balanced.stderr"
"$writers" --writers 1,4 --rows 12000 --batch-size 10 \
    --repetitions 3 --durability safe --sqlite-sync strict \
    --safe-group-delay-us 200 --csv "$out/writers-safe.csv" \
    >"$out/writers-safe.stdout" 2>"$out/writers-safe.stderr"

for repetition in 1 2 3; do
    ELITESQL_BENCH_ROWS=1000000 "$out/sql" --bench \
        >"$out/sql-r$repetition.stdout" 2>"$out/sql-r$repetition.stderr"
done
