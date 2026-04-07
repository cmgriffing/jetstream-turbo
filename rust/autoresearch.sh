#!/bin/bash
set -euo pipefail

bench_name="sqlite_batch_sizes/store/100"

output=$(cargo bench --bench hydration_benchmark "$bench_name" -- --noplot 2>&1)

# Extract the median value (second number) from the time line.
# Example line: "                        time:   [9.8206 ms 10.210 ms 10.618 ms]"
median=$(echo "$output" | grep "time:" | head -n1 | grep -oE '[0-9]+(\.[0-9]+)?' | sed -n '2p')

if [[ -z "$median" ]]; then
    echo "ERROR: Could not extract median timing for $bench_name" >&2
    echo "Full output:" >&2
    echo "$output" >&2
    exit 1
fi

# The value is already in milliseconds as printed by criterion.
# Output as METRIC with unit ms.
echo "METRIC store_batch_ms=$median"
