#!/bin/bash
set -euo pipefail

bench_name="enriched_record_extract_uris"

output=$(cargo bench --bench hydration_benchmark "$bench_name" -- --noplot 2>&1)

# Extract median ns: look for line containing "time:" and a bracket with numbers.
median_ns=$(echo "$output" | grep -E "time:.*\[" | head -n1 | grep -oE '[0-9]+(\.[0-9]+)?' | sed -n '2p')

if [[ -z "$median_ns" ]]; then
    echo "ERROR: Could not extract median timing for $bench_name" >&2
    echo "Full output:" >&2
    echo "$output" >&2
    exit 1
fi

echo "METRIC total_ns=$median_ns"