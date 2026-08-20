# Autoresearch: Optimize cpu_throughput (msgs/sec) without regressing other benchmarks

## Objective
Optimize the Tier-2 end-to-end CPU throughput benchmark (`benches/cpu_throughput.rs`): parse → hydrate (all cache hits) → serialize over a 10,000-message batch, with fetchers/stores mocked so only CPU stages are measured. Primary goal is more `msgs/sec`. Must NOT regress the Tier-1 hot-path microbenchmarks (esp. `extract_at_uri`, `parse_message_simd_json`) or Tier-3 regression guards (esp. `cache_user_profile_set` and the other cache/hydration/sqlite benches) beyond CI thresholds.

## Metrics
- **Primary**: `throughput_msgs_per_sec` (msgs/sec, higher is better) — median of 3 timed batches of 10k messages.
- **Secondary** (all criterion estimates in ns, lower is better):
  - Hot path: `parse_message_simd_json`, `parse_message_simd_json_owned`, `record_view_extract_refs`, `simd_json_serialize_record`, `extract_at_uri`
  - Regression: `cache_user_profile_set`, `cache_user_profile_get`, `cache_post_set`, `cache_post_get`, `cache_bulk_get_user_profiles`, `cache_bulk_get_posts`

CI gates (vs main baseline): Tier1 benches ≤ 2% regression, Tier3 ≤ 5%, throughput ≤ 5% decrease. Thresholds: TIER1=2.0, TIER2=5.0, TIER3=5.0.

## How to Run
`./.auto/measure.sh` — emits `METRIC name=value` lines. Runtime ~60-90s (builds are incremental).
Checks: `./.auto/checks.sh` — runs `cargo test --all-targets --all-features` (190 unit tests; ~25s + compile).

## Files in Scope
- `src/client/jetstream.rs` — `parse_message` (simd-json), ws ingestion loop
- `src/hydration/hydrator.rs` — `hydrate_batch` / `hydrate_one`, MessageContext, RecordView extraction
- `src/hydration/resolver.rs` — batch cache-miss resolution
- `src/hydration/cache.rs` — `TurboCache` (moka user/post caches, metrics atomics, negative cache)
- `src/models/record_view.rs` — zero-alloc JSON lens over `record: serde_json::Value`
- `src/models/jetstream.rs` — `JetstreamMessage`/`CommitData`, `extract_at_uri`, `extract_did`
- `src/models/enriched.rs` — `EnrichedRecord`, `HydratedMetadata` (serialized per message)
- `benches/cpu_throughput.rs` — the primary harness (phase timings are allowed; msgs/sec computation stays identical)

## Off Limits
- No dependency changes or vendoring (AGENTS.md)
- No test-logic changes unless absolutely necessary
- Do not modify the measured workload in benches to inflate scores (no cheating/overfitting)

## Constraints
- `cargo test --all-targets --all-features` must pass (`.auto/checks.sh`)
- CI benchmark regression thresholds vs main: Tier1 2%, Tier2 5%, Tier3 5%
- Keep code simple; equal perf with less code = keep; ugly complexity for tiny gain = discard

## What's Been Tried
- (initial baseline) throughput ≈ 500k msgs/sec. Hot path: parse_message_simd_json ≈ 2.35 µs, extract_at_uri ≈ 17.3 ns, record_view_extract_refs ≈ 120 ns, simd_json_serialize_record ≈ 1.28 µs. cache_user_profile_set ≈ 470 µs, cache_user_profile_get ≈ 70 µs, cache_post_set ≈ 580 µs, cache_post_get ≈ 68 µs, bulk profile get ≈ 4.30 µs, bulk post get ≈ 4.36 µs.

## Session notes
- Bench binaries run directly via `target/release/deps/<name>-<hash>` are smoke tests only for criterion benches; must use `cargo bench --bench <name>` to get real measurements. cpu_throughput is a plain main() so direct runs are fine.
- Throughput bench fixtures are simple posts (no reply/embed/facets), so RecordView extraction is cheap; hydration is all cache-hits.
