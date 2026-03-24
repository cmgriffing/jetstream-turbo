# Testing Architecture

This document describes the testing infrastructure for jetstream-turbo-rs, including how to run tests, write new tests, run benchmarks, and integrate with pi-autoresearch for performance optimization.

## Architecture Overview

The pipeline uses **trait-based abstractions** so that every I/O boundary can be replaced with a mock at test time. The production data flow is:

```
Jetstream WebSocket
       │
       ▼
┌──────────────┐     MessageSource trait
│JetstreamClient│     (client/jetstream.rs)
└──────┬───────┘
       │ Stream<JetstreamMessage>
       ▼
┌──────────────────────────────────────────────┐
│ TurboCharger<M, P, Po, S, E>                │  turbocharger/orchestrator.rs
│                                              │
│  Batches messages (25 per batch, 200ms flush)│
│  Spawns concurrent batch tasks via Semaphore │
│                                              │
│  For each batch:                             │
│    ┌────────────────────────────────┐        │
│    │ Hydrator<P, Po>               │        │  hydration/hydrator.rs
│    │                                │        │
│    │  1. Collect unique DIDs & URIs │        │
│    │  2. Check TurboCache           │        │
│    │  3. Fetch uncached via traits:  │        │
│    │     ProfileFetcher (P)         │        │  client/bluesky.rs
│    │     PostFetcher (Po)           │        │  client/bluesky.rs
│    │  4. Hydrate each message       │        │
│    └────────────┬───────────────────┘        │
│                 │ Vec<EnrichedRecord>         │
│                 ▼                             │
│    ┌───────────────────────────────────┐     │
│    │ tokio::join!(store, publish)      │     │
│    │   RecordStore::store_batch (S)    │     │  storage/sqlite.rs
│    │   EventPublisher::publish_batch(E)│     │  storage/redis.rs
│    └───────────────────────────────────┘     │
│                 │                             │
│                 ▼                             │
│    broadcast::Sender<EnrichedRecord>         │
└──────────────────────────────────────────────┘
```

### Trait Boundaries

| Trait | Defined in | Production impl | Mock impl |
|-------|-----------|----------------|-----------|
| `MessageSource` | `src/client/jetstream.rs` | `JetstreamClient` | `MockMessageSource` |
| `ProfileFetcher` | `src/client/bluesky.rs` | `BlueskyClient` | `MockProfileFetcher` |
| `PostFetcher` | `src/client/bluesky.rs` | `BlueskyClient` | `MockPostFetcher` |
| `RecordStore` | `src/storage/sqlite.rs` | `SQLiteStore` | `MockRecordStore` |
| `EventPublisher` | `src/storage/redis.rs` | `RedisStore` | `MockEventPublisher` |

### Generic Structs

The core orchestration structs are generic over the trait bounds:

- **`TurboCharger<M, P, Po, S, E>`** (`turbocharger/orchestrator.rs`) — top-level orchestrator, generic over all five traits. The `run()` method and `process_batch_internal()` work with any implementations.
- **`Hydrator<P, Po>`** (`hydration/hydrator.rs`) — hydrates messages using `ProfileFetcher` and `PostFetcher`.
- **`DataFetcher<P, Po>`** (`hydration/fetcher.rs`) — fetches missing profiles/posts from cache or API.
- **`BatchProcessor<P, Po>`** (`hydration/batch.rs`) — processes a stream of messages through the hydrator.

### Where Mocks Plug In

In tests and benchmarks, mocks replace the I/O traits:

```
MockMessageSource ──→ TurboCharger<Mock..., Mock..., Mock..., Mock..., Mock...>
                           │
                           ▼
                      Hydrator<MockProfileFetcher, MockPostFetcher>
                           │
                      ┌────┴────┐
                      ▼         ▼
              MockRecordStore  MockEventPublisher
```

All mock implementations are in `src/testing/mocks.rs`. They store data in-memory and expose `call_count` / `requested_*` fields for assertions.

## Running Tests

### Unit Tests

Run all unit tests (inline `#[cfg(test)]` modules):

```bash
cd rust && cargo test --lib
```

### Integration Tests

Run the pipeline integration tests:

```bash
cd rust && cargo test --test pipeline_integration_test
```

These tests exercise the full pipeline: hydration → store → publish → broadcast, using mock implementations.

### All Tests

```bash
cd rust && cargo test --features testing
```

The `testing` feature flag gates the `testing` module (`src/testing/`) which exposes mocks and fixtures. Integration tests and benchmarks enable this feature via `Cargo.toml`.

### Running a Specific Test

```bash
cd rust && cargo test --test pipeline_integration_test test_single_message_flows_through_pipeline
```

## Writing New Tests

### Using Mocks and Fixtures

The `testing` module (`src/testing/`) provides:

