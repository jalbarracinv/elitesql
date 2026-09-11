#!/bin/sh
set -eu

repo=$(CDPATH= cd -- "$(dirname "$0")/../.." && pwd)
root="$repo/benchmark-results/transaction-ingest-2026-09-11"
out="$root/candidate-no-smallvec"
with_smallvec="$root/final-formatted/scale_vs_sqlite"
without_smallvec="$out/scale_vs_sqlite"

run_one() {
    rows=$1
    batch=$2
    label=$3
    variant=$4
    repetition=$5
    case "$variant" in
        with-smallvec) binary=$with_smallvec ;;
        no-smallvec) binary=$without_smallvec ;;
        *) exit 2 ;;
    esac
    prefix="$out/$label-$variant-r$repetition"
    /usr/bin/time -l -o "$prefix.resources" \
        "$binary" --rows "$rows" --batch-size "$batch" \
        --point-reads 1000 --full-scans 1 --durability fast \
        --engine elitesql --csv "$prefix.csv" >"$prefix.stdout" 2>&1
}

for repetition in 1 2 3 4 5; do
    if [ $((repetition % 2)) -eq 1 ]; then
        order="with-smallvec no-smallvec"
    else
        order="no-smallvec with-smallvec"
    fi
    for workload in "1000000 1000 1m-b1k" "1000000 10000 1m-b10k"; do
        set -- $workload
        for variant in $order; do
            run_one "$1" "$2" "$3" "$variant" "$repetition"
        done
    done
done

for repetition in 1 2 3; do
    if [ $((repetition % 2)) -eq 1 ]; then
        order="with-smallvec no-smallvec"
    else
        order="no-smallvec with-smallvec"
    fi
    for variant in $order; do
        run_one 10000000 10000 10m-b10k "$variant" "$repetition"
    done
done
