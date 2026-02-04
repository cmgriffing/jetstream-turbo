use criterion::{criterion_group, criterion_main, Criterion, BenchmarkId};
use jetstream_turbo::hydration::TurboCache;
use jetstream_turbo::models::bluesky::BlueskyProfile;

fn bench_cache_operations(c: &mut Criterion) {
    let rt = tokio::runtime::Runtime::new().unwrap();
    
    c.bench_function("cache_user_profile_set", |b| {
        b.iter(|| {
            rt.block_on(async {
                let cache = TurboCache::new(10000, 10000);
                let profile = BlueskyProfile {
                    did: "did:plc:test".to_string(),
                    handle: "test.bsky.social".to_string(),
                    display_name: Some("Test User".to_string()),
                    description: None,
                    avatar: None,
                    banner: None,
                    followers_count: 1000,
                    follows_count: 500,
                    posts_count: 250,
                    indexed_at: None,
                    created_at: None,
                    labels: None,
                };
                
                for i in 0..1000 {
                    cache.set_user_profile(
                        format!("did:plc:test{}", i),
                        profile.clone()
                    ).await;
                }
            });
        });
    });
    
    c.bench_function("cache_user_profile_get", |b| {
        let cache = rt.block_on(async {
            let cache = TurboCache::new(10000, 10000);
            let profile = BlueskyProfile {
                did: "did:plc:test".to_string(),
                handle: "test.bsky.social".to_string(),
                display_name: Some("Test User".to_string()),
                description: None,
                avatar: None,
                banner: None,
                followers_count: 1000,
                follows_count: 500,
                posts_count: 250,
                indexed_at: None,
                created_at: None,
                labels: None,
            };
            
            // Pre-populate cache
            for i in 0..1000 {
                cache.set_user_profile(
                    format!("did:plc:test{}", i),
                    profile.clone()
                ).await;
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
    
    let mut group = c.benchmark_group("cache_hit_rates");
    
    for cache_size in [100, 1000, 10000].iter() {
        group.bench_with_input(
            BenchmarkId::new("hit_rate", cache_size),
            cache_size,
            |b, &cache_size| {
                b.iter(|| {
                    rt.block_on(async {
                        let cache = TurboCache::new(cache_size, cache_size);
                        let profile = BlueskyProfile {
                            did: "did:plc:test".to_string(),
                            handle: "test.bsky.social".to_string(),
                            display_name: Some("Test User".to_string()),
                            description: None,
                            avatar: None,
                            banner: None,
                            followers_count: 1000,
                            follows_count: 500,
                            posts_count: 250,
                            indexed_at: None,
                            created_at: None,
                            labels: None,
                        };
                        
                        // Fill cache halfway
                        for i in 0..(cache_size / 2) {
                            cache.set_user_profile(
                                format!("did:plc:test{}", i),
                                profile.clone()
                            ).await;
                        }
                        
                        // Test hit rate
                        let mut hits = 0;
                        let mut misses = 0;
                        
                        // 50% hits, 50% misses
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
                        assert!(hit_rate >= 0.45 && hit_rate <= 0.55); // Allow some variance
                    });
                });
            },
        );
    }
    
    group.finish();
}

criterion_group!(benches, bench_cache_operations);
criterion_main!(benches);