#!/bin/bash
set -euo pipefail

bench_name="cache_user_profile_set"

output=$(cargo bench --bench hydration_benchmark "$bench_name" -- --noplot 2>&1)

# Extract median time in nanoseconds
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

echo "METRIC cache_set_ns=$median_ns"
