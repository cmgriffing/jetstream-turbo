#!/bin/bash
set -euo pipefail

bench_name="serde_json_serialize_enriched_record"

output=$(cargo bench --bench hydration_benchmark "$bench_name" -- --noplot 2>&1)

# Extract median ns
median_ns=$(echo "$output" | grep "time:" | head -n1 | grep -oE '[0-9]+(\.[0-9]+)?' | sed -n '2p')

if [[ -z "$median_ns" ]]; then
    echo "ERROR: Could not extract median timing for $bench_name" >&2
    echo "Full output:" >&2
    echo "$output" >&2
    exit 1
fi

echo "METRIC enriched_serialize_ns=$median_ns"
