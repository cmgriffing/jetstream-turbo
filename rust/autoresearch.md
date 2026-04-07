# Autoresearch: Optimize serde_json_serialize_message

## Objective
Reduce the execution time of serializing a `JetstreamMessage` to JSON using serde_json. The benchmark `serde_json_serialize_message` measures the time to serialize a representative message containing commit data with various fields.

## Metrics
- **Primary**: `serialize_ns` (nanoseconds, lower is better) — median serialization time as measured by Criterion.
- **Secondary**: (none for now, but may add if we instrument e.g., allocation counts)

## How to Run
`./autoresearch.sh` — runs the benchmark and outputs `METRIC serialize_ns=<value>`.

## Files in Scope
- `src/models/jetstream.rs` — Defines `JetstreamMessage`, `CommitData`, `MessageKind`, `OperationType`. These structs and their serde attributes directly affect serialization performance.
- Possibly other model files if they are included in the message (but the current message only includes fields from jetstream.rs).

## Off Limits
- Do not change the JSON wire format expected by consumers (field names, enum values as strings, structure).
- Do not add new dependencies.
- Do not break existing tests.
- Do not change the benchmark workload (the message structure being serialized).

## Constraints
1. All tests must pass: `cargo test --features testing --workspace`.
2. No new dependencies allowed.
3. Serialization output must remain semantically identical (same JSON structure and values) to ensure compatibility.

## What's Been Tried
(Will be updated as experiments progress)

## Initial Observations
Baseline serialization time is around 200 ns. The data structure includes several `Option` fields (with `skip_serializing_if`) and custom enum serialization. Opportunities may include:
- Reducing allocations (e.g., using Cow, but may break API)
- Optimizing custom serialization for enums (though they already use fast path)
- Reducing redundant work in serialization (e.g., avoiding double formatting)
- Possibly enabling serde_json's `compact-format` features or using `serde_json::to_vec` vs `to_string`.
- Changing data layout to be more cache-friendly for the serializer.
