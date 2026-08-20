#!/usr/bin/env bash
# Measure jetstream-turbo CPU throughput (primary) + hot-path/regression benches (secondary).
# Emits METRIC name=value lines; primary is throughput_msgs_per_sec (higher better).
set -euo pipefail

cd "$(dirname "$0")/.."

CRIT_ARGS=(--warm-up-time 0.5 --measurement-time 1 --sample-size 10)

emit_estimate() {
    local name="$1"
    local est="target/criterion/$name/new/estimates.json"
    local mean
    mean="$(python3 -c "import json;print(json.load(open('$est'))['mean']['point_estimate'])" 2>/dev/null || echo 0)"
    echo "METRIC ${name}_ns=$mean"
}

# ---------- Primary: throughput (msgs/sec, higher better) ----------
TP_OUT="$(cargo bench --bench cpu_throughput 2>&1)"
TP="$(printf '%s\n' "$TP_OUT" | sed -n 's/^msgs\/sec: //p' | tail -1)"
if [ -z "$TP" ]; then
    printf 'No msgs/sec line in throughput output:\n%s\n' "$TP_OUT" >&2
    exit 1
fi
echo "METRIC throughput_msgs_per_sec=$TP"

# ---------- Secondary: Tier 1 hot path ----------
cargo bench --bench cpu_hot_path -- "${CRIT_ARGS[@]}" >/dev/null 2>&1 || true
for b in parse_message_simd_json parse_message_simd_json_owned record_view_extract_refs \
         simd_json_serialize_record extract_at_uri; do
    emit_estimate "$b"
done

# ---------- Secondary: Tier 3 cache guards ----------
cargo bench --bench regression -- "${CRIT_ARGS[@]}" cache >/dev/null 2>&1 || true
for b in cache_user_profile_set cache_user_profile_get cache_post_set cache_post_get \
         cache_bulk_get_user_profiles cache_bulk_get_posts; do
    emit_estimate "$b"
done

# ---------- Secondary: hydration/pipeline guards (keep an eye when touching hydrator) ----------
cargo bench --bench regression -- "${CRIT_ARGS[@]}" hydration >/dev/null 2>&1 || true
for b in single_message_hydration batch_hydration_25_messages; do
    emit_estimate "$b"
done
