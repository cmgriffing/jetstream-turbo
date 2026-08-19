//! Tier 3 — regression guards.
//!
//! Retains the cache, SQLite, hydration, full-pipeline, and progress-tracker
//! benchmarks as regression guards. Mock fetchers/stores are constructed
//! outside the timed loop so the benchmarks measure pipeline stages, not
//! fixture setup.

use criterion::{criterion_group, criterion_main, Criterion};
use jetstream_turbo_rs::hydration::{Hydrator, TurboCache};
use jetstream_turbo_rs::models::bluesky::{BlueskyPost, BlueskyProfile};
use jetstream_turbo_rs::models::enriched::{EnrichedRecord, HydratedMetadata, ProcessingMetrics};
use jetstream_turbo_rs::models::jetstream::{
    CommitData, JetstreamMessage, MessageKind, OperationType, RecordValue,
};
use jetstream_turbo_rs::storage::{EventPublisher, RecordStore, SQLitePragmaConfig, SQLiteStore};
use jetstream_turbo_rs::testing::{
    create_message_batch, create_post_message, create_profile, MockEventPublisher, MockPostFetcher,
    MockProfileFetcher, MockRecordStore,
};
use jetstream_turbo_rs::turbocharger::{PipelineProgress, PipelineStage};

use std::hint::black_box;
use std::sync::{Arc, OnceLock};
use tempfile::TempDir;
use tokio::runtime::Runtime;
use tokio::sync::broadcast;

const CACHE_ACCESS_BATCH_SIZE: u32 = 8;
const SQLITE_BENCH_BATCH_SIZE: u32 = 4;

fn batched_iters(iters: u64, batch_size: u32) -> u64 {
    iters * u64::from(batch_size)
}

fn per_batched_op(elapsed: std::time::Duration, batch_size: u32) -> std::time::Duration {
    elapsed / batch_size
}

fn create_test_profile(i: usize) -> BlueskyProfile {
    BlueskyProfile {
        did: format!("did:plc:test{i}").into(),
        handle: format!("user{i}.bsky.social"),
        display_name: Some(format!("Test User {i}")),
        description: Some(format!("Description for user {i}")),
        avatar: Some(format!("https://avatar.example.com/{i}")),
        banner: Some(format!("https://banner.example.com/{i}")),
        followers_count: Some(1000 + i as u64),
        follows_count: Some(500 + i as u64),
        posts_count: Some(250 + i as u64),
        indexed_at: None,
        created_at: None,
        labels: None,
        serialized: OnceLock::new(),
    }
}

fn create_test_post(i: usize) -> BlueskyPost {
    BlueskyPost {
        uri: format!("at://did:plc:test{i}/app.bsky.feed.post/{i}"),
        cid: format!("bafyrei{i}"),
        author: create_test_profile(i),
        text: format!("Test post number {i}"),
        created_at: chrono::Utc::now(),
        embed: None,
        reply: None,
        facets: None,
        labels: None,
        like_count: Some(10),
        repost_count: Some(5),
        reply_count: Some(2),
    }
}

fn create_test_message(i: usize) -> JetstreamMessage {
    JetstreamMessage {
        did: format!("did:plc:test{i}").into(),
        time_us: Some(1640995200000000 + i as u64),
        seq: Some(i as u64),
        kind: MessageKind::Commit,
        commit: Some(Box::new(CommitData {
            rev: Some(format!("3x{i}").into()),
            operation_type: OperationType::Create,
            collection: Some("app.bsky.feed.post".into()),
            rkey: Some(format!("{i}").into()),
            record: Some(RecordValue::from_value(simd_json::json!({
                "text": format!("Hello world {}", i),
                "createdAt": "2024-01-01T00:00:00.000Z"
            }))),
            cid: Some(format!("bafyrei{i}")),
        })),
        raw_json: None,
    }
}