**Fixtures** (`testing/fixtures.rs`):
- `create_post_message(index: usize) -> JetstreamMessage` — creates a realistic post creation message
- `create_reply_message(index: usize, parent_did: &str, parent_rkey: &str) -> JetstreamMessage` — creates a reply message
- `create_message_batch(count: usize) -> Vec<JetstreamMessage>` — creates N post messages
- `create_profile(did: &str) -> BlueskyProfile` — creates a profile with realistic fields

**Mocks** (`testing/mocks.rs`):
- `MockMessageSource` — yields a fixed set of messages as a stream
- `MockProfileFetcher` — returns profiles by DID from an in-memory map; tracks `call_count` and `requested_dids`
- `MockPostFetcher` — returns posts by URI from an in-memory map; tracks `call_count` and `requested_uris`
- `MockRecordStore` — stores records in a `Vec`; tracks `call_count` and `stored_records`
- `MockEventPublisher` — records published events; tracks `call_count` and `published_records`

### Example: Writing a New Integration Test

```rust
use jetstream_turbo_rs::hydration::{Hydrator, TurboCache};
use jetstream_turbo_rs::storage::{EventPublisher, RecordStore};
use jetstream_turbo_rs::testing::{
    create_message_batch, create_post_message, create_profile,
    MockEventPublisher, MockPostFetcher, MockProfileFetcher, MockRecordStore,
};
use std::sync::Arc;
use std::sync::atomic::Ordering;

#[tokio::test]
async fn test_cache_reduces_api_calls() {
    // Set up mock pipeline
    let profile_fetcher = Arc::new(MockProfileFetcher::new());
    let post_fetcher = Arc::new(MockPostFetcher::new());
    let cache = TurboCache::new(1000, 1000);
    let hydrator = Hydrator::new(
        cache,
        Arc::clone(&profile_fetcher),
        Arc::clone(&post_fetcher),
    );
    let record_store = Arc::new(MockRecordStore::new());
    let event_publisher = Arc::new(MockEventPublisher::new());

    // Pre-populate mock with profiles
    let msg = create_post_message(0);
    profile_fetcher.add_profile(create_profile(&msg.did)).await;

    // First hydration — cache miss, should call profile fetcher
    let enriched = hydrator.hydrate_batch(vec![msg.clone()]).await.unwrap();
    let calls_after_first = profile_fetcher.call_count.load(Ordering::SeqCst);

    // Second hydration of same DID — cache hit, should NOT call profile fetcher again
    let enriched2 = hydrator.hydrate_batch(vec![msg]).await.unwrap();
    let calls_after_second = profile_fetcher.call_count.load(Ordering::SeqCst);

    assert_eq!(calls_after_first, calls_after_second,
        "second hydration should use cache, not call fetcher again");
}
```

### TestPipeline Helper

For full pipeline tests, see the `TestPipeline` struct in `tests/pipeline_integration_test.rs`. It wires up all mocks and provides a `process_batch()` method that mirrors the production `process_batch_internal()` flow:

```rust
let pipeline = TestPipeline::new();
let message = create_post_message(0);
pipeline.profile_fetcher.add_profile(create_profile(&message.did)).await;
let results = pipeline.process_batch(vec![message]).await;
assert_eq!(results.len(), 1);
```

## Benchmarking

### Running Benchmarks

```bash
cd rust && cargo bench --bench pipeline_benchmark
```

This runs 4 Criterion benchmarks:

| Benchmark | What it measures |
|-----------|-----------------|
| `single_message_hydration` | Hydrating one message through `Hydrator` |
| `batch_hydration_25_messages` | Hydrating a batch of 25 messages through `Hydrator` |
| `full_pipeline_single_message` | Full pipeline: hydrate → store → publish → broadcast (1 msg) |
| `full_pipeline_batch_25` | Full pipeline: hydrate → store → publish → broadcast (25 msgs) |

### Interpreting Criterion Output

Criterion prints timing statistics for each benchmark:

```
single_message_hydration
                        time:   [29.123 µs 30.456 µs 31.789 µs]
                        change: [-2.5% +0.3% +3.1%] (p = 0.89 > 0.05)
                        No change in performance detected.
```

- **time**: `[lower bound  estimate  upper bound]` — the middle value is the best estimate
- **change**: Comparison vs. the last saved baseline. If `p < 0.05`, the change is statistically significant.
- Results are saved in `target/criterion/` for comparison across runs.

To compare against a baseline:

```bash
# Save a baseline
cd rust && cargo bench --bench pipeline_benchmark -- --save-baseline before

# Make changes, then compare
cd rust && cargo bench --bench pipeline_benchmark -- --baseline before
```

### Adding a New Benchmark Scenario

Add a new function in `benches/pipeline_benchmark.rs` and register it in the `criterion_group!`:

