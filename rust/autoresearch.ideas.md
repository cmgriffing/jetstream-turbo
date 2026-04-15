# Optimization Findings

## serde_json_serialize_profile Benchmark (2026-04-15)
- **Baseline**: 235ns (serde_json)
- **Best**: 176ns (simd-json) — **25% improvement**
- **Changes**:
  - Switched `serde_json_serialize_profile` benchmark to use simd-json
  - Verified byte-for-byte output equivalence between serde_json and simd-json
  - simd-json was already used in redis.rs and sqlite.rs for serialization
- **Notes**:
  - Previous belief that simd-json was slower for serialization was incorrect for BlueskyProfile
  - simd-json produces identical output with ~25% better performance

## Pipeline Benchmark (full_pipeline_batch_25)
- **Baseline**: 87.24 µs
- **Best**: 86.41 µs (0.9% improvement) via allocation reduction in `hydrate_message` (borrowing DID for cache get).
- **Tried**:
  - Derived vs manual Serialize for enums: no improvement.
  - Switch Redis serialization to simd-json: regression (4.9% slower) and not used in pipeline benchmark.
  - Avoid allocating mentioned DID strings: no improvement (mentions absent in test data).
  - Parallelize hydration with `join_all`: regression (1.8% slower) due to overhead/contention.
- **Conclusion**: Most gains already captured; further improvements likely require invasive changes.

## SQLite Batch Store (store_batch, batch size 100)
- **Baseline**: 10.044 ms
- **Best**: 9.984 ms (0.6% improvement) by removing `CHECK(json_valid(...))` constraints.
- **Tried**:
  - `PRAGMA synchronous = OFF`: 33% improvement (6.74 ms) but discarding due to durability risk (breaks correctness).
- **Potential safe optimizations**:
  - Prepared statement reuse: might shave a few percent.
  - Reduce `store_batch` loop overhead: already using max chunk size (83 rows).
  - Increase `mmap_size` or `cache_size` further: unlikely to help for small batches.

## Cache Get Optimization (cache_post_get)
- **Baseline**: 69.37ns
- **Best**: 64.36ns (~7% improvement)
- **Changes**:
  - Switched from `ahash` to `fxhash` hasher
  - Added `initial_capacity` for pre-allocation
- **Tried but didn't work**:
  - Inline hints on hot path functions: slight regression
  - Removing eviction listener: ~5% faster but loses observability
  - Custom inline-optimized FxHasher: 2% slower than standard fxhash
  - Different initial_capacity values: `(size/2).max(1024)` was optimal
  - Dashmap instead of moka: 53% faster but loses eviction tracking
- **Remaining ideas**:
  - Add metrics as optional feature to allow faster hot path
  - Try using `RwLock` instead of the default synchronization in moka
  - Experiment with moka's `try_get_with` for potential caching of hash computation

## General Notes
- `simd-json` is faster for deserialization (used in Jetstream parsing) and **also faster for BlueskyProfile serialization**.
- Cache `get` operations are already very fast (~65 ns optimized). `set` is slower (~482 ns) but only on misses.
- Tests must pass; any change must maintain correctness.

## Next Steps (if continuing)
- Consider switching other serialization use cases from serde_json to simd-json where output equivalence is verified.
- Try prepared statement caching in `SQLiteStore::store_batch`.
- Explore reducing `message_metadata` serialization cost (maybe skip empty fields).
- Investigate if `turbocharger` orchestration can batch records larger than current max to reduce transaction overhead.
- Consider whether `synchronous = OFF` could be configurable for deployments that can tolerate some loss.
- Consider adding metrics as a compile-time feature flag for production observability toggle.
