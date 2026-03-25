#!/bin/bash
set -euo pipefail

# Run test suite with testing feature enabled to ensure correctness after changes.
# Use a single test thread to reduce global state conflicts.
cargo test --features testing --workspace -- --test-threads=1
