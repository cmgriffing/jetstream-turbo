#!/usr/bin/env bash
set -euo pipefail

export MEMORY_MEASUREMENT_SCOPE="${MEMORY_MEASUREMENT_SCOPE-process_rss}"
case "$MEMORY_MEASUREMENT_SCOPE" in
  process_rss|service_cgroup) ;;
  *)
    echo "invalid MEMORY_MEASUREMENT_SCOPE '$MEMORY_MEASUREMENT_SCOPE': expected process_rss or service_cgroup" >&2
    exit 2
    ;;
esac
export MEMORY_ARTIFACT_DIR="${MEMORY_ARTIFACT_DIR:-target/runtime-memory-artifacts}"
mkdir -p "$MEMORY_ARTIFACT_DIR"

# Reuse the collector-retention change's focused churn scenario first. The
# production suite owns the total-process phases and does not duplicate it.
cargo test --lib client::bluesky::tests::collector_ownership_settles_after_cache_capacity_and_ttl_churn -- --exact

if [[ "${MEMORY_SUITE_SCALE:-smoke}" == "production" ]]; then
  cargo test --test runtime_memory_suite production_scale_memory_suite -- --ignored --exact
else
  cargo test --test runtime_memory_suite production_shaped_memory_suite_smoke -- --exact
fi
