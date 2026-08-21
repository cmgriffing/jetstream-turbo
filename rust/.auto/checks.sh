#!/usr/bin/env bash
# Correctness gate: full test suite must pass.
set -euo pipefail

cd "$(dirname "$0")/.."

cargo test --all-targets --all-features 2>&1 | tail -60
