use criterion::{criterion_group, criterion_main, BenchmarkId, Criterion};
use jetstream_turbo_rs::hydration::TurboCache;
use jetstream_turbo_rs::models::bluesky::{BlueskyPost, BlueskyProfile};
use jetstream_turbo_rs::models::enriched::{EnrichedRecord, HydratedMetadata, ProcessingMetrics};
use jetstream_turbo_rs::models::jetstream::{CommitData, JetstreamMessage};
use jetstream_turbo_rs::storage::SQLiteStore;
use serde_json::json;
use std::sync::Arc;
use tempfile::TempDir;

fn create_test_profile(i: usize) -> BlueskyProfile {
    BlueskyProfile {
        did: format!("did:plc:test{}", i).into(),
        handle: format!("user{}.bsky.social", i),
        display_name: Some(format!("Test User {}", i)),
        description: Some(format!("Description for user {}", i)),
        avatar: Some(format!("https://avatar.example.com/{}", i)),
        banner: Some(format!("https://banner.example.com/{}", i)),
        followers_count: Some(1000 + i as u64),
        follows_count: Some(500 + i as u64),
        posts_count: Some(250 + i as u64),
        indexed_at: None,
        created_at: None,
        labels: None,
    }
}

fn create_test_post(i: usize) -> BlueskyPost {
    BlueskyPost {
        uri: format!("at://did:plc:test{}/app.bsky.feed.post/{}", i, i),
        cid: format!("bafyrei{}", i),
        author: create_test_profile(i),
        text: format!("Test post number {}", i),
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
        did: format!("did:plc:test{}", i),
        time_us: Some(1640995200000000 + i as u64),
        seq: Some(i as u64),
        kind: "commit".to_string(),
        commit: Some(CommitData {
            rev: Some(format!("3x{}", i)),
            operation_type: "create".to_string(),
            collection: Some("app.bsky.feed.post".to_string()),
            rkey: Some(format!("{}", i)),
            record: Some(json!({
                "text": format!("Hello world {}", i),
                "createdAt": "2024-01-01T00:00:00.000Z"
            })),
            cid: Some(format!("bafyrei{}", i)),
        }),
    }
}

fn bench_cache_operations(c: &mut Criterion) {
    let rt = tokio::runtime::Runtime::new().unwrap();

    c.bench_function("cache_user_profile_set", |b| {
        b.iter(|| {
            rt.block_on(async {
                let cache = TurboCache::new(10000, 10000);
                let profile = create_test_profile(0);

                for i in 0..1000 {
                    cache
                        .set_user_profile(format!("did:plc:test{}", i), Arc::new(profile.clone()))
                        .await;
                }
            });
        });
    });

    c.bench_function("cache_user_profile_get", |b| {
        let cache = rt.block_on(async {
            let cache = TurboCache::new(10000, 10000);
            let profile = create_test_profile(0);

            for i in 0..1000 {
                cache
                    .set_user_profile(format!("did:plc:test{}", i), Arc::new(profile.clone()))
                    .await;
            }

            cache
        });

        b.iter(|| {
            rt.block_on(async {
                for i in 0..1000 {
                    let _result = cache.get_user_profile(&format!("did:plc:test{}", i)).await;
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
                    cache
                        .set_post(format!("at://test/{}", i), Arc::new(post.clone()))
                        .await;
                }
            });
        });
    });

    c.bench_function("cache_post_get", |b| {
        let cache = rt.block_on(async {
            let cache = TurboCache::new(10000, 10000);
            let post = create_test_post(0);

            for i in 0..1000 {
                cache
                    .set_post(format!("at://test/{}", i), Arc::new(post.clone()))
                    .await;
            }

            cache
        });

        b.iter(|| {
            rt.block_on(async {
                for i in 0..1000 {
                    let _result = cache.get_post(&format!("at://test/{}", i)).await;
                }
            });
        });
    });

    c.bench_function("cache_bulk_get_user_profiles", |b| {
        let cache = rt.block_on(async {
            let cache = TurboCache::new(10000, 10000);
            let profile = create_test_profile(0);

            for i in 0..100 {
                cache
                    .set_user_profile(format!("did:plc:test{}", i), Arc::new(profile.clone()))
                    .await;
            }

            cache
        });

        b.iter(|| {
            rt.block_on(async {
                let dids: Vec<String> = (0..100).map(|i| format!("did:plc:test{}", i)).collect();
                let _results = cache.get_user_profiles(&dids).await;
            });
        });
    });

    c.bench_function("cache_bulk_get_posts", |b| {
        let cache = rt.block_on(async {
            let cache = TurboCache::new(10000, 10000);
            let post = create_test_post(0);

            for i in 0..100 {
                cache
                    .set_post(format!("at://test/{}", i), Arc::new(post.clone()))
                    .await;
            }

            cache
        });

        b.iter(|| {
            rt.block_on(async {
                let uris: Vec<String> = (0..100).map(|i| format!("at://test/{}", i)).collect();
                let _results = cache.get_posts(&uris).await;
            });
        });
    });

    let mut group = c.benchmark_group("cache_hit_rates");

    for cache_size in [100, 1000, 10000].iter() {
        group.bench_with_input(
            BenchmarkId::new("hit_rate", cache_size),
            cache_size,
            |b, &cache_size| {
                b.iter(|| {
                    rt.block_on(async {
                        let cache = TurboCache::new(cache_size, cache_size);
                        let profile = create_test_profile(0);

                        for i in 0..(cache_size / 2) {
                            cache
                                .set_user_profile(
                                    format!("did:plc:test{}", i),
                                    Arc::new(profile.clone()),
                                )
                                .await;
                        }

                        let mut hits = 0;
                        let mut misses = 0;

                        for i in 0..cache_size {
                            let did = if i < (cache_size / 2) {
                                format!("did:plc:test{}", i)
                            } else {
                                format!("did:plc:missing{}", i)
                            };

                            if cache.get_user_profile(&did).await.is_some() {
                                hits += 1;
                            } else {
                                misses += 1;
                            }
                        }

                        let hit_rate = hits as f64 / (hits + misses) as f64;
                        assert!(hit_rate >= 0.45 && hit_rate <= 0.55);
                    });
                });
            },
        );
    }

    group.finish();
}

