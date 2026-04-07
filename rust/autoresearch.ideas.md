# Ideas for serialization optimization

## Background
Baseline: ~202 ns for serde_json serialization of JetstreamMessage using serde_json 1.0.

Previous session (git history) optimized enums and skip_serializing_if, gaining ~3.8% from 205 ns to 197 ns. Current baseline is slightly higher at 202.51 ns (noise).

## Explored in previous sessions
- `#[repr(u8)]` on enums
- `Copy` trait on enums
- `#[serde(skip_serializing_if = "Option::is_none")]` on optional fields
- Custom Serialize for enums (string representation)

## Potential next steps

### 1. Easy/medium changes (low risk)
- **Manual vs derived serialization for enums**: Replace manual Serialize with derived (benchmark if any diff). Might be neutral.
- **Switch to `serde_json::to_vec`**: Benchmark if avoiding the final UTF-8 conversion (via `String::from_utf8`) helps. Likely negligible but trivial to test.
- **Add `#[inline(always)]` to `JetstreamMessage::serialize`** if we write custom. Derived may not inline.
- **Use `#[serde(borrow)]`** on fields to reduce cloning during deserialization (secondary metric improvement).
- **Replace `String` with `Box<str>`** in structs to reduce allocation size; serialization might still be same.

### 2. Advanced changes (higher risk, higher reward)
- **Custom serializer for `JetstreamMessage`**: Implement `Serialize` manually to potentially skip some serde overhead. Might yield 5-10% if carefully written.
- **Avoid double-serialization of `record` field**: Currently the record is stored as `serde_json::Value` and re-serialized when storing to DB/Redis. Keep original raw JSON bytes alongside the Value and use them during re-serialization to avoid the reserialize step. Could significantly reduce CPU for messages with large records.
- **Switch deserialization to `simd-json`**: Use `simd-json` for parsing incoming messages. This would improve deserialization time (secondary metric), not serialization directly, but overall throughput gain. Requires feature flag or separate code path.
- **Change `record` representation**: Instead of `serde_json::Value`, parse into a typed struct with only needed fields, and store raw JSON separately. Could reduce memory and parsing time. Big change.
- **Cache pragma tuning**: Experiment with different cache sizes and TTL for `TurboCache` to maximize hit rates under real workload (requires integration test, not unit bench). Might improve overall system perf.
- **SQLite pragma optimization**: Tune `cache_size_kib`, `mmap_size_mb`, `journal_size_limit_mb` for better write throughput. Easy parameter sweep.
- **Batch size tuning**: For `SQLiteStore::store_record`, test different batch sizes (the code might already batch?) Looking at code, store_record is per-record. Could implement batch insert and benchmark.

### 3. Long shot / structural changes
- **Switch to binary serialization** (e.g., bincode) for internal storage, convert to JSON only at API boundary. Would break compatibility, so probably not allowed.
- **Use `postcard`** for no-std serialization? Not applicable.
- **Parallelize serialization**: If encoding many messages, use rayon to parallelize. But each message is individual; overhead may outweigh benefits.
- **Pre-allocate buffers** in `RedisStore::publish_record` and reuse across calls. Might reduce allocation overhead. Would need thread_local buffer.

## Proposed sequence
1. Run baseline (done).
2. Try derived Serialize for enums (switch from manual to `#[serde(rename_all="lowercase")]`). Measure.
   - If improves or neutral, keep. If regresses, discard.
3. Try `serde_json::to_vec` approach (modify benchmark to call a wrapper that does `to_vec` then `String::from_utf8`). Measure.
   - If improves even slightly, keep.
4. Consider implementing custom `Serialize` for `JetstreamMessage` that writes fields in a pre-determined order with minimal overhead.
5. If serialization gains plateau, consider reinitializing the experiment to target deserialization or SQLite store performance.

## Notes
- All changes must preserve JSON format compatibility.
- Tests must pass.
- No new dependencies (but `simd-json` already present; can be used conditionally behind feature flag).
