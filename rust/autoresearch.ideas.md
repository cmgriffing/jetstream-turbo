# Ideas Backlog

Complex optimizations that are promising but not yet pursued. Revisit when stuck or after simpler wins are exhausted.

- ✅ **Already tried**: Removing `FuturesUnordered` (sequential loop) — big win, keep.
- ❌ **Already tried**: Pre-sizing HashSet (over-allocated) — regression, discard.
- **Bounded concurrency**: If we need to retain some parallelism for I/O-bound scenarios but want to limit overhead, switch to `stream::buffer_unordered(N)` with a small N (e.g., 4).
- **TurboCache TTL tuning**: Current 300s may be arbitrary. Shorter TTL could free memory faster; longer TTL could improve hit rates if data reused across batches.
- **Moka cache policies**: Explore `squeue_stats` or other eviction policies for lower contention/higher throughput.
- **Zero-copy deserialization**: Use `serde_json::value::RawValue` to avoid allocating JSON strings for message records.
- **SQLite optimizations**: Batch operations in transactions, reuse prepared statements, tune WAL checkpointing. (Not directly measurable in mocks, but could improve production.)
- **Profile allocation patterns**: Use `dhat` or `tracing` to see where heap allocations happen; maybe replace `String` with `Arc<str>` or `Bytes` for certain fields.
- **Arc<EnrichedRecord> for broadcast**: Currently broadcast moves each record; cloning an Arc might be cheaper if multiple consumers need the same record. But in pipeline only one consumer (broadcast channel) exists, so maybe not needed.
