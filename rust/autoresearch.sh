#!/bin/bash
set -euo pipefail

# Pipeline benchmark runner for autoresearch
# Extracts timing metrics from cargo bench output (criterion)

run_benchmark_median() {
    local bench_name=$1
    local output
    # Run only the specified benchmark(s) to save time
    output=$(cargo bench --bench pipeline_benchmark "$bench_name" -- --noplot 2>&1)

    # Parse median time in microseconds from criterion output.
    # Expected pattern: after a line containing just the bench name, a line with "time:" and [min µs median µs max µs]
    local median
    median=$(echo "$output" | awk -v target="$bench_name" '
        $0 == target { found=1; next }
        found && /time:/ {
            if (match($0, /\[([0-9.]+) µs ([0-9.]+) µs ([0-9.]+) µs\]/, arr)) {
                print arr[2];
                exit
            }
        }
    ')

    if [[ -z "$median" ]]; then
        echo "ERROR: Could not extract median timing for $bench_name" >&2
        echo "Full output:" >&2
        echo "$output" >&2
        return 1
    fi
    echo "$median"
}

# Primary benchmark: full pipeline (hydrate + store + publish + broadcast) with batch size 25
PIPELINE_BENCH="full_pipeline_batch_25"
# Secondary: hydration only, same batch size
HYDRATE_BENCH="batch_hydration_25_messages"

pipeline_µs=$(run_benchmark_median "$PIPELINE_BENCH") || exit 1
hydrate_µs=$(run_benchmark_median "$HYDRATE_BENCH") || exit 1

# Secondary metrics
batch_size=25
# Cache hit rate is not directly reported by the benchmark; we could approximate or leave out for now.
# We'll set a placeholder or skip. For now, omit; can compute later if we instrument the benchmark.

# Output metrics. The primary metric must be first or named appropriately for init_experiment.
echo "METRIC pipeline_µs=$pipeline_µs"
echo "METRIC hydrate_µs=$hydrate_µs"
echo "METRIC batch_size=$batch_size"
