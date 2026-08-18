#!/bin/bash
set -e

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
PROJECT_ROOT="$(cd "$SCRIPT_DIR/../.." && pwd)"
BASELINE_DIR="${BASELINE_DIR:-$PROJECT_ROOT/benches/baselines}"
BENCHMARK_OUTPUT="${BENCHMARK_OUTPUT:-$PROJECT_ROOT/target/criterion}"
TIER1_THRESHOLD="${TIER1_THRESHOLD:-2.0}"
TIER2_THRESHOLD="${TIER2_THRESHOLD:-5.0}"
TIER3_THRESHOLD="${TIER3_THRESHOLD:-5.0}"

# Map a criterion benchmark name to its tier.
# Tier 1 = CPU hot-path microbenchmarks (cpu_hot_path.rs).
# Tier 3 = regression guards (regression.rs).
# Tier 2 (throughput) is compared separately by compare_throughput.sh.
tier_for() {
    case "$1" in
        parse_message_simd_json|parse_message_simd_json_owned|record_view_extract_refs|simd_json_serialize_record|extract_at_uri)
            echo 1 ;;
        *)
            echo 3 ;;
    esac
}

threshold_for() {
    case "$1" in
        1) echo "$TIER1_THRESHOLD" ;;
        2) echo "$TIER2_THRESHOLD" ;;
        3) echo "$TIER3_THRESHOLD" ;;
    esac
}

echo "=========================================="
echo "Benchmark Regression Check"
echo "=========================================="
echo ""

if [ ! -d "$BENCHMARK_OUTPUT" ]; then
    echo "Error: No benchmark results found. Run 'cargo bench' first."
    exit 1
fi

if [ ! -d "$BASELINE_DIR" ]; then
    echo "Warning: No baseline directory found at $BASELINE_DIR"
    echo "Set BASELINE_DIR to a main-branch criterion output to compare."
    exit 0
fi

FAILED=0
TOTAL_CHECKED=0

for bench_dir in "$BENCHMARK_OUTPUT"/*/; do
    BENCH_NAME=$(basename "$bench_dir")

    if [ "$BENCH_NAME" = "report" ]; then
        continue
    fi

    BASELINE_FILE="$BASELINE_DIR/${BENCH_NAME}.json"
    BASELINE_CRITERION_FILE="$BASELINE_DIR/${BENCH_NAME}/new/estimates.json"
    NEW_FILE="$bench_dir/new/estimates.json"

    if [ ! -f "$NEW_FILE" ]; then
        continue
    fi

    if [ ! -f "$BASELINE_FILE" ] && [ -f "$BASELINE_CRITERION_FILE" ]; then
        BASELINE_FILE="$BASELINE_CRITERION_FILE"
    fi

    if [ ! -f "$BASELINE_FILE" ]; then
        echo "[NEW] $BENCH_NAME - no baseline to compare"
        continue
    fi

    TOTAL_CHECKED=$((TOTAL_CHECKED + 1))

    BASELINE_MEAN=$(cat "$BASELINE_FILE" | python3 -c "import sys,json; d=json.load(sys.stdin); print(d.get('mean',{}).get('point_estimate',0))" 2>/dev/null || echo "0")
    NEW_MEAN=$(cat "$NEW_FILE" | python3 -c "import sys,json; d=json.load(sys.stdin); print(d.get('mean',{}).get('point_estimate',0))" 2>/dev/null || echo "0")

    if [ -z "$BASELINE_MEAN" ] || [ -z "$NEW_MEAN" ]; then
        echo "[SKIP] $BENCH_NAME - could not parse mean values"
        continue
    fi

    TIER=$(tier_for "$BENCH_NAME")
    THRESHOLD=$(threshold_for "$TIER")

    CHANGE_PCT=$(echo "scale=4; (($NEW_MEAN - $BASELINE_MEAN) / $BASELINE_MEAN) * 100" | bc)

    IS_REGRESSION=$(echo "$CHANGE_PCT > $THRESHOLD" | bc -l 2>/dev/null || echo "0")

    if [ "$IS_REGRESSION" = "1" ]; then
        echo "[REGRESSION] $BENCH_NAME (Tier $TIER): +${CHANGE_PCT}% (threshold: ${THRESHOLD}%)"
        FAILED=1
    else
        echo "[OK] $BENCH_NAME (Tier $TIER): ${CHANGE_PCT}%"
    fi
done

echo ""
echo "=========================================="
if [ $FAILED -eq 1 ]; then
    echo "FAILED: Benchmark regression detected!"
    exit 1
else
    if [ $TOTAL_CHECKED -eq 0 ]; then
        echo "No benchmarks to compare (first run?)"
        exit 0
    fi
    echo "PASSED: All benchmarks within threshold"
    exit 0
fi
