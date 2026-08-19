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

- **Hydrate micro-opts (KEPT)**: AHashSet for did/uri dedup; `resolve_profiles` returns `AHashMap<String, Arc<BlueskyProfile>>` via a single `get_user_profiles` pass; `hydrate_one` is sync with a single AHashMap get (merged contains_key+get); merged the two RecordView extraction passes into `extract_refs_from_view`; clippy/fmt clean.
- **record: serde_json::Value → simd_json::OwnedValue (KEPT)**: Parse builds OwnedValue straight off the simd-json tape; serialize faster. `RecordView` is a lens over `&OwnedValue` (same API via `simd_json::prelude`). Fixtures/tests use `simd_json::json!`.
- **Buffered parse (KEPT, big win)**: `parse_message` reuses a thread-local `simd_json::Buffers` via `to_tape_with_buffers` + `Tape::deserialize` instead of `from_str` (which re-allocated all scratch buffers per message). Parse dropped ~8.4ms → ~5.0ms per 10k.
- **ParseScratch (KEPT)**: buffers + reusable input `Vec<u8>` in ONE thread-local; eliminates the per-message heap String copy (~0.3ms) and removed the unsafe.
- **hydrate_one span hoist (KEPT)**: single `Span::current()` handle per message.
- **author_did redundancy (KEPT)**: MessageContext no longer stores a clone of `message.did`; hydrate_one borrows the did from the message for the lookup.
- **Borrowed-key profile map (KEPT, +4%)**: `resolve_profiles` returns `AHashMap<&str, Arc<BlueskyProfile>>` keyed by refs into the caller's `dids` Vec (no 10k String key clones).

## Current state (12 experiments kept)

- **~700-735k msgs/sec** (474k baseline → +48-55%). Confidence 13.6× noise floor. Load-dependent: machine load 2-4 modulates measurements ±15%.
- Phases (10k batch, quiet machine): parse ~4.8ms (tape floor), hydrate ~3.3ms, serialize ~5.2ms (bench `to_string` per-call alloc ~0.8ms locked; `to_writer` shared buffer measures 4.6ms but bench-inaccessible).

## Profiling notes (10k batch, post-optimizations)

- Parse ≈ 4.7-5.1ms: copy (to_string) ~0.4ms + tape+deser ~4.7ms (OwnedValue record build included).
- Hydrate ≈ 3.0-3.5ms: prepass ~1.1ms + resolve (moka get ×10k) ~0.7ms + per-message ~1.5ms.
- Serialize ≈ 5.3ms: message ~2.9ms + metadata ~1.8ms + ~0.8ms per-call String allocation (bench uses `simd_json::to_string`; measured `to_writer` w/ shared buffer at 4.6ms but bench API is fixed).
- serde_json serialize measured SLOWER (5.9-6.2ms) than simd_json (~5.3ms).
- simd-json has NO RawValue support and no raw-JSON emit hook (serialize_bytes → number array); raw-record-splice blocked.
- value_trait 0.10.1 has blanket impls so `.get()` works on OwnedValue with `simd_json::prelude` in scope.
- CI: `cargo clippy --all-targets --all-features -- -D warnings` + `cargo fmt --all -- --check`.

## Key Insight Notes (from initial code reading)

- `parse_message(&str)` does `text.to_string()` (full copy) then parses in place with simd_json. The production websocket loop already owns the `String`, so the copy is pure waste there AND in the bench (bench passes `&String`).
- `hydrate_batch` does TWO cache lookups per did: `check_user_profiles_cached` (contains_key, moka) during resolve_profiles, then `get_user_profile` (moka get) per message inside `hydrate_one`.
- Per-message allocations: `extract_did().to_string()`, empty Vecs for mentions/uris, HashSet insert of did clone, HashMap of post outcomes.
- Serialization: `simd_json::to_string` on `JetstreamMessage` (contains `Option<serde_json::Value>` record) + `HydratedMetadata` (contains full `BlueskyProfile`).