```rust
fn bench_batch_hydration_100(c: &mut Criterion) {
    let rt = Runtime::new().unwrap();

    c.bench_function("batch_hydration_100_messages", |b| {
        b.iter(|| {
            rt.block_on(async {
                let profile_fetcher = Arc::new(MockProfileFetcher::new());
                let post_fetcher = Arc::new(MockPostFetcher::new());

                let messages = create_message_batch(100);
                for msg in &messages {
                    profile_fetcher
                        .add_profile(create_profile(&msg.did))
                        .await;
                }

                let hydrator = build_hydrator(profile_fetcher, post_fetcher);
                hydrator.hydrate_batch(messages).await.unwrap()
            })
        });
    });
}

criterion_group!(
    benches,
    bench_single_message_hydration,
    bench_batch_hydration_25,
    bench_full_pipeline_single,
    bench_full_pipeline_batch_25,
    bench_batch_hydration_100,  // ← add here
);
```

## pi-autoresearch Integration

This section describes how to point pi-autoresearch at the benchmark harness for automated performance optimization.

### Target Command

```bash
cargo bench --bench pipeline_benchmark
```

This runs the full benchmark suite using **mock implementations only** — no network I/O, no disk I/O. All measured time is pure computation: JSON construction, data transformation, cache lookups, batch processing, and memory allocation.

### Key Files to Optimize

These files contain the hot path measured by the benchmarks:

| File | What it does | Optimization potential |
|------|-------------|----------------------|
| `src/turbocharger/orchestrator.rs` | Batch orchestration, message buffering, concurrent task spawning | Batch size tuning, buffer reuse, clone reduction |
| `src/hydration/hydrator.rs` | `hydrate_batch()` and `hydrate_message()` — the core enrichment logic | HashSet allocation, cache lookup patterns, FuturesUnordered overhead |
| `src/hydration/fetcher.rs` | `DataFetcher` — fetches missing profiles/posts from cache | Cache check patterns, batch chunking |
| `src/hydration/batch.rs` | `BatchProcessor` — stream-based batch processing | Buffer management, flush timing |

### Constraints

1. **Do not change trait interfaces.** The trait signatures in `MessageSource`, `ProfileFetcher`, `PostFetcher`, `RecordStore`, and `EventPublisher` are the API contract. Changing them breaks all consumers.

2. **Keep mocks realistic.** Mock implementations should remain simple and representative. Don't optimize mocks to make benchmarks look faster — that defeats the purpose.

3. **Don't break existing tests.** All 7 integration tests in `tests/pipeline_integration_test.rs` and all unit tests must continue to pass:
   ```bash
   cd rust && cargo test --features testing
   ```

4. **The benchmark uses mocks, so I/O is eliminated.** All measured time is pure computation. This means network/disk optimizations won't show up in benchmarks — focus on algorithmic and memory improvements.

### Optimization Targets

These are the most promising areas for computational optimization:

**Batch Processing (`hydrator.rs:hydrate_batch`)**
- The method collects unique DIDs and URIs into `HashSet`s, then converts to `Vec`s. Pre-sizing or using a different collection could reduce allocations.
- `hydrate_messages()` uses `FuturesUnordered` to process each message concurrently. For CPU-bound work with mocks, this adds overhead — consider whether sequential processing is faster for small batches.
- Each enriched record is `.clone()`d twice in `process_batch_internal()` (once for store, once for publish). Reducing clones via `Arc<Vec<EnrichedRecord>>` or processing in-place could help.

**Data Transformation (`hydrator.rs:hydrate_message`)**
- `EnrichedRecord::new()` and metadata population involve allocations. Check if fields can be populated in-place.
- `extract_mentioned_dids()` and `extract_post_uris()` on `JetstreamMessage` may allocate vectors that could be avoided.

**Parsing and Serialization**
- The project uses `simd-json` for parsing in `client/jetstream.rs`. Benchmark fixtures construct JSON via `serde_json::json!()` — the hydration path deserializes/re-serializes this data.
- Mock `store_batch` and `publish_batch` are lightweight, but the `EnrichedRecord` construction and cloning dominates.

**Memory Allocation**
- Profile and post data is wrapped in `Arc` for cache storage. Audit whether `Arc` wrapping happens more than necessary.
- `Vec` allocations in batch processing: check if `Vec::with_capacity()` is used consistently.

### Workflow for pi-autoresearch

1. **Establish baseline:**
   ```bash
   cd rust && cargo bench --bench pipeline_benchmark -- --save-baseline baseline
   ```

2. **Make optimization changes** to the key files listed above.

3. **Verify tests still pass:**
   ```bash
   cd rust && cargo test --features testing
   ```

4. **Compare against baseline:**
   ```bash
   cd rust && cargo bench --bench pipeline_benchmark -- --baseline baseline
   ```

5. **Look for statistically significant improvements** (Criterion reports `p < 0.05`). Focus on the `batch_hydration_25_messages` and `full_pipeline_batch_25` benchmarks — these represent the real-world hot path.

6. **Iterate.** Save new baselines after confirmed improvements.
