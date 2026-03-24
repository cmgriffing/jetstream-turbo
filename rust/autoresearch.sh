#!/bin/bash
set -euo pipefail

BENCH_NAME="full_pipeline_batch_25"

# Run benchmark with bencher output format; capture stdout only (stderr to /dev/null)
output=$(cargo bench --bench pipeline_benchmark "$BENCH_NAME" -- --output-format bencher 2>/dev/null)

# Extract the timing value (nanoseconds per iteration) from the line like:
# test full_pipeline_batch_25 ... bench:       94537 ns/iter (+/- 1106)
median_ns=$(echo "$output" | grep "test $BENCH_NAME " | head -n1 | awk '{print $5}')

if [[ -z "$median_ns" ]]; then
    echo "ERROR: Could not extract timing for $BENCH_NAME" >&2
    exit 1
fi

echo "METRIC total_ns=$median_ns"
