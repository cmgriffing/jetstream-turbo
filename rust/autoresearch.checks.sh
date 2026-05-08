#!/bin/bash
set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
cd "$SCRIPT_DIR"

echo "=== Running tests ==="
cargo test --features testing --workspace -- --test-threads=1

echo ""
echo "=== Running clippy ==="
cargo clippy -- -D warnings
