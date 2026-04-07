#!/bin/bash
set -euo pipefail

BENCH_NAME="serde_json_serialize_message"

# Run benchmark with default format but skip generating plots for speed
output=$(cargo bench --bench hydration_benchmark "$BENCH_NAME" -- --noplot 2>&1)

# Extract the median time (the middle value in [min median max]) from the line after the benchmark name
median_ns=$(echo "$output" | grep -A1 " $BENCH_NAME" | tail -n1 | sed -n 's/.*\[\([0-9.]*\) ns \([0-9.]*\) ns \([0-9.]*\) ns\].*/\2/p')

if [[ -z "$median_ns" ]]; then
    echo "ERROR: Could not extract median timing for $BENCH_NAME" >&2
    echo "Full output:" >&2
    echo "$output" >&2
    exit 1
fi

echo "METRIC serialize_ns=$median_ns"
