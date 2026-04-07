#!/bin/bash
set -euo pipefail

BENCH_NAME="serde_json_serialize_message"

# Run benchmark with default format but skip generating plots for speed
output=$(cargo bench --bench hydration_benchmark "$BENCH_NAME" -- --noplot 2>&1)

# Extract the median time from the line containing "time:" (e.g., "time:   [200.00 ns 203.77 ns 208.73 ns]")
median_ns=$(echo "$output" | grep "time:" | head -n1 | sed -n 's/.*\[\([0-9.]*\) ns \([0-9.]*\) ns \([0-9.]*\) ns\].*/\2/p')

if [[ -z "$median_ns" ]]; then
    echo "ERROR: Could not extract median timing for $BENCH_NAME" >&2
    echo "Full output:" >&2
    echo "$output" >&2
    exit 1
fi

echo "METRIC serialize_ns=$median_ns"
