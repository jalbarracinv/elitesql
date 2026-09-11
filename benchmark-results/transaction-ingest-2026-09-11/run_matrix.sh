#!/bin/sh
set -eu

repo=$(CDPATH= cd -- "$(dirname "$0")/../.." && pwd)
results="$repo/benchmark-results/transaction-ingest-2026-09-11"
out=${1:-"$results/final-formatted"}
initial="$results/instrumented-baseline/scale_vs_sqlite"
final=${FINAL_BINARY:-"$results/final-retained/scale_vs_sqlite"}

mkdir -p "$out"

run_one() {
    rows=$1
    batch=$2
    label=$3
    variant=$4
    repetition=$5

    case "$variant" in
        initial) binary=$initial; engine=elitesql ;;
        final) binary=$final; engine=elitesql ;;
        sqlite) binary=$final; engine=sqlite ;;
        *) echo "unknown variant: $variant" >&2; exit 2 ;;
    esac

    prefix="$out/$label-$variant-r$repetition"
    /usr/bin/time -l -o "$prefix.resources" \
        "$binary" \
        --rows "$rows" \
        --batch-size "$batch" \
        --point-reads 1000 \
        --full-scans 1 \
        --durability fast \
        --engine "$engine" \
        --csv "$prefix.csv" \
        >"$prefix.stdout" 2>&1
}

repetitions=${REPETITIONS:-"1 2 3 4 5"}
workloads=${WORKLOADS:-"1000000 1000 1m-b1k
1000000 10000 1m-b10k
10000000 1000 10m-b1k
10000000 10000 10m-b10k"}

for repetition in $repetitions; do
    case $(((repetition - 1) % 3)) in
        0) order="initial sqlite final" ;;
        1) order="final initial sqlite" ;;
        2) order="sqlite final initial" ;;
    esac

    echo "$workloads" | while IFS= read -r workload; do
        set -- $workload
        for variant in $order; do
            run_one "$1" "$2" "$3" "$variant" "$repetition"
        done
    done
done
