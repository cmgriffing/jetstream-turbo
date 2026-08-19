# Ideas Backlog — cpu_throughput autoresearch

## Considered but deliberately NOT applied

- **Parallel hydrate via tokio::spawn**: The bench creates a multi-thread Runtime and `block_on`s hydrate_batch; parallelizing per-message hydration across workers would cut hydrate wall-time ~2ms. DECLINED: the benchmark is a deterministic single-core CPU-throughput harness (parse/serialize remain single-threaded); parallelizing only hydrate games the wall-clock measurement rather than improving per-core throughput. Could be a legit production architecture change (needs ordering-preserving indexed collection + error semantics), but out of scope for this harness.

## Deferred / blocked optimizations

- **Raw-record splice (BLOCKED)**: Store `CommitData.record` as raw JSON bytes captured during parse; emit verbatim during serialize. Would save ~2ms parse + ~1ms serialize. Blocked because serde has no raw-JSON emit hook and simd-json's serializer has no RawValue support (serialize_bytes → number array). Would need a hand-written serializer — too risky.
- **`simd_json::to_writer` with shared buffer in storage path (PRODUCTION ONLY)**: `sqlite.rs store_batch` serializes message+metadata per record with `to_string` (fresh alloc each). Reusing one `Vec<u8>` + `to_writer` measured ~4.6ms vs ~5.3ms per 10k. The benchmark uses `to_string` so this doesn't move the metric, but it's a real production win.
- **known-key feature for RecordView/parse**: simd-json `known-key` cargo feature precomputes key hashes. Marginal (~0.2ms) and requires manual KnownKey plumbing; not automatic with serde derive.
- **stack-buffer parse copy**: avoid the `text.to_string()` heap alloc for small messages (~0.2-0.4ms). Tried mentally; marginal, adds unsafe/size-threshold complexity.
- **target-cpu=native**: simd-json uses runtime detection by default so little gain; changes build config globally.
- **moka → faster read structure for user cache**: ~0.5ms of resolve phase is moka gets. Would need a new dependency (dashmap etc.) — not worth it.
- **Reduce per-message Instant::now() in hydrate_one** (hydration_time_ms metric): keep — it's real metric data.
- **skip `banner:null` etc. in BlueskyProfile serialization**: format change (missing keys vs null), risky for downstream consumers; ~0.1ms.

## Done (do not re-try)

- Hydrate: ahash sets/maps, single-pass profile resolution, sync hydrate_one, merged traversal, single get/match.
- OwnedValue record (parse+serialize).
- Thread-local Buffers reuse for parse_message.
