#!/bin/bash
set -euo pipefail
# Correctness gate: all unit tests must pass. Only errors surface.
OUT=$(cargo test --features testing 2>&1)
echo "$OUT" | grep -E '^error|FAILED|panicked|test result: FAILED|failures:' | head -40
echo "$OUT" | grep -q 'test result: ok' || { echo "TESTS FAILED"; exit 1; }
