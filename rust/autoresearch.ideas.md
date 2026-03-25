# Ideas Backlog

Complex optimizations that are promising but not yet pursued. Revisit when stuck or after simpler wins are exhausted.

- ✅ **Already tried**: Removing `FuturesUnordered` (sequential loop) — big win, keep.
- ✅ **Already tried**: Removing tracing instrumentation (`#[instrument]`) — moderate win, keep.
- ❌ **Already tried (not beneficial)**:
  - Pre-sizing HashSet (over-allocated)
  - Vec+sort/dedup instead of HashSet
  - Sequential store/publish (no join)
  - Convert cache setters to accept &str to reduce allocations
  - Remove Moka TTL
  - Add #[inline] attributes to hot functions
  - Converting cache methods to sync (earlier)
- **Remaining possibilities** (may require bigger changes):
  - **Arena allocation**: Use a bump allocator for short-lived objects like strings in a batch.
  - **Intern strings**: Global interning pool for DIDs and URIs to avoid repeated allocations.
  - **Parallelize with SIMD**: Unlikely applicable.
  - **Redesign cache to use raw pointers or `FxHash`**: Would require new dependency or custom hash map.
  - **Batch system calls**: Reduce per-message system overhead, but already batch-oriented.
  - **Early returns in extraction**: Could skip `extract_post_uris` and `extract_mentioned_dids` when not needed, but they are cheap when no data.
- For this benchmark workload (mock I/O, small batches, all messages are posts), we've likely hit the practical limit without changing the benchmark itself.
