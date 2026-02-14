#!/bin/bash
set -e

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
PROJECT_ROOT="$(cd "$SCRIPT_DIR/../.." && pwd)"
BASELINE_DIR="$PROJECT_ROOT/benches/baselines"
BENCHMARK_OUTPUT="$PROJECT_ROOT/target/criterion"

REGRESSION_THRESHOLD=2.0

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
    echo "This appears to be the first run. Creating baseline..."
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
    NEW_FILE="$bench_dir/new/estimates.json"
    
    if [ ! -f "$NEW_FILE" ]; then
        continue
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
    
    CHANGE_PCT=$(echo "scale=4; (($NEW_MEAN - $BASELINE_MEAN) / $BASELINE_MEAN) * 100" | bc)
    
    IS_REGRESSION=$(echo "$CHANGE_PCT > $REGRESSION_THRESHOLD" | bc -l 2>/dev/null || echo "0")
    
    if [ "$IS_REGRESSION" = "1" ]; then
        echo "[REGRESSION] $BENCH_NAME: +${CHANGE_PCT}% (threshold: ${REGRESSION_THRESHOLD}%)"
        FAILED=1
    else
        echo "[OK] $BENCH_NAME: ${CHANGE_PCT}%"
    fi
done

echo ""
echo "=========================================="
if [ $FAILED -eq 1 ]; then
    echo "FAILED: Benchmark regression detected!"
    echo ""
    echo "To update baselines (if regression is intentional):"
    echo "  ./scripts/benchmarks/update.sh"
    exit 1
else
    if [ $TOTAL_CHECKED -eq 0 ]; then
        echo "No benchmarks to compare (first run?)"
        exit 0
    fi
    echo "PASSED: All benchmarks within threshold"
    exit 0
fi
