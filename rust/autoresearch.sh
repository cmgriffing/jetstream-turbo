#!/bin/bash
set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
cd "$SCRIPT_DIR"

BENCH_NAME="full_pipeline_batch_25"

output=$(cargo bench --bench pipeline_benchmark "$BENCH_NAME" -- --noplot 2>&1)
median_us=$(echo "$output" | grep "time:" | head -n1 | sed -n 's/.*\[\([0-9.]*\) µs \([0-9.]*\) µs \([0-9.]*\) µs\].*/\2/p')

median_us=$(echo "$output" | grep "time:" | head -n1 | sed -n 's/.*\[\([0-9.]*\) µs \([0-9.]*\) µs \([0-9.]*\) µs\].*/\2/p')

if [[ -z "$median_us" ]]; then
    echo "ERROR: Could not extract median timing for $BENCH_NAME" >&2
    echo "Full output:" >&2
    echo "$output" >&2
    exit 1
fi

echo "$median_us"
