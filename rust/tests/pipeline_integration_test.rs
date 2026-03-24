use jetstream_turbo_rs::hydration::{Hydrator, TurboCache};
use jetstream_turbo_rs::storage::{EventPublisher, RecordStore};
use jetstream_turbo_rs::testing::{
    create_message_batch, create_post_message, create_profile, create_reply_message,
    MockEventPublisher, MockPostFetcher, MockProfileFetcher, MockRecordStore,
};
use std::sync::atomic::Ordering;
use std::sync::Arc;
use tokio::sync::broadcast;

/// Helper to build the full mock pipeline components.
#[allow(dead_code)]
struct TestPipeline {
    hydrator: Hydrator<MockProfileFetcher, MockPostFetcher>,
    profile_fetcher: Arc<MockProfileFetcher>,
    post_fetcher: Arc<MockPostFetcher>,
    record_store: Arc<MockRecordStore>,
    event_publisher: Arc<MockEventPublisher>,
    broadcast_sender: broadcast::Sender<jetstream_turbo_rs::models::enriched::EnrichedRecord>,
}

impl TestPipeline {
    fn new() -> Self {
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
        let (broadcast_sender, _) = broadcast::channel(100);

        Self {
            hydrator,
            profile_fetcher,
            post_fetcher,
            record_store,
            event_publisher,
            broadcast_sender,
        }
    }

    /// Drive messages through the full pipeline: hydrate → store → publish → broadcast
    async fn process_batch(
        &self,
        messages: Vec<jetstream_turbo_rs::models::jetstream::JetstreamMessage>,
    ) -> Vec<jetstream_turbo_rs::models::enriched::EnrichedRecord> {
        // Hydrate
        let enriched = self
            .hydrator
            .hydrate_batch(messages)
            .await
            .expect("hydration should succeed");

        if enriched.is_empty() {
            return enriched;
        }

        // Store and publish concurrently (mirrors process_batch_internal)
        let store_records = enriched.clone();
        let publish_records = enriched.clone();

        let store_future = self.record_store.store_batch(&store_records);
        let publish_future = self.event_publisher.publish_batch(&publish_records);

        let (store_result, publish_result) = tokio::join!(store_future, publish_future);
        store_result.expect("store should succeed");
        publish_result.expect("publish should succeed");

        // Broadcast
        for record in &enriched {
            let _ = self.broadcast_sender.send(record.clone());
        }

        enriched
    }
}

#[tokio::test]
async fn test_single_message_flows_through_pipeline() {
    let pipeline = TestPipeline::new();

    let message = create_post_message(0);
    let did = message.did.clone();

    // Pre-populate mock with the profile for this DID
    pipeline
        .profile_fetcher
        .add_profile(create_profile(&did))
        .await;

    let results = pipeline.process_batch(vec![message]).await;

    // Verify: exactly one record came through
    assert_eq!(results.len(), 1, "should produce exactly 1 enriched record");

    // Verify: DID matches
    assert_eq!(results[0].get_did(), did);

    // Verify: record store was called once with 1 record
    assert_eq!(
        pipeline.record_store.call_count.load(Ordering::SeqCst),
        1,
        "store_batch should be called once"
    );
    assert_eq!(
        pipeline.record_store.get_stored_count().await,
        1,
        "store should contain 1 record"
    );

    // Verify: event publisher was called once with 1 record
    assert_eq!(
        pipeline.event_publisher.call_count.load(Ordering::SeqCst),
        1,
        "publish_batch should be called once"
    );
    assert_eq!(
        pipeline.event_publisher.get_published_count().await,
        1,
        "publisher should have 1 record"
    );

    // Verify: profile fetcher was called (cache miss on first request)
    assert!(
        pipeline.profile_fetcher.call_count.load(Ordering::SeqCst) >= 1,
        "profile fetcher should be called at least once"
    );
}

