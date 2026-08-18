#!/bin/bash
set -e

# Compares the Tier 2 throughput (`msgs/sec`) between a main-branch run and a
# candidate run. A regression is a *decrease* in throughput beyond the threshold.

MAIN_FILE="${1:-$MAIN_THROUGHPUT_FILE}"
CAND_FILE="${2:-$CAND_THROUGHPUT_FILE}"
TIER2_THRESHOLD="${TIER2_THRESHOLD:-5.0}"

if [ -z "$MAIN_FILE" ] || [ -z "$CAND_FILE" ]; then
    echo "Usage: $0 <main-throughput-file> <candidate-throughput-file>"
    exit 2
fi

if [ ! -f "$MAIN_FILE" ]; then
    echo "Error: main throughput file not found: $MAIN_FILE"
    exit 1
fi

if [ ! -f "$CAND_FILE" ]; then
    echo "Error: candidate throughput file not found: $CAND_FILE"
    exit 1
fi

MAIN=$(grep '^msgs/sec:' "$MAIN_FILE" | tail -1 | awk '{print $2}')
CAND=$(grep '^msgs/sec:' "$CAND_FILE" | tail -1 | awk '{print $2}')

if [ -z "$MAIN" ] || [ -z "$CAND" ]; then
    echo "Error: could not parse msgs/sec from throughput output"
    exit 1
fi

echo "=========================================="
echo "Throughput Comparison (Tier 2)"
echo "=========================================="
echo "main:      $MAIN msgs/sec"
echo "candidate: $CAND msgs/sec"

CHANGE_PCT=$(python3 -c "print(round(($CAND - $MAIN) / $MAIN * 100, 4))")
echo "change:    ${CHANGE_PCT}%"

IS_REGRESSION=$(python3 -c "print(1 if $CHANGE_PCT < -$TIER2_THRESHOLD else 0)")

if [ "$IS_REGRESSION" = "1" ]; then
    echo "[REGRESSION] throughput: ${CHANGE_PCT}% (threshold: -${TIER2_THRESHOLD}%)"
    exit 1
else
    echo "[OK] throughput within threshold"
    exit 0
fi
