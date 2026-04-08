# Autoresearch: EnrichedRecord Serialization

## Objective

Reduce the time to serialize an `EnrichedRecord` to JSON. The current `serde_json_serialize_enriched_record` benchmark reports ~604 ns median. We will try replacing `serde_json::to_string` with `simd_json::to_string` for the full `EnrichedRecord`, measuring any improvement.

## Metrics

- **Primary**: `enriched_serialize_ns` (nanoseconds, lower is better).
- **Secondary**: `test_status`.

## How to Run

`./autoresearch.sh` runs the `serde_json_serialize_enriched_record` benchmark (the benchmark name stays the same even if we switch the implementation). Output: `METRIC enriched_serialize_ns=<median_ns>`.

## Files in Scope

- `src/storage/redis.rs` — where `EnrichedRecord` is serialized for publishing.
- `src/models/enriched.rs` — the `EnrichedRecord` struct (ensure `simd_json` compatibility).
- `src/models/errors.rs` — error mapping.
- `autoresearch.sh`

## Off Limits

- None beyond correctness and tests passing.

## Constraints

- Tests must pass.
- Serialized JSON format must remain identical.
