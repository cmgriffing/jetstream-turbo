# Autoresearch: SQLite Batch Store Optimization

## Objective

Optimize the performance of `SQLiteStore::store_batch`. This component persists enriched records to SQLite and is a significant contributor to overall processing latency in the real system (not the mock pipeline). The current baseline time for a batch of 100 records is approximately 7.9 ms (79 µs per record). We aim to reduce this by exploring:

- SQLite pragma tuning (cache_size, mmap_size, journal_size_limit, synchronous, journal_mode)
- Statement preparation reuse (caching the INSERT statement)
- Reducing per-row overhead in the batch loop
- Alternative batching strategies (e.g., using `UNION ALL`?)
- Minimizing allocations during serialization (already using simd-json for message/metadata)

We will measure the median time for `store_batch` with a batch size of 100 records.

## Metrics

- **Primary**: `store_batch_ms` (milliseconds, lower is better) — median time to store 100 records in a batch.
- **Secondary**: `records_per_second` (derived) — throughput.
- **Secondary**: `test_status` — all tests must pass.

## How to Run

`./autoresearch.sh` runs the `sqlite_batch_store` benchmark from `benches/hydration_benchmark.rs` and outputs:

```
METRIC store_batch_ms=<median_ms>
```

The script extracts the median time (originally in ns) and converts to ms.

## Files in Scope

- `src/storage/sqlite.rs` — `SQLiteStore::store_batch` implementation, pragma configuration.
- `benches/hydration_benchmark.rs` — the benchmark definition (may tweak batch size if needed)
- `src/models/enriched.rs` — serialization of `EnrichedRecord` (uses simd-json)
- `autoresearch.sh`

## Off Limits

- Do NOT compromise data integrity or durability guarantees beyond acceptable trade-offs (any change to WAL mode or synchronous must be evaluated carefully; we may experiment with `synchronous=OFF` only if it's acceptable for the use case? The project's requirements likely need durability; we'll keep `synchronous=NORMAL` as default; changes to lower durability may be considered only if clearly beneficial and documented).
- Do NOT break tests.
- Do NOT introduce new dependencies.

## Constraints

- All tests must pass with `--features testing`.
- Preserve existing behavior (correctness).
- No new dependencies.

## What's Been Tried

(Will be populated after experiments)

Baseline: `store_batch` for 100 records: ~7,917,517 ns = 7.92 ms (median). (From `benches/baselines/sqlite_batch_store.json`.)

Potential directions:
1. Increase `cache_size_kib` further (currently 32*1024=32768 KiB = 32 GiB? That seems huge. Actually 32*1024 = 32768 KiB = 32 MiB. Could increase to 64 MiB? Probably not needed for tiny DB.
2. Use `PRAGMA synchronous = OFF` (risky) — measure gain vs risk.
3. Reuse prepared statement: store the query string as a member of `SQLiteStore` and reuse across batches, avoiding re-preparation. This may reduce overhead.
4. Use `sqlx::query_as` with a pre-bound statement? Not sure.
5. Reduce the number of columns stored? Can't.

We'll start by establishing a fresh baseline from this branch, then experiment.