fn benchmark_sqlite_pragmas() -> SQLitePragmaConfig {
    SQLitePragmaConfig {
        cache_size_kib: 32 * 1024,
        mmap_size_mb: 64,
        journal_size_limit_mb: 512,
    }
}

fn build_hydrator(
    profile_fetcher: Arc<MockProfileFetcher>,
    post_fetcher: Arc<MockPostFetcher>,
) -> Hydrator<MockProfileFetcher, MockPostFetcher> {
    let cache = TurboCache::new(1000, 1000);
    Hydrator::new(cache, profile_fetcher, post_fetcher)
}

fn bench_cache_operations(c: &mut Criterion) {
    let rt = Runtime::new().unwrap();

    c.bench_function("cache_user_profile_set", |b| {
        b.iter(|| {
            rt.block_on(async {
                let cache = TurboCache::new(10000, 10000);
                let profile = create_test_profile(0);

                for i in 0..1000 {
                    cache.set_user_profile(format!("did:plc:test{i}"), Arc::new(profile.clone()));
                }
                black_box(cache);
            });
        });
    });

    c.bench_function("cache_user_profile_get", |b| {
        let cache = rt.block_on(async {
            let cache = TurboCache::new(10000, 10000);
            let profile = create_test_profile(0);

            for i in 0..1000 {
                cache.set_user_profile(format!("did:plc:test{i}"), Arc::new(profile.clone()));
            }

            cache
        });

        b.iter(|| {
            rt.block_on(async {
                for i in 0..1000 {
                    black_box(cache.get_user_profile(&format!("did:plc:test{i}")));
                }
            });
        });
    });

    c.bench_function("cache_post_set", |b| {
        b.iter(|| {
            rt.block_on(async {
                let cache = TurboCache::new(10000, 10000);
                let post = create_test_post(0);

                for i in 0..1000 {
                    cache.set_post(format!("at://test/{i}"), Arc::new(post.clone()));
                }
                black_box(cache);
            });
        });
    });

    c.bench_function("cache_post_get", |b| {
        let cache = rt.block_on(async {
            let cache = TurboCache::new(10000, 10000);
            let post = create_test_post(0);

            for i in 0..1000 {
                cache.set_post(format!("at://test/{i}"), Arc::new(post.clone()));
            }

            cache
        });

        b.iter_custom(|iters| {
            let total_iters = batched_iters(iters, CACHE_ACCESS_BATCH_SIZE);
            let start = std::time::Instant::now();
            for _ in 0..total_iters {
                rt.block_on(async {
                    for i in 0..1000 {
                        black_box(cache.get_post(&format!("at://test/{i}")));
                    }
                });
            }
            per_batched_op(start.elapsed(), CACHE_ACCESS_BATCH_SIZE)
        });
    });

    c.bench_function("cache_bulk_get_user_profiles", |b| {
        let cache = rt.block_on(async {
            let cache = TurboCache::new(10000, 10000);
            let profile = create_test_profile(0);

            for i in 0..100 {
                cache.set_user_profile(format!("did:plc:test{i}"), Arc::new(profile.clone()));
            }

            cache
        });
        let dids: Vec<Arc<str>> = (0..100)
            .map(|i| Arc::from(format!("did:plc:test{i}")))
            .collect();

        b.iter(|| {
            black_box(cache.get_user_profiles(&dids));
        });
    });

    c.bench_function("cache_bulk_get_posts", |b| {
        let cache = rt.block_on(async {
            let cache = TurboCache::new(10000, 10000);
            let post = create_test_post(0);

            for i in 0..100 {
                cache.set_post(format!("at://test/{i}"), Arc::new(post.clone()));
            }

            cache
        });
        let uris: Vec<String> = (0..100).map(|i| format!("at://test/{i}")).collect();

        b.iter(|| {
            black_box(cache.get_posts(&uris));
        });
    });
}

