# Autoresearch: Jetstream Turbo Pipeline Optimization

## Objective

Optimize the end-to-end message processing pipeline. The system receives Jetstream firehose messages and performs hydration (enriching with Bluesky profile/post data), storage (SQLite), and event publishing (Redis). The critical performance metric is the time to process a batch of messages through the full pipeline.

Previous experiments focused on low-level serialization of `JetstreamMessage`, gaining a few percent. Now we target holistic improvements that could yield larger gains:
- Cache hit rate optimization via policy tuning
- Batch size tuning for storage and publishing
- Parallelism and concurrency adjustments
- Reducing allocation overhead in hot paths
- Database pragma fine-tuning
- Serialization backend selection (serde_json vs simd-json)
- Workflow improvements (prefetching, early filtering)

We'll measure baseline performance across different batch sizes, then systematically explore changes.

## Metrics

- **Primary**: `pipeline_µs` (microseconds, lower is better) — median time to process a batch through the full pipeline (hydrate → store → publish → broadcast).
- **Secondary**: `hydrate_µs` (µs) — hydration-only time for the same batch.
- **Secondary**: `cache_hit_rate` (0.0-1.0) — observed cache hit rate during the run.
- **Secondary**: `batch_size` (count) — number of messages in batch (experiment with different sizes).

## How to Run

`./autoresearch.sh` — runs the pipeline benchmark and outputs structured `METRIC` lines:

```
METRIC pipeline_µs=<median_time_µs>
METRIC hydrate_µs=<hydration_time_µs>
METRIC cache_hit_rate=<hit_rate>
METRIC batch_size=<size>
```

The script uses `cargo bench` with the `pipeline_benchmark` suite.

## Files in Scope

- `src/hydration/hydrator.rs` — main hydration logic and batch processing
- `src/hydration/cache.rs` — TurboCache hit rates, eviction policies, capacity tuning
- `src/storage/sqlite.rs` — `RecordStore::store_batch` implementation and pragmas
- `src/storage/redis.rs` — `EventPublisher::publish_batch` (serde_json vs simd-json)
- `src/turbocharger/orchestrator.rs` — high-level orchestration, buffer sizes
- `benches/pipeline_benchmark.rs` — the benchmark itself (may modify batch sizes/scenarios)
- `autoresearch.sh` — experiment runner

## Off Limits

- Do NOT break correctness: data must be stored and published correctly.
- Do NOT change external API contracts (JSON format, database schema).
- Do NOT add new runtime dependencies without careful evaluation (but can enable existing optional ones).
- Do NOT degrade error handling or observability.

## Constraints

- All tests must pass: `cargo test --features testing --workspace -- --test-threads=1`
- No new dependencies (can use what's already in Cargo.toml).
- Each experiment must produce measurable improvements on the primary metric.

## What's Been Tried

(Will be populated as experiments proceed)

### Baseline
- **Pipeline (batch 25)**: ~87 µs
- **Hydration only (batch 25)**: ~61 µs
- **Cache hit rate**: ~0.5 (in mock tests)

### Initial Hypotheses
1. Switching `redis.rs` from `serde_json::to_string` to `simd_json::to_string` may reduce serialization overhead in the publish step.
2. Increasing cache capacity in the benchmark from 1000 to larger (e.g., 10000) could improve hit rates and reduce fetches.
3. Tuning SQLite pragmas (larger `mmap_size_mb`, `cache_size_kib`) might speed up `store_batch`.
4. Reducing allocations in batch loops (e.g., pre-allocating vectors) could shave microseconds.
