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
`./.auto/measure.sh` — emits `METRIC name=value` lines. Runtime ~75-90s (builds are incremental).
Checks: `./.auto/checks.sh` — runs `cargo test --all-targets --all-features` (192 tests; ~20-28s + compile).

## Files in Scope
- `src/client/jetstream.rs` — `parse_message` (simd-json), ws ingestion loop, raw-capture
- `src/hydration/hydrator.rs` — `hydrate_batch` / `hydrate_one`, slot-indexed profile resolution
- `src/hydration/resolver.rs` — batch cache-miss resolution
- `src/hydration/cache.rs` — `TurboCache` (RwLock<HashMap> user/post caches, Lru negative cache, metrics atomics)
- `src/models/record_view.rs` — zero-alloc JSON lens over `record: simd_json::OwnedValue`
- `src/models/jetstream.rs` — `JetstreamMessage` (incl. `raw_json` wire capture + `write_json`), `CommitData`, `extract_at_uri`, `extract_did`
- `src/models/enriched.rs` — `EnrichedRecord`, `HydratedMetadata` (serialized per message)
- `benches/cpu_throughput.rs` — the primary harness (phase timings allowed; msgs/sec computation stays identical)

## Off Limits
- No dependency changes or vendoring (AGENTS.md)
- No test-logic changes unless absolutely necessary
- Do not modify the measured workload in benches to inflate scores (no cheating/overfitting)

## Constraints
- `cargo test --all-targets --all-features` must pass (`.auto/checks.sh`)
- CI benchmark regression thresholds vs main: Tier1 2%, Tier2 5%, Tier3 5%
- Keep code simple; equal perf with less code = keep; ugly complexity for tiny gain = discard

## Current State (FINAL: 1,028,780 msgs/sec median-of-3, +120% vs 468k baseline; stable band 1.00-1.05M with the 27-sample protocol)

Phase costs per 10k batch (~9.6ms total) — ALL measured structural within the no-dep-change/no-format-drift constraints:
- **parse ~5.1ms**: simd-json tape work ~2.9ms + dep-internal tape `Vec<Node>` growth churn ~0.7ms (32 nodes/message, no public reuse API) + envelope Strings ~0.5-1ms (did/rev/collection/rkey/cid — all feed SourceEventId/extract_at_uri) + RecordData walker ~0.15ms + raw capture clone ~0.3ms
- **hydrate ~1.5ms**: single-pass extract+dedup slots ~0.13ms + RwLock bulk get ~0.15ms + hydrate_one loop ~0.7ms (struct construction + hydration_time_ms diagnostic clock)
- **encode ~1.9ms**: message raw write ~0.1ms + metadata ~1.86ms (12-field/13-key JSON structure + NEON escape scan — hand-rolled writer measured only 2% faster, structural)
- **drop ~0.9ms**: dealloc-bound

## What's Been Tried (FINAL session log; details in .auto/log.jsonl + .auto/ideas.md)
Session: 468k → 1,028,780 (+120%). Eight production-faithful wins, each committed and all 210 tests passing:
1. #16 Wire-faithful storage: raw_json captured at parse (1 clone) + write_json() emits original wire bytes at store time (byte-faithful). Encode 4.4→2.0ms.
2. #17 moka → RwLock<HashMap> caches (lazy TTL, FIFO eviction): hydrate 4.4→2.4ms; cache benches 2-6x faster.
3. #18 Slot-indexed hydrate: profile_slot per context — zero per-message hash lookups.
4. #19 Record-DOM elimination: CommitData.record → RecordData extracted leniently from the tape (Cow keys, draining IgnoredAny skips). Parse 2180→1600ns hot-path; hydration batch 8.0→5.9us.
5. #20-25 (convergence): metadata writer rejected (measured), tape reuse rejected (dep-internal), rev/cid skip rejected (source_event_id dedup index), TIMED_RUNS 5→9 (27-sample stable measurement), is_in_scope pre-split (production ws-loop), warning cleanup.

Dead ends (measured, do NOT retry — details in ideas.md): hand-rolled metadata writer (byte-loop AND NEON versions both no better than simd_json::to_writer), native owned-DOM parse (1.7x slower), sqlx buffer reuse (borrows), raw-span capture (no API), Arc<str> raw_json (double copy), &str walker keys (break serde_json), non-draining visitors (cursor desync), tape reuse (dep-internal), rev/cid skip (SourceEventId), raw-clone skip (dep mutation reliance), Instant removal (telemetry), arena serialization (wash).

## What's Left
- Nothing honest within the constraints. Every remaining cost has a concrete measurement showing why it is structural. If a constraint ever lifts: dep change (tape Vec reuse / allocator) or a storage-format change would open new wins (~5-10% each).