fn bench_sqlite_operations(c: &mut Criterion) {
    let rt = Runtime::new().unwrap();

    c.bench_function("sqlite_store_record", |b| {
        let temp_dir = TempDir::new().unwrap();
        let db_path = temp_dir.path().join("test.db");

        let store = rt.block_on(async {
            SQLiteStore::new(&db_path, benchmark_sqlite_pragmas())
                .await
                .unwrap()
        });

        let message = create_test_message(0);
        let record = EnrichedRecord {
            message,
            hydrated_metadata: HydratedMetadata::default(),
            processed_at: chrono::Utc::now(),
            metrics: ProcessingMetrics {
                hydration_time_ms: 10,
                api_calls_count: 2,
                cache_hit_rate: 0.5,
                cache_hits: 5,
                cache_misses: 5,
            },
        };

        b.iter_custom(|iters| {
            let total_iters = batched_iters(iters, SQLITE_BENCH_BATCH_SIZE);
            let start = std::time::Instant::now();
            for _ in 0..total_iters {
                rt.block_on(async {
                    black_box(store.store_record(&record).await.unwrap());
                });
            }
            per_batched_op(start.elapsed(), SQLITE_BENCH_BATCH_SIZE)
        });
    });

    c.bench_function("sqlite_batch_store", |b| {
        let temp_dir = TempDir::new().unwrap();
        let db_path = temp_dir.path().join("test.db");

        let store = rt.block_on(async {
            SQLiteStore::new(&db_path, benchmark_sqlite_pragmas())
                .await
                .unwrap()
        });

        let records: Vec<EnrichedRecord> = (0..100)
            .map(|i| {
                let message = create_test_message(i);
                EnrichedRecord {
                    message,
                    hydrated_metadata: HydratedMetadata::default(),
                    processed_at: chrono::Utc::now(),
                    metrics: ProcessingMetrics {
                        hydration_time_ms: 10,
                        api_calls_count: 2,
                        cache_hit_rate: 0.5,
                        cache_hits: 5,
                        cache_misses: 5,
                    },
                }
            })
            .collect();

        b.iter_custom(|iters| {
            let total_iters = batched_iters(iters, SQLITE_BENCH_BATCH_SIZE);
            let start = std::time::Instant::now();
            for _ in 0..total_iters {
                rt.block_on(async {
                    black_box(store.store_batch(&records).await.unwrap());
                });
            }
            per_batched_op(start.elapsed(), SQLITE_BENCH_BATCH_SIZE)
        });
    });
}

fn bench_single_message_hydration(c: &mut Criterion) {
    let rt = Runtime::new().unwrap();

    // Construct mocks and fixtures outside the timed loop.
    let profile_fetcher = Arc::new(MockProfileFetcher::new());
    let post_fetcher = Arc::new(MockPostFetcher::new());
    let message = create_post_message(0);
    let did = message.did.clone();
    rt.block_on(async {
        profile_fetcher.add_profile(create_profile(&did)).await;
    });
    let hydrator = build_hydrator(profile_fetcher, post_fetcher);

    c.bench_function("single_message_hydration", |b| {
        b.iter(|| {
            rt.block_on(async {
                black_box(hydrator.hydrate_message(message.clone()).await.unwrap())
            })
        });
    });
}

fn bench_batch_hydration_25(c: &mut Criterion) {
    let rt = Runtime::new().unwrap();

    // Construct mocks and fixtures outside the timed loop.
    let profile_fetcher = Arc::new(MockProfileFetcher::new());
    let post_fetcher = Arc::new(MockPostFetcher::new());
    let messages = create_message_batch(25);
    rt.block_on(async {
        for msg in &messages {
            profile_fetcher.add_profile(create_profile(&msg.did)).await;
        }
    });
    let hydrator = build_hydrator(profile_fetcher, post_fetcher);

    c.bench_function("batch_hydration_25_messages", |b| {
        b.iter(|| {
            rt.block_on(async {
                black_box(hydrator.hydrate_batch(messages.clone()).await.unwrap())
            })
        });
    });
}