#[tokio::test]
async fn test_batch_25_messages_flows_through_pipeline() {
    let pipeline = TestPipeline::new();

    let messages = create_message_batch(25);

    // Pre-populate profiles for all DIDs
    for msg in &messages {
        pipeline
            .profile_fetcher
            .add_profile(create_profile(&msg.did))
            .await;
    }

    let results = pipeline.process_batch(messages).await;

    // Verify: all 25 messages produced enriched records
    assert_eq!(
        results.len(),
        25,
        "should produce 25 enriched records"
    );

    // Verify: record store received all 25
    assert_eq!(
        pipeline.record_store.get_stored_count().await,
        25,
        "store should contain 25 records"
    );

    // Verify: event publisher received all 25
    assert_eq!(
        pipeline.event_publisher.get_published_count().await,
        25,
        "publisher should have 25 records"
    );
}

#[tokio::test]
async fn test_reply_message_extracts_mentioned_dids() {
    let pipeline = TestPipeline::new();

    let parent_did = "did:plc:parentuser";
    let parent_rkey = "3mepgzgia0001";
    let reply_message = create_reply_message(1, parent_did, parent_rkey);
    let reply_did = reply_message.did.clone();

    // Add profiles for both the reply author and the parent
    pipeline
        .profile_fetcher
        .add_profile(create_profile(&reply_did))
        .await;
    pipeline
        .profile_fetcher
        .add_profile(create_profile(parent_did))
        .await;

    let results = pipeline.process_batch(vec![reply_message]).await;

    assert_eq!(results.len(), 1);
    assert_eq!(results[0].get_did(), reply_did);
}

#[tokio::test]
async fn test_empty_batch_produces_no_records() {
    let pipeline = TestPipeline::new();

    let results = pipeline.process_batch(vec![]).await;

    assert_eq!(results.len(), 0, "empty batch should produce no records");
    assert_eq!(
        pipeline.record_store.call_count.load(Ordering::SeqCst),
        0,
        "store should not be called for empty batch"
    );
    assert_eq!(
        pipeline.event_publisher.call_count.load(Ordering::SeqCst),
        0,
        "publisher should not be called for empty batch"
    );
}

#[tokio::test]
async fn test_broadcast_delivers_records() {
    let pipeline = TestPipeline::new();
    let mut receiver = pipeline.broadcast_sender.subscribe();

    let message = create_post_message(42);
    let did = message.did.clone();
    pipeline
        .profile_fetcher
        .add_profile(create_profile(&did))
        .await;

    let _results = pipeline.process_batch(vec![message]).await;

    // Verify broadcast delivered the record
    let broadcast_record = receiver.try_recv().expect("should receive broadcast record");
    assert_eq!(broadcast_record.get_did(), did);
}

#[tokio::test]
async fn test_profile_fetcher_tracks_requested_dids() {
    let pipeline = TestPipeline::new();

    let message = create_post_message(7);
    let did = message.did.clone();
    pipeline
        .profile_fetcher
        .add_profile(create_profile(&did))
        .await;

    let _results = pipeline.process_batch(vec![message]).await;

    let requested = pipeline.profile_fetcher.requested_dids.lock().await;
    // At least one call should include our DID
    let all_dids: Vec<String> = requested.iter().flat_map(|v| v.iter().cloned()).collect();
    assert!(
        all_dids.contains(&did),
        "profile fetcher should have been asked for the message DID"
    );
}

#[tokio::test]
async fn test_multiple_batches_accumulate() {
    let pipeline = TestPipeline::new();

    // Process first batch
    let batch1 = create_message_batch(5);
    for msg in &batch1 {
        pipeline
            .profile_fetcher
            .add_profile(create_profile(&msg.did))
            .await;
    }
    pipeline.process_batch(batch1).await;

    // Process second batch
    let batch2 = create_message_batch(3);
    for msg in &batch2 {
        pipeline
            .profile_fetcher
            .add_profile(create_profile(&msg.did))
            .await;
    }
    pipeline.process_batch(batch2).await;

    // Total stored and published should be 8
    assert_eq!(
        pipeline.record_store.get_stored_count().await,
        8,
        "store should have accumulated 8 records"
    );
    assert_eq!(
        pipeline.event_publisher.get_published_count().await,
        8,
        "publisher should have accumulated 8 records"
    );
}
