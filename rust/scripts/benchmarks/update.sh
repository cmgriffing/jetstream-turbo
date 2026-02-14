#!/bin/bash
set -e

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
PROJECT_ROOT="$(cd "$SCRIPT_DIR/../.." && pwd)"
BASELINE_DIR="$PROJECT_ROOT/benches/baselines"
BENCHMARK_OUTPUT="$PROJECT_ROOT/target/criterion"

echo "=========================================="
echo "Update Benchmark Baselines"
echo "=========================================="
echo ""

if [ ! -d "$BENCHMARK_OUTPUT" ]; then
    echo "Error: No benchmark results found. Run 'cargo bench' first."
    exit 1
fi

mkdir -p "$BASELINE_DIR"

for result_file in "$BENCHMARK_OUTPUT"/*/new/estimates.json; do
    if [ -f "$result_file" ]; then
        BENCH_NAME=$(basename "$(dirname "$result_file")")
        BASELINE_FILE="$BASELINE_DIR/${BENCH_NAME}.json"
        
        cp "$result_file" "$BASELINE_FILE"
        echo "[UPDATED] $BENCH_NAME baseline"
    fi
done

echo ""
echo "Baselines updated successfully!"
echo "Remember to commit the new baselines:"
echo "  git add benches/baselines/"
echo "  git commit -m 'Update benchmark baselines'"
