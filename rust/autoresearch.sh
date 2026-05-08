#!/bin/bash
set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
cd "$SCRIPT_DIR"

BENCH_NAME="enriched_record_new"

output=$(cargo bench --bench hydration_benchmark "$BENCH_NAME" -- --noplot 2>&1)

median_ns=$(echo "$output" | grep "time:" | head -n1 | sed -n 's/.*\[\([0-9.]*\) ns \([0-9.]*\) ns \([0-9.]*\) ns\].*/\2/p')

if [[ -z "$median_ns" ]]; then
    echo "ERROR: Could not extract median timing for $BENCH_NAME" >&2
    echo "Full output:" >&2
    echo "$output" >&2
    exit 1
fi

echo "$median_ns"
