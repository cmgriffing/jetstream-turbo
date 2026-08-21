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

## Current State (as of last run: 993.2k msgs/sec median-of-3, direct runs ~1.02M; +112% vs 468k baseline)

Phase costs per 10k batch (~9.9ms total):
- **parse ~5.2ms**: simd-json tape build ~3.2ms (incl. per-message Tape Vec alloc — dep-internal) + envelope Strings ~0.6ms (did/rev/collection/rkey/cid; rev/cid feed SourceEventId) + RecordData walker ~0.15ms + raw capture clone ~0.3ms
- **hydrate ~1.8ms**: extract ~0.03ms + dedup slot-assign ~0.25ms + RwLock bulk get ~0.05ms + hydrate_one loop ~0.5ms + async/misc
- **encode ~1.9ms**: message raw write ~0.1ms + metadata ~1.86ms (profile serialize ~1.64ms at ~1.5GB/s — near structural floor)
- **drop ~0.9ms**: RecordData + envelope Strings + raw (halved by DOM removal)

## What's Been Tried (session log; details in .auto/log.jsonl + .auto/ideas.md)
- **[KEPT #16] Wire-faithful storage**: `JetstreamMessage.raw_json: Option<String>` captured at parse (1 clone before simd-json mutates input) + `write_json()` emitting original bytes at store time. Encode 4.4→2.0ms. Byte-faithful round-trip. +10-25%.
- **[KEPT #17] moka → RwLock<HashMap> caches**: moka get was 160-560ns; lazy TTL + FIFO eviction preserve semantics. Hydrate 4.4→2.0-2.6ms; all cache benches 2-6x faster. +5.5%.
- **[KEPT #18] Slot-indexed hydrate**: dedup assigns profile_slot per context; hydrate_one reads `profiles[slot]` — no per-message hash lookups. Hydrate 2.4→1.7ms; hydration benches best-yet. +3.6%.
- **[KEEP #19] Record-DOM elimination**: `CommitData.record` is now `RecordData` (text/reply/embed/facets) extracted leniently from the parser's tape via deserialize_any visitors (Cow keys, draining IgnoredAny skips) instead of a full OwnedValue HashMap. Storage keeps writing raw wire bytes; redis blob splices write_json+metadata; fixtures build wire JSON and parse; hot-path bench inputs are real wire forms. Parse benches 2180→1633ns, record_view 31→2.3ns, serialize 1269→516ns, hydration batch 8.0→5.9µs. Throughput 895.6k→993k. +10.9%.
- Earlier keeps: shared Buffers parse, hydrate single-pass, ahash maps, MessageContext did dedup, owned ws-loop parse, one-buffer store serialize.
- Dead ends: native owned-DOM parse (1.7x slower), sqlx buffer reuse (borrows), raw-span capture via serde bridge (no API), Arc<str> raw_json (double copy), `&str` walker keys (break serde_json), non-draining visitors (cursor desync).

## What's Left (see .auto/ideas.md for full details)
- All remaining phases are near their structural floors within the no-dependency-change / no-format-drift constraints (tape alloc is dep-internal; metadata profile write ~1.5GB/s; envelope strings feed SourceEventId).
