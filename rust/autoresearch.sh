#!/bin/bash
set -euo pipefail

# Run all pipeline benchmarks; capture full output
output=$(cargo bench --bench pipeline_benchmark -- --noplot 2>&1)

# Extract median time (in µs) for a given benchmark name from the output
extract_median_us() {
    local bench_id=$1
    # Find the line that contains the timing for this benchmark.
    # Two formats:
    #   bench_id  time:   [min µs median µs max µs]  (same line)
    #   bench_id (on its own line), then next indented line contains "time:"
    local time_line
    time_line=$(echo "$output" | awk -v bn="$bench_id" '
        $0 ~ "^" bn ".*time:" { print; exit }
        $0 == bn { getline; if ($0 ~ /time:/) { print; exit } }
    ')
    if [[ -z "$time_line" ]]; then
        echo "ERROR: Could not find timing line for $bench_id" >&2
        return 1
    fi
    # Extract all numbers from the line (they appear in order: min, median, max)
    # Then pick the second one (median).
    local median_str
    median_str=$(echo "$time_line" | grep -oE '[0-9]+(\.[0-9]*)?' | sed -n '2p')
    if [[ -z "$median_str" ]]; then
        echo "ERROR: Could not extract median value from: $time_line" >&2
        return 1
    fi
    # Ensure three decimal places
    printf "%.3f" "$median_str"
}

pipeline_µs=$(extract_median_us "full_pipeline_batch_25") || exit 1
hydrate_µs=$(extract_median_us "batch_hydration_25_messages") || exit 1

# Secondary metrics
batch_size=25

echo "METRIC pipeline_µs=$pipeline_µs"
echo "METRIC hydrate_µs=$hydrate_µs"
echo "METRIC batch_size=$batch_size"
