#!/bin/bash
set -euo pipefail

# Run SQLite batch store benchmark for batch size 100, extract median time in ms.
bench_name="sqlite_batch_sizes/store/100"

output=$(cargo bench --bench hydration_benchmark "$bench_name" -- --noplot 2>&1)

# Extract median time (ns) from the line containing "time:"
median_ns=$(echo "$output" | awk -v bn="$bench_name" '
    $0 ~ "^" bn ".*time:" {
        if (match($0, /\[([0-9.]+) ns ([0-9.]+) ns ([0-9.]+) ns\]/, arr)) {
            print arr[2]; exit
        }
    }
    $0 == bn { found=1; next }
    found && /time:/ {
        if (match($0, /\[([0-9.]+) ns ([0-9.]+) ns ([0-9.]+) ns\]/, arr)) {
            print arr[2]; exit
        }
    }
')

if [[ -z "$median_ns" ]]; then
    echo "ERROR: Could not extract median timing for $bench_name" >&2
    echo "Full output:" >&2
    echo "$output" >&2
    exit 1
fi

# Convert ns to ms with 3 decimal places
median_ms=$(awk -v ns="$median_ns" 'BEGIN {printf "%.3f", ns/1000000}')

echo "METRIC store_batch_ms=$median_ms"
