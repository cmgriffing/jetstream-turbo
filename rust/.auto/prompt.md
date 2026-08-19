# Autoresearch: cpu_throughput

## Objective

Optimize the end-to-end CPU throughput of the Jetstream message pipeline: **parse → hydrate (cache-hit) → serialize**. Measured by `benches/cpu_throughput.rs`, a standalone harness that processes a fixed batch of 10,000 realistic post messages (no I/O — fetchers/stores are mocks) and reports `msgs/sec`.

Current baseline: ~439k msgs/sec (~22.8ms per 10k batch of 3 timed runs, median reported).

## Metrics

- **Primary**: `msgs_per_sec` (msgs/sec, **higher** is better) — median across binary runs, median of 3 timed batches inside the bench.
- **Secondary**: `parse_ms`, `hydrate_ms`, `serialize_ms` — phase timings inside the batch (from instrumented analysis runs), plus bytes serialized.

## How to Run

```bash
./.auto/measure.sh   # builds benches (fast pre-check), runs the bench binary 3x, prints METRIC lines
```

Build is slow (~2.5 min cold, ~30-90s incremental) because `[profile.release]` has `lto = "fat"` + `codegen-units = 1`.

## Files in Scope

Core hot path (parse → hydrate → serialize):

- `src/client/jetstream.rs` — `parse_message(&str)` (copies input String, then `simd_json::from_str`); websocket ingest loop calls it per message.
- `src/hydration/hydrator.rs` — `hydrate_batch` pre-pass (RecordView extraction, dedup HashSets, cache resolution) + per-message `hydrate_one`.
- `src/hydration/cache.rs` — `TurboCache` (moka user/post caches, ahash).
- `src/hydration/resolver.rs` — `CacheMissResolver` (cache check → batch fetch → populate).
- `src/models/jetstream.rs` — `JetstreamMessage` / `CommitData` shapes; `record: Option<serde_json::Value>`.
- `src/models/enriched.rs` — `EnrichedRecord` / `HydratedMetadata` / `ProcessingMetrics` (serialized with simd_json in the bench).
- `src/models/record_view.rs` — zero-alloc lens over record JSON.
- `src/models/bluesky.rs` — `BlueskyProfile` (serialized inside HydratedMetadata).

## Off Limits

- `benches/` — the benchmark harnesses themselves. Do NOT change them to game the metric (no removing stages, no reducing batch size, no pre-baked outputs). Temporary local instrumentation for profiling must be reverted before logging results.
- `benches/baselines/` Criterion baselines (per AGENTS.md — never update from a feature branch).
- Correctness behavior: parse validation, hydration semantics, serialized payload shape.

## Constraints

- All existing tests must pass: `cargo test --features testing` (checked via `.auto/checks.sh` after each passing run).
- No new heavyweight dependencies. Minor std/alloc-only changes preferred. Small perf crates (e.g. hashbrown, compact_str) allowed only with clear wins and justification in the log.
- Keep the code idiomatic — ugly complexity for tiny gains gets discarded.

## What's Been Tried

- **Hydrate micro-opts (KEPT, +3.6-6%)**: AHashSet for did/uri dedup; `resolve_profiles` now returns a `HashMap<String, Arc<BlueskyProfile>>` via a single `get_user_profiles` pass (was contains_key + per-message moka get); `hydrate_one` became sync with HashMap lookups instead of per-message moka gets.
- **record: serde_json::Value → simd_json::OwnedValue (KEPT, +20% total)**: Parse builds OwnedValue straight off the simd-json tape (record tree ~1ms cheaper), serialize faster. `RecordView` is now a lens over `&OwnedValue` (same accessor API via `simd_json::prelude`). Fixtures/tests use `simd_json::json!`. Serialized record key ORDER changed (hash order vs sorted) — semantically irrelevant.

## Profiling notes (10k batch, post-optimizations)

- Parse ≈ 7.5-8.5ms: envelope tape ~6.4ms floor + OwnedValue record ~1ms + `text.to_string()` copy ~0.3ms.
- Hydrate ≈ 3.9ms: prepass ~1.2ms + resolve ~0.8ms + per-message ~1.9ms.
- Serialize ≈ 4.8ms: message ~2.9ms (OwnedValue record now) + metadata ~1.9ms.
- simd-json has NO RawValue support; raw-record-splice approach blocked (serde has no raw-JSON emit hook; simd-json serializer would escape it).
- value_trait 0.10.1 has blanket impls so `.get()` works on OwnedValue with `simd_json::prelude` in scope.

## Key Insight Notes (from initial code reading)

- `parse_message(&str)` does `text.to_string()` (full copy) then parses in place with simd_json. The production websocket loop already owns the `String`, so the copy is pure waste there AND in the bench (bench passes `&String`).
- `hydrate_batch` does TWO cache lookups per did: `check_user_profiles_cached` (contains_key, moka) during resolve_profiles, then `get_user_profile` (moka get) per message inside `hydrate_one`.
- Per-message allocations: `extract_did().to_string()`, empty Vecs for mentions/uris, HashSet insert of did clone, HashMap of post outcomes.
- Serialization: `simd_json::to_string` on `JetstreamMessage` (contains `Option<serde_json::Value>` record) + `HydratedMetadata` (contains full `BlueskyProfile`).
