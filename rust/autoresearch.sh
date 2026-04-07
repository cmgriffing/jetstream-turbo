#!/bin/bash
set -euo pipefail

bench_name="cache_user_profile_set"

output=$(cargo bench --bench hydration_benchmark "$bench_name" -- --noplot 2>&1)

# Extract the second number (median) from the time line, which looks like:
# "time:   [XXXXX ns YYYYY ns ZZZZZ ns]"
median_ns=$(echo "$output" | grep "time:" | head -n1 | grep -oE '[0-9]+(\.[0-9]+)?' | sed -n '2p')

if [[ -z "$median_ns" ]]; then
    echo "ERROR: Could not extract median timing for $bench_name" >&2
    echo "Full output:" >&2
    echo "$output" >&2
    exit 1
fi

echo "METRIC cache_set_ns=$median_ns"
