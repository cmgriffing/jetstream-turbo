# Autoresearch: Jetstream Turbo Serialization Optimization

## Objective

Optimize the serialization performance of `JetstreamMessage` to JSON. The system processes high-volume Jetstream firehose data, and serialization speed directly impacts throughput and latency. Previous optimization sessions (see git history) improved serialization from ~205 ns to ~197 ns (~3.8% gain) via enum layout optimizations (`#[repr(u8)]`, `Copy`, `skip_serializing_if`). We've exhausted simple attribute-based tweaks. This session explores more advanced techniques:

- Custom serialization implementations
- Alternate serialization backends (simd-json, maybe others)
Serializer selection and feature flags
- Reducing allocation overhead
- Profiling-guided micro-optimizations

We may also branch out to related hot paths if promising:
- Deserialization performance
- EnrichedRecord serialization
- Cache hit rates via policy tuning

## Metrics

- **Primary**: `serialize_ns` (nanoseconds, lower is better) — median time to serialize a `JetstreamMessage` to JSON string
- **Secondary**: `deserialize_ns` (nanoseconds) — deserialization time (for completeness)
- **Secondary**: `test_status` (pass/fail) — all tests must pass

## How to Run

`./autoresearch.sh` — runs the benchmark and outputs structured `METRIC` lines:

```
METRIC serialize_ns=<median_ns>
METRIC deserialize_ns=<median_ns> (if applicable)
```

The script uses `cargo bench` with the `hydration_benchmark` suite and extracts the median timing.

## Files in Scope

- `src/models/jetstream.rs` — `JetstreamMessage` struct and related types (MessageKind, OperationType, CommitData). This is the primary serialization target.
- `src/models/errors.rs` — error types used in serialization paths
- `src/models/enriched.rs` — `EnrichedRecord` may also be a candidate if we expand scope
- `Cargo.toml` — to adjust features or add optional dependencies (e.g., switch to simd-json via feature flags)
- `autoresearch.sh` — the experiment runner we will modify to test different configurations

## Off Limits

- Do NOT break serialization correctness: deserialized data must match original.
- Do NOT change public API contracts (AtProto data formats) — the JSON structure must remain compatible.
- Do NOT introduce new runtime dependencies without discussion (but enabling existing optional features is fine).
- Do NOT degrade memory safety or error handling.

## Constraints

- All tests must pass: `cargo test --features testing --workspace -- --test-threads=1`
- No new dependencies beyond what's already in Cargo.toml (simd-json is already present).
- Changes should be reversible and well-documented in this file's "What's Been Tried".

## What's Been Tried

From previous autoresearch sessions (see git logs and `_autoresearch-old/`):

1. **Enum layout optimization** (git 14330ff): Added `#[repr(u8)]` to `MessageKind` and `OperationType`. Result: 1.2% improvement (199.84 → 197.44 ns).
2. **Derive Copy for enums** (git dc36348): Added `Copy` trait to those enums. Result: 0.3% improvement (197.44 → 196.81 ns).
3. **Conditional skip_serializing_if** (git 2920dec): Used `#[serde(skip_serializing_if = "Option::is_none")]` on optional fields. Result: brought performance to ~197 ns.
4. **Verification runs** (git 7741842): Confirmed stable at ~197 ns. No further improvements with simple attribute changes.

**Current baseline**: ~197 ns median for serde_json serialization of a typical `JetstreamMessage`.

**Potential next steps** (to be explored in this session):
- Switch to `simd-json` for serialization (requires custom Serialize impl or different API usage).
- Use `serde_json::to_vec` instead of `to_string` to avoid UTF-8 validation overhead if possible.
- Reduce struct size by making fields `#[serde(skip)]` where appropriate (e.g., metadata not needed in output).
- Use `#[serde(borrow)]` to eliminate cloning of owned strings where borrowed data works.
- Pre-allocate buffers or reuse serializers across calls.
- Explore `ryu` for numeric serialization if numbers are common (likely not the bottleneck).
- Consider using a different JSON library altogether (e.g., `json_iter`?), but that would require new dependencies.

We'll start by establishing a clean baseline from this branch, then systematically try these approaches, recording results in `autoresearch.jsonl`.