fn bench_serialization(c: &mut Criterion) {
    c.bench_function("serde_json_serialize_profile", |b| {
        let profile = create_test_profile(0);
        b.iter(|| {
            let _json = serde_json::to_string(&profile).unwrap();
        });
    });

    c.bench_function("serde_json_deserialize_profile", |b| {
        let json_str = serde_json::to_string(&create_test_profile(0)).unwrap();
        b.iter(|| {
            let _profile: BlueskyProfile = serde_json::from_str(&json_str).unwrap();
        });
    });

    c.bench_function("serde_json_serialize_message", |b| {
        let message = create_test_message(0);
        b.iter(|| {
            let _json = serde_json::to_string(&message).unwrap();
        });
    });

    c.bench_function("serde_json_deserialize_message", |b| {
        let json_str = serde_json::to_string(&create_test_message(0)).unwrap();
        b.iter(|| {
            let _message: JetstreamMessage = serde_json::from_str(&json_str).unwrap();
        });
    });

    c.bench_function("serde_json_serialize_enriched_record", |b| {
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
        b.iter(|| {
            let _json = serde_json::to_string(&record).unwrap();
        });
    });
}

fn bench_sqlite_operations(c: &mut Criterion) {
    let rt = tokio::runtime::Runtime::new().unwrap();

    c.bench_function("sqlite_store_record", |b| {
        let temp_dir = TempDir::new().unwrap();
        let db_path = temp_dir.path().join("test.db");
        
        let store = rt.block_on(async {
            SQLiteStore::new(&db_path).await.unwrap()
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

        b.iter(|| {
            rt.block_on(async {
                let _id = store.store_record(&record).await.unwrap();
            });
        });
    });

    c.bench_function("sqlite_batch_store", |b| {
        let temp_dir = TempDir::new().unwrap();
        let db_path = temp_dir.path().join("test.db");
        
        let store = rt.block_on(async {
            SQLiteStore::new(&db_path).await.unwrap()
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

        b.iter(|| {
            rt.block_on(async {
                for record in &records {
                    let _id = store.store_record(record).await.unwrap();
                }
            });
        });
    });

    let mut group = c.benchmark_group("sqlite_batch_sizes");

    for batch_size in [10, 50, 100, 500].iter() {
        group.bench_with_input(
            BenchmarkId::new("store", batch_size),
            batch_size,
            |b, &batch_size| {
                let temp_dir = TempDir::new().unwrap();
                let db_path = temp_dir.path().join("test.db");
                
                let store = rt.block_on(async {
                    SQLiteStore::new(&db_path).await.unwrap()
                });

                let records: Vec<EnrichedRecord> = (0..batch_size)
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

                b.iter(|| {
                    rt.block_on(async {
                        for record in &records {
                            let _id = store.store_record(record).await.unwrap();
                        }
                    });
                });
            },
        );
    }

    group.finish();
}

fn bench_enriched_record_creation(c: &mut Criterion) {
    c.bench_function("enriched_record_new", |b| {
        let message = create_test_message(0);
        b.iter(|| {
            let _record = EnrichedRecord::new(message.clone());
        });
    });

    c.bench_function("enriched_record_with_profile", |b| {
        let message = create_test_message(0);
        let profile = Arc::new(create_test_profile(0));
        
        b.iter(|| {
            let mut record = EnrichedRecord::new(message.clone());
            record.hydrated_metadata.author_profile = Some(profile.clone());
            record.metrics.cache_hits = 5;
            record.metrics.cache_misses = 2;
            record.calculate_cache_hit_rate();
            let _hit_rate = record.metrics.cache_hit_rate;
        });
    });

    c.bench_function("enriched_record_extract_uris", |b| {
        let message = create_test_message(0);
        let record = EnrichedRecord::new(message);
        
        b.iter(|| {
            let _at_uri = record.get_at_uri();
            let _did = record.get_did();
            let _text = record.get_text();
        });
    });
}

fn bench_batch_operations(c: &mut Criterion) {

    c.bench_function("batch_message_creation", |b| {
        b.iter(|| {
            let messages: Vec<JetstreamMessage> = (0..100)
                .map(|i| create_test_message(i))
                .collect();
            let _count = messages.len();
        });
    });

    c.bench_function("batch_enriched_record_creation", |b| {
        b.iter(|| {
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
            let _count = records.len();
        });
    });

    let mut group = c.benchmark_group("batch_profile_creation");

    for batch_size in [10, 25, 50, 100].iter() {
        group.bench_with_input(
            BenchmarkId::new("profiles", batch_size),
            batch_size,
            |b, &batch_size| {
                b.iter(|| {
                    let profiles: Vec<BlueskyProfile> = (0..batch_size)
                        .map(|i| create_test_profile(i))
                        .collect();
                    let _arc_profiles: Vec<Arc<BlueskyProfile>> = profiles
                        .into_iter()
                        .map(|p| Arc::new(p))
                        .collect();
                });
            },
        );
    }

    group.finish();
}

criterion_group!(
    benches,
    bench_cache_operations,
    bench_serialization,
    bench_sqlite_operations,
    bench_enriched_record_creation,
    bench_batch_operations
);
criterion_main!(benches);
