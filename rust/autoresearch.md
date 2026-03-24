# Autoresearch: Optimize Full Pipeline Throughput

## Objective
Reduce the execution time of the full message processing pipeline (hydration → storage → publish → broadcast) for a batch of 25 messages. The benchmark uses mock implementations to measure pure computational performance of the Rust code without I/O.

## Metrics
- **Primary**: `total_ns` (nanoseconds, lower is better) — median total time measured by Criterion for the `full_pipeline_batch_25` benchmark.
- **Secondary**: (optional) We may add cache hit rates and API call counts later if we instrument the benchmark.

## How to Run
`./autoresearch.sh` — runs the specific benchmark and outputs `METRIC total_ns=<value>`.

## Files in Scope
These are the key files that contain hot-path code measured by the benchmark:

- `src/hydration/hydrator.rs` — `Hydrator::hydrate_batch` and `hydrate_message` perform the enrichment logic. Opportunities: reduce cloning, allocate less.
- `src/hydration/cache.rs` — `TurboCache` uses Moka with TTL; can tune capacity, TTL, or check patterns.
- `src/turbocharger/orchestrator.rs` — `process_batch_internal` orchestrates the pipeline; `BATCH_SIZE`, `MAX_WAIT_TIME_MS`, and concurrency control via semaphore. Opportunities: remove unnecessary clones of enriched records, adjust constants.
- `src/config/settings.rs` — Configuration structs for cache sizes, batch sizes, SQLite pragmas; may expose tunable knobs.
- `src/storage/sqlite.rs` — `SQLiteStore` and pragma configuration. Opportunities: tune pragmas (cache_size_kib, mmap_size_mb, journal_size_limit_mb) for better in-memory performance.
- `benches/pipeline_benchmark.rs` — The benchmark itself; may need instrumentation to expose secondary metrics.

## Off Limits
- No new dependencies (do not modify `Cargo.toml` to add crates).
- Do not change trait signatures (`MessageSource`, `ProfileFetcher`, `PostFetcher`, `RecordStore`, `EventPublisher`).
- Do not break existing API contracts.
- Mocks in `src/testing/mocks.rs` are used by the benchmark; keep them simple and realistic. Do not "cheat" by making mocks artificially faster.

## Constraints
1. **All tests must pass** with the `testing` feature enabled:
   ```bash
   cargo test --features testing --workspace
   ```
2. **No new dependencies** — stay within the existing `Cargo.toml`.
3. **Correctness first** — any change that introduces data loss, deadlock, or race condition is unacceptable.
4. **Benchmark workload remains constant** — batch size 25, same mock data profiles/posts.
5. **Changes should be production-appropriate** — avoid tuning that only helps the synthetic benchmark but would degrade real-world I/O latency. For example, removing concurrency entirely might speed up mocks but hurt real network calls. Prefer tunable knobs or balanced improvements.

## What's Been Tried
(Will be updated as experiments progress)

## Initial Ideas & Hypotheses
- **Remove redundant clones**:
  - In `Hydrator::hydrate_message`, `EnrichedRecord::new(message.clone())` clones the entire `JetstreamMessage`. Change to `EnrichedRecord::new(message)` after extracting needed fields.
  - In `TurboCharger::process_batch_internal` (and similarly in test pipeline), `enriched_records.clone()` is done twice to pass to `store_batch` and `publish_batch`. Replace with references: `store_batch(&enriched_records)` and `publish_batch(&enriched_records)` using `tokio::join!` to run concurrently with borrowed data.
- **SQLite pragma tuning**: The current `SQLitePragmaConfig` uses `cache_size_kib: 32*1024` (32MiB), `mmap_size_mb: 64`, `journal_size_limit_mb: 512`. Could larger cache size, different mmap, or different synchronous mode improve performance? The benchmark uses `MockRecordStore`, not real SQLite, so SQLite pragmas won't affect it. However, optimizing the real `SQLiteStore` might still be valuable, but it won't show in this benchmark. We may need separate optimization for storage if needed.
- **Cache configuration**: `TurboCache::new(user_capacity, post_capacity)` currently uses default TTL of 300 seconds. Could different TTL or capacity improve hit rates and reduce fetches? The benchmark already seeds all profiles, so after the first bulk fetch, all cache hits are from the local cache. Cache size might not matter as long as it fits 25 profiles/posts. The benchmark's cache is per-iteration, so cache capacity isn't tested across iterations.
- **Reduce allocations in `hydrate_batch`**:
  - The `HashSet`s for unique DIDs/URIs could be pre-sized (`with_capacity(messages.len())`) to avoid rehashing.
  - The `Vec`s for uncached_dids/uris could be collected more efficiently.
- **Change `hydrate_messages` concurrency**: For a batch of 25 with no I/O, `FuturesUnordered` overhead may be significant. Could sequential processing be faster? Or use a limited concurrency semaphore (e.g., `buffer_unordered(4)`)? We can experiment by modifying `hydration/hydrator.rs` to use a different concurrency strategy and measure impact.
- **Batch processing constants**: The orchestrator's `BATCH_SIZE = 25` and `MAX_WAIT_TIME_MS = 200` are not used in the benchmark directly (the benchmark calls `hydrate_batch` with a pre-made batch). But if we change the orchestrator's batch size, the benchmark calls it directly. To keep benchmark comparable, we should keep batch size at 25 for the targeted benchmark. But we could create a variant of the benchmark for other batch sizes to find optimal batch size. That would be a separate benchmark, not the one we're optimizing.

## Experiment Strategy
We will:
1. Establish baseline median time for `full_pipeline_batch_25`.
2. Apply targeted code changes (starting with low-hanging fruit: clone elimination).
3. Run the benchmark and compare against baseline using Criterion's statistical analysis.
4. If the improvement is statistically significant and tests still pass, keep the change; otherwise discard.
5. Iterate: try further refinements like pre-sizing collections, adjusting concurrency limits, etc.

Each experiment will be logged with `log_experiment`, and we'll rely on `run_experiment` to time and parse the metric.

## Secondary Metrics to Possibly Add
If needed, we can instrument the benchmark to also report:
- `cache_hits`, `cache_misses`, `api_calls_count` from the `TurboCache` and `MockProfileFetcher`.
- `allocations` using `jemalloc` stats or custom tally.
But these will be added only if primary metric improvements stall and we need more diagnostic data.

## Expected Baseline
We'll measure the baseline before making any changes. Typically, the `full_pipeline_batch_25` benchmark runs in the range of ~100-300 µs (but actual number unknown until measured). Our goal is to reduce this time consistently and significantly (>2% improvement with statistical confidence).
