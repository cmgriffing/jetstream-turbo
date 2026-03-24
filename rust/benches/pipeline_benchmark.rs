use criterion::{criterion_group, criterion_main, Criterion};
use jetstream_turbo_rs::hydration::{Hydrator, TurboCache};
use jetstream_turbo_rs::storage::{EventPublisher, RecordStore};
use jetstream_turbo_rs::testing::{
    create_message_batch, create_post_message, create_profile, MockEventPublisher,
    MockPostFetcher, MockProfileFetcher, MockRecordStore,
};
use std::sync::Arc;
use tokio::runtime::Runtime;
use tokio::sync::broadcast;

fn build_hydrator(
    profile_fetcher: Arc<MockProfileFetcher>,
    post_fetcher: Arc<MockPostFetcher>,
) -> Hydrator<MockProfileFetcher, MockPostFetcher> {
    let cache = TurboCache::new(1000, 1000);
    Hydrator::new(cache, profile_fetcher, post_fetcher)
}

fn bench_single_message_hydration(c: &mut Criterion) {
    let rt = Runtime::new().unwrap();

    c.bench_function("single_message_hydration", |b| {
        b.iter(|| {
            rt.block_on(async {
                let profile_fetcher = Arc::new(MockProfileFetcher::new());
                let post_fetcher = Arc::new(MockPostFetcher::new());

                let message = create_post_message(0);
                let did = message.did.clone();
                profile_fetcher
                    .add_profile(create_profile(&did))
                    .await;

                let hydrator = build_hydrator(profile_fetcher, post_fetcher);
                hydrator.hydrate_message(message).await.unwrap()
            })
        });
    });
}

fn bench_batch_hydration_25(c: &mut Criterion) {
    let rt = Runtime::new().unwrap();

    c.bench_function("batch_hydration_25_messages", |b| {
        b.iter(|| {
            rt.block_on(async {
                let profile_fetcher = Arc::new(MockProfileFetcher::new());
                let post_fetcher = Arc::new(MockPostFetcher::new());

                let messages = create_message_batch(25);
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

fn bench_full_pipeline_single(c: &mut Criterion) {
    let rt = Runtime::new().unwrap();

    c.bench_function("full_pipeline_single_message", |b| {
        b.iter(|| {
            rt.block_on(async {
                let profile_fetcher = Arc::new(MockProfileFetcher::new());
                let post_fetcher = Arc::new(MockPostFetcher::new());
                let record_store = Arc::new(MockRecordStore::new());
                let event_publisher = Arc::new(MockEventPublisher::new());
                let (broadcast_sender, _) = broadcast::channel(100);

                let message = create_post_message(0);
                let did = message.did.clone();
                profile_fetcher
                    .add_profile(create_profile(&did))
                    .await;

                let hydrator = build_hydrator(
                    Arc::clone(&profile_fetcher),
                    Arc::clone(&post_fetcher),
                );

                // Hydrate
                let enriched = hydrator.hydrate_batch(vec![message]).await.unwrap();
                // Store
                record_store.store_batch(&enriched).await.unwrap();
                // Publish
                event_publisher.publish_batch(&enriched).await.unwrap();
                // Broadcast
                for record in &enriched {
                    let _ = broadcast_sender.send(record.clone());
                }
                enriched
            })
        });
    });
}

fn bench_full_pipeline_batch_25(c: &mut Criterion) {
    let rt = Runtime::new().unwrap();

    c.bench_function("full_pipeline_batch_25", |b| {
        b.iter(|| {
            rt.block_on(async {
                let profile_fetcher = Arc::new(MockProfileFetcher::new());
                let post_fetcher = Arc::new(MockPostFetcher::new());
                let record_store = Arc::new(MockRecordStore::new());
                let event_publisher = Arc::new(MockEventPublisher::new());
                let (broadcast_sender, _) = broadcast::channel(100);

                let messages = create_message_batch(25);
                for msg in &messages {
                    profile_fetcher
                        .add_profile(create_profile(&msg.did))
                        .await;
                }

                let hydrator = build_hydrator(
                    Arc::clone(&profile_fetcher),
                    Arc::clone(&post_fetcher),
                );

                // Hydrate
                let enriched = hydrator.hydrate_batch(messages).await.unwrap();
                // Store
                record_store.store_batch(&enriched).await.unwrap();
                // Publish
                event_publisher.publish_batch(&enriched).await.unwrap();
                // Broadcast
                for record in &enriched {
                    let _ = broadcast_sender.send(record.clone());
                }
                enriched
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
);
criterion_main!(benches);
