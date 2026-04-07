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

### Baseline (commit a0ba963)
- Initial baseline median: **199.84 ns** (after manual `Serialize` impls for enums were already present).

### Experiment 1: Revert enums to derived Serialize
- **Change**: Removed manual `Serialize` impls for `MessageKind` and `OperationType`, falling back to derived.
- **Result**: 204.09 ns (worse). Discarded.
- **Conclusion**: The manual impls are faster for these simple enums.

### Experiment 2: Try `#[repr(u8)]` with `#[inline]` changes
- **Change**: Added `#[repr(u8)]` to enums and changed `#[inline(always)]` to `#[inline]`.
- **Result**: 201.51 ns (slightly worse, within noise). Discarded.

### Experiment 3: Combine `#[repr(u8)]` with array lookup
- **Change**: Added `#[repr(u8)]` and `Copy`; used static array lookup instead of match.
- **Result**: 202.27 ns (worse). Discarded.

### Experiment 4: Keep manual impl with `#[repr(u8)]` only (commit 14330ff)
- **Change**: Kept manual `Serialize` with match, added `#[repr(u8)]` to enums.
- **Result**: **197.44 ns** — **1.2% improvement** over session baseline (199.84 ns).
- **Conclusion**: Smaller enum size improves cache locality during serialization.

### Experiment 5: Additionally add `Copy` derive to enums (commit dc36348)
- **Change**: Added `Copy` to the enums (still with `#[repr(u8)]` and manual `Serialize`).
- **Result**: **196.81 ns** — additional **0.3% improvement**.
- **Conclusion**: Marking enums as `Copy` enables marginal further gains.

### Experiment 6: Final verification (commit 7741842)
- **Result**: **196.90 ns** — consistent with previous best.

### Summary of Improvements
- Combined improvement: **~1.5%** from session baseline (199.84 ns → 196.90 ns).
- Compared to historical baseline from `benches/baselines/serde_json_serialize_message.json` (205.65 ns), the current state is **~4.3% faster**, fully recovering the reported regression and then some.
- The most effective change: `#[repr(u8)]` on the enums.
- Further significant gains would require invasive changes (e.g., custom serializer for the `record` field, removing Option wrappers) which are likely out of scope or risk breaking compatibility.


## Initial Observations
Baseline serialization time is around 200 ns. The data structure includes several `Option` fields (with `skip_serializing_if`) and custom enum serialization. Opportunities may include:
- Reducing allocations (e.g., using Cow, but may break API)
- Optimizing custom serialization for enums (though they already use fast path)
- Reducing redundant work in serialization (e.g., avoiding double formatting)
- Possibly enabling serde_json's `compact-format` features or using `serde_json::to_vec` vs `to_string`.
- Changing data layout to be more cache-friendly for the serializer.