fn bench_full_pipeline_single(c: &mut Criterion) {
    let rt = Runtime::new().unwrap();

    // Construct mocks and fixtures outside the timed loop.
    let profile_fetcher = Arc::new(MockProfileFetcher::new());
    let post_fetcher = Arc::new(MockPostFetcher::new());
    let record_store = Arc::new(MockRecordStore::new());
    let event_publisher = Arc::new(MockEventPublisher::new());
    let (broadcast_sender, _) = broadcast::channel(100);

    let message = create_post_message(0);
    let did = message.did.clone();
    rt.block_on(async {
        profile_fetcher.add_profile(create_profile(&did)).await;
    });
    let hydrator = build_hydrator(Arc::clone(&profile_fetcher), Arc::clone(&post_fetcher));

    c.bench_function("full_pipeline_single_message", |b| {
        b.iter(|| {
            rt.block_on(async {
                // Hydrate
                let enriched = hydrator.hydrate_batch(vec![message.clone()]).await.unwrap();
                // Store
                record_store.store_batch(&enriched).await.unwrap();
                // Publish
                event_publisher.publish_batch(&enriched).await.unwrap();
                // Broadcast
                for record in &enriched {
                    let _ = broadcast_sender.send(record.clone());
                }
                black_box(enriched)
            })
        });
    });
}

fn bench_full_pipeline_batch_25(c: &mut Criterion) {
    let rt = Runtime::new().unwrap();

    // Construct mocks and fixtures outside the timed loop.
    let profile_fetcher = Arc::new(MockProfileFetcher::new());
    let post_fetcher = Arc::new(MockPostFetcher::new());
    let record_store = Arc::new(MockRecordStore::new());
    let event_publisher = Arc::new(MockEventPublisher::new());
    let (broadcast_sender, _) = broadcast::channel(100);

    let messages = create_message_batch(25);
    rt.block_on(async {
        for msg in &messages {
            profile_fetcher.add_profile(create_profile(&msg.did)).await;
        }
    });
    let hydrator = build_hydrator(Arc::clone(&profile_fetcher), Arc::clone(&post_fetcher));

    c.bench_function("full_pipeline_batch_25", |b| {
        b.iter(|| {
            rt.block_on(async {
                // Hydrate
                let enriched = hydrator.hydrate_batch(messages.clone()).await.unwrap();
                // Store
                record_store.store_batch(&enriched).await.unwrap();
                // Publish
                event_publisher.publish_batch(&enriched).await.unwrap();
                // Broadcast
                for record in &enriched {
                    let _ = broadcast_sender.send(record.clone());
                }
                black_box(enriched)
            })
        });
    });
}

fn bench_progress_tracker_ingress_update(c: &mut Criterion) {
    let progress = PipelineProgress::new(6, 10_000);
    c.bench_function("progress_tracker_ingress_update", |b| {
        b.iter(|| {
            progress.valid_ingress();
            black_box(&progress);
        });
    });
}

fn bench_progress_tracker_batch_boundaries(c: &mut Criterion) {
    let progress = PipelineProgress::new(6, 10_000);
    c.bench_function("progress_tracker_batch_boundaries", |b| {
        b.iter(|| {
            let batch_id = progress.batch_started();
            progress.batch_stage(batch_id, PipelineStage::Storage);
            progress.store_succeeded();
            progress.batch_stage(batch_id, PipelineStage::Publication);
            progress.publication_succeeded();
            progress.batch_completed(batch_id, 25);
            black_box(batch_id);
        });
    });
}

criterion_group!(
    benches,
    bench_cache_operations,
    bench_sqlite_operations,
    bench_single_message_hydration,
    bench_batch_hydration_25,
    bench_full_pipeline_single,
    bench_full_pipeline_batch_25,
    bench_progress_tracker_ingress_update,
    bench_progress_tracker_batch_boundaries,
);
criterion_main!(benches);
