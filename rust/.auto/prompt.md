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

## Current State (as of last run: 895.6k msgs/sec, +91% vs 468k baseline)

Phase costs per 10k batch (~11.7ms total at 895k):
- **parse ~5.4ms**: simd-json tape build ~2.8ms (floor) + envelope Strings ~0.7ms (did/rev/collection/rkey/cid) + **record DOM build ~1.6ms** (biggest parse item) + raw capture clone ~0.3ms
- **hydrate ~1.7-2.0ms**: extract_refs ~0.1ms + dedup slot-assign ~0.2ms + RwLock bulk get ~0.05ms + hydrate_one loop ~0.5ms
- **encode ~2.0ms**: message raw write ~0.1ms (wire-faithful) + metadata ~1.9ms (unique profile per message — structural)
- **drop ~1.7-1.9ms**: record DOM ~8 deallocs/msg + 4 envelope Strings + raw_json (allocator-bound)

## What's Been Tried (session log; details in .auto/log.jsonl + .auto/ideas.md)
- **[KEPT #16] Wire-faithful storage**: `JetstreamMessage.raw_json: Option<String>` captured at parse (1 clone before simd-json mutates input) + `write_json()` emitting original bytes at store time. Encode 4.4→2.0ms. Byte-faithful round-trip. +10-25%.
- **[KEPT #17] moka → RwLock<HashMap> caches**: moka get was 160-560ns; lazy TTL + FIFO eviction preserve semantics. Hydrate 4.4→2.0-2.6ms; all cache benches 2-6x faster. +5.5%.
- **[KEPT #18] Slot-indexed hydrate**: dedup assigns profile_slot per context; hydrate_one reads `profiles[slot]` — no per-message hash lookups, no profiles_by_did map. Hydrate 2.4→1.7ms; hydration benches best-yet (696ns / 8.0µs). +3.6%.
- Earlier keeps: shared Buffers parse, hydrate single-pass, ahash maps, OwnedValue record (BTreeMap→HashMap), MessageContext did dedup, owned ws-loop parse, one-buffer store serialize.
- Dead ends: native owned-DOM parse (1.7x slower), sqlx buffer reuse (borrows), raw-span capture via serde bridge (no API), Arc<str> raw_json (double copy).

## What's Left (see .auto/ideas.md for full design)
- **Record-DOM elimination (~+20-25%)**: PARKED — needs CommitData.record type change + redis/fixture raw-aware round-trip; high churn/risk.
- Envelope `rev`/`cid` skip: fields nothing reads but model/round-trip depend on them — skipped.
- Metadata encode, tape build: structural floors.
