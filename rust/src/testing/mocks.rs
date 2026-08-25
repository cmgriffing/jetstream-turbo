use crate::client::{MessageSource, PostFetchOutcome, PostFetcher, ProfileFetcher};
use crate::models::enriched::EnrichedRecord;
use crate::models::{
    bluesky::{BlueskyPost, BlueskyProfile},
    errors::TurboResult,
    jetstream::JetstreamMessage,
};
use crate::storage::{EventPublisher, RecordStore};
use futures::Stream;
use std::pin::Pin;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use tokio::sync::Mutex;

/// Mock implementation of `MessageSource` that yields a fixed set of messages.
pub struct MockMessageSource {
    messages: Mutex<Vec<JetstreamMessage>>,
    pub stream_messages_call_count: AtomicUsize,
}

impl MockMessageSource {
    pub fn new(messages: Vec<JetstreamMessage>) -> Self {
        Self {
            messages: Mutex::new(messages),
            stream_messages_call_count: AtomicUsize::new(0),
        }
    }
}

impl MessageSource for MockMessageSource {
    async fn stream_messages(
        &self,
    ) -> TurboResult<Pin<Box<dyn Stream<Item = TurboResult<JetstreamMessage>> + Send>>> {
        self.stream_messages_call_count
            .fetch_add(1, Ordering::SeqCst);
        let messages = self.messages.lock().await.drain(..).collect::<Vec<_>>();
        let stream = futures::stream::iter(messages.into_iter().map(Ok));
        Ok(Box::pin(stream))
    }
}

/// Mock implementation of `ProfileFetcher` that returns configurable profiles.
pub struct MockProfileFetcher {
    /// Profiles to return, keyed by DID.
    profiles: Mutex<std::collections::HashMap<String, Arc<BlueskyProfile>>>,
    pub call_count: AtomicUsize,
    pub requested_dids: Mutex<Vec<Vec<String>>>,
}

impl Default for MockProfileFetcher {
    fn default() -> Self {
        Self::new()
    }
}

impl MockProfileFetcher {
    pub fn new() -> Self {
        Self {
            profiles: Mutex::new(std::collections::HashMap::new()),
            call_count: AtomicUsize::new(0),
            requested_dids: Mutex::new(Vec::new()),
        }
    }

    pub async fn add_profile(&self, profile: BlueskyProfile) {
        let did = profile.did.to_string();
        self.profiles.lock().await.insert(did, Arc::new(profile));
    }
}

impl ProfileFetcher for MockProfileFetcher {
    async fn bulk_fetch_profiles(
        &self,
        dids: &[String],
    ) -> TurboResult<Vec<Option<Arc<BlueskyProfile>>>> {
        self.call_count.fetch_add(1, Ordering::SeqCst);
        self.requested_dids.lock().await.push(dids.to_vec());
        let mut profiles = self.profiles.lock().await;
        let results = dids
            .iter()
            .map(|did| profiles.remove(did.as_str()))
            .collect();
        Ok(results)
    }
}

/// Mock implementation of `PostFetcher` that returns configurable posts.
pub struct MockPostFetcher {
    posts: Mutex<std::collections::HashMap<String, Arc<BlueskyPost>>>,
    outcomes: Mutex<std::collections::HashMap<String, PostFetchOutcome>>,
    pub call_count: AtomicUsize,
    pub requested_uris: Mutex<Vec<Vec<String>>>,
}

impl Default for MockPostFetcher {
    fn default() -> Self {
        Self::new()
    }
}

impl MockPostFetcher {
    pub fn new() -> Self {
        Self {
            posts: Mutex::new(std::collections::HashMap::new()),
            outcomes: Mutex::new(std::collections::HashMap::new()),
            call_count: AtomicUsize::new(0),
            requested_uris: Mutex::new(Vec::new()),
        }
    }

    pub async fn add_post(&self, post: BlueskyPost) {
        let uri = post.uri.clone();
        self.posts.lock().await.insert(uri, Arc::new(post));
    }

    pub async fn add_outcome(&self, uri: String, outcome: PostFetchOutcome) {
        self.outcomes.lock().await.insert(uri, outcome);
    }
}

impl PostFetcher for MockPostFetcher {
    async fn bulk_fetch_posts(&self, uris: &[String]) -> TurboResult<Vec<PostFetchOutcome>> {
        self.call_count.fetch_add(1, Ordering::SeqCst);
        self.requested_uris.lock().await.push(uris.to_vec());
        let mut posts = self.posts.lock().await;
        let mut outcomes = self.outcomes.lock().await;
        let results = uris
            .iter()
            .map(|uri| {
                outcomes.remove(uri).unwrap_or_else(|| {
                    posts
                        .remove(uri.as_str())
                        .map(PostFetchOutcome::Found)
                        .unwrap_or(PostFetchOutcome::Missing)
                })
            })
            .collect();
        Ok(results)
    }
}

/// Mock implementation of `RecordStore` that stores records in memory.
pub struct MockRecordStore {
    pub stored_records: Mutex<Vec<EnrichedRecord>>,
    pub call_count: AtomicUsize,
    next_id: AtomicUsize,
}

impl Default for MockRecordStore {
    fn default() -> Self {
        Self::new()
    }
}

impl MockRecordStore {
    pub fn new() -> Self {
        Self {
            stored_records: Mutex::new(Vec::new()),
            call_count: AtomicUsize::new(0),
            next_id: AtomicUsize::new(1),
        }
    }

    pub async fn get_stored_count(&self) -> usize {
        self.stored_records.lock().await.len()
    }
}

impl RecordStore for MockRecordStore {
    async fn store_batch(&self, records: &[EnrichedRecord]) -> TurboResult<Vec<i64>> {
        self.call_count.fetch_add(1, Ordering::SeqCst);
        let mut stored = self.stored_records.lock().await;
        let mut ids = Vec::with_capacity(records.len());
        for record in records {
            let id = self.next_id.fetch_add(1, Ordering::SeqCst) as i64;
            stored.push(record.clone());
            ids.push(id);
        }
        Ok(ids)
    }
}

/// Mock implementation of `EventPublisher` that records published events.
pub struct MockEventPublisher {
    pub published_records: Mutex<Vec<EnrichedRecord>>,
    pub call_count: AtomicUsize,
    next_id: AtomicUsize,
}

impl Default for MockEventPublisher {
    fn default() -> Self {
        Self::new()
    }
}

impl MockEventPublisher {
    pub fn new() -> Self {
        Self {
            published_records: Mutex::new(Vec::new()),
            call_count: AtomicUsize::new(0),
            next_id: AtomicUsize::new(1),
        }
    }

    pub async fn get_published_count(&self) -> usize {
        self.published_records.lock().await.len()
    }
}

impl EventPublisher for MockEventPublisher {
    async fn publish_batch(&self, records: &[EnrichedRecord]) -> TurboResult<Vec<String>> {
        self.call_count.fetch_add(1, Ordering::SeqCst);
        let mut published = self.published_records.lock().await;
        let mut ids = Vec::with_capacity(records.len());
        for record in records {
            let id = self.next_id.fetch_add(1, Ordering::SeqCst);
            published.push(record.clone());
            ids.push(format!("{}-{}", record.processed_at.timestamp_millis(), id));
        }
        Ok(ids)
    }
}
