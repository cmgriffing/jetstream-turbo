# Ideas Backlog

Complex optimizations that are promising but not yet pursued. Revisit when stuck or after simpler wins are exhausted.

- Switch `Hydrator::hydrate_messages` to use a bounded concurrency pattern (e.g., `stream::buffer_unordered(N)`) to reduce task-switch overhead while still parallelizing.
- Tune `TurboCache` TTL from 300s to something shorter if data is short-lived, or longer if reuse across batches is possible in production.
- Investigate using `crossbeam` or `moka` with different policies (e.g., `squeue_stats`) for potentially lower overhead under high contention.
- Profile allocation patterns; consider using `Bytes` or `Arc<[u8]>` for binary data if messages are large.
- Explore using `serde_json::value::RawValue` for zero-copy JSON deserialization of message records.
- Batch SQLite operations with `transaction()` even for individual batch stores if not already done; also prepare statements reuse.
- Consider removing `FuturesUnordered` entirely for small batches; a simple `for` loop with `.await` might be faster due to less state machine overhead and better CPU cache locality.
- Pre-allocate vectors with exact capacity in `hydrate_batch` to eliminate reallocations: `HashSet::with_capacity(messages.len() * avg_dids_per_msg)`, etc.
- Investigate whether `Arc<EnrichedRecord>` would reduce cloning when broadcasting multiple times.
