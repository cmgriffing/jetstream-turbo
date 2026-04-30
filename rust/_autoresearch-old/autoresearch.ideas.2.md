# Ideas for Further Optimization of serde_json_serialize_message

## Completed
- Enums `MessageKind` and `OperationType` with `#[repr(u8)]` and custom `Serialize` impl: ~1.5% faster
- Added `Copy` to these enums: additional ~0.3%
- Added `#[serde(skip_serializing_if = "Option::is_none")` to optional fields: part of above

These combined yield ~3% improvement (from ~205 ns to ~198 ns).

## Potential but Intrusive: RawValue with lifetime handling
- Storing `record` as `Option<Arc<RawValue>>` gave ~16% gain in my experiment, but broke deserialization because `RawValue` requires borrowing and lifetime propagation across many types. It would be a major refactor to add lifetimes to `JetstreamMessage`, `EnrichedRecord`, and all functions that manipulate them.
- Alternatively, using `Box<RawValue>` and `#[serde(borrow)]` also requires lifetime parameters on structs. The improvement is substantial if we can make it work.

## Other smaller ideas
- Manual `Serialize` impl for `JetstreamMessage` and `CommitData` to avoid derive overhead (likely <1%).
- Use `serde_json::to_vec` with a pre-allocated buffer to avoid reallocations? Might not help.
- Reorder fields to put the most frequently present fields first to skip conditionals faster? Minimal.
