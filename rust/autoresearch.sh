#!/bin/bash
set -euo pipefail

run_one_bench() {
    local bench_name=$1
    local output
    output=$(cargo bench --bench hydration_benchmark "$bench_name" -- --noplot 2>&1)
    local median_ns
    median_ns=$(echo "$output" | grep "time:" | head -n1 | sed -n 's/.*\[\([0-9.]*\) ns \([0-9.]*\) ns \([0-9.]*\) ns\].*/\2/p')
    if [[ -z "$median_ns" ]]; then
        echo "ERROR: Could not extract median timing for $bench_name" >&2
        echo "Full output:" >&2
        echo "$output" >&2
        return 1
    fi
    echo "$median_ns"
}

SERIALIZE_BENCH="serde_json_serialize_message"
DESERIALIZE_BENCH="serde_json_deserialize_message"

serialize_ns=$(run_one_bench "$SERIALIZE_BENCH") || exit 1
deserialize_ns=$(run_one_bench "$DESERIALIZE_BENCH") || exit 1

# Output primary and secondary metrics. Primary must be first or explicitly matched by init_experiment's metric_name.
echo "METRIC serialize_ns=$serialize_ns"
echo "METRIC deserialize_ns=$deserialize_ns"
