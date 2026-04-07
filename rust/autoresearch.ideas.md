# Optimization Findings

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

## General Notes
- `simd-json` is faster for deserialization (used in Jetstream parsing), but not for serialization of `EnrichedRecord` (regression observed).
- Cache `get` operations are already very fast (~81 ns). `set` is slower (~482 ns) but only on misses.
- Tests must pass; any change must maintain correctness.

## Next Steps (if continuing)
- Try prepared statement caching in `SQLiteStore::store_batch`.
- Explore reducing `message_metadata` serialization cost (maybe skip empty fields).
- Investigate if `turbocharger` orchestration can batch records larger than current max to reduce transaction overhead.
- Consider whether `synchronous = OFF` could be configurable for deployments that can tolerate some loss.
