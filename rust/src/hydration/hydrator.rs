use crate::client::BlueskyClient;
use crate::hydration::TurboCache;
use crate::models::{enriched::EnrichedRecord, jetstream::JetstreamMessage, TurboResult};
use std::sync::Arc;
use std::time::Instant;
use tracing::{debug, info};

#[derive(Clone)]
pub struct Hydrator {
    cache: TurboCache,
    bluesky_client: Arc<BlueskyClient>,
}

impl Hydrator {
    pub fn new(cache: TurboCache, bluesky_client: Arc<BlueskyClient>) -> Self {
        Self {
            cache,
            bluesky_client,
        }
    }

    pub async fn hydrate_message(&self, message: JetstreamMessage) -> TurboResult<EnrichedRecord> {
        let start_time = Instant::now();
        let mut enriched = EnrichedRecord::new(message.clone());

        // Extract author DID and mentioned DIDs
        let author_did = message.extract_did();
        let mentioned_dids = message.extract_mentioned_dids();

        // Fetch author profile if not cached
        if let Some(_at_uri) = message.extract_at_uri() {
            let mut author_profile = self.cache.get_user_profile(author_did).await;

            if author_profile.is_none() {
                let profiles = self
                    .bluesky_client
                    .bulk_fetch_profiles(&[author_did.to_string()])
                    .await?;

                if let Some(profile) = profiles.into_iter().next().flatten() {
                    author_profile = Some(profile.clone());
                    self.cache
                        .set_user_profile(author_did.to_string(), profile)
                        .await;
                }
            }

            enriched.hydrated_metadata.author_profile = author_profile;
        }

        // Process mentions
        for did in &mentioned_dids {
            if let Some(profile) = self.cache.get_user_profile(did).await {
                enriched.hydrated_metadata.add_mentioned_profile(profile);
            }
        }

        // Update metrics
        enriched.metrics.hydration_time_ms = start_time.elapsed().as_millis() as u64;

        debug!("Hydrated message for DID: {}", author_did);
        Ok(enriched)
    }

    pub async fn hydrate_batch(
        &self,
        messages: Vec<JetstreamMessage>,
    ) -> TurboResult<Vec<EnrichedRecord>> {
        let start_time = Instant::now();

        let mut unique_dids = std::collections::HashSet::new();
        let mut unique_uris = std::collections::HashSet::new();

        for message in &messages {
            unique_dids.insert(message.extract_did().to_string());
            for did in message.extract_mentioned_dids() {
                unique_dids.insert(did.to_string());
            }
            for uri in message.extract_post_uris() {
                unique_uris.insert(uri);
            }
        }

        let dids: Vec<String> = unique_dids.into_iter().collect();
        let uris: Vec<String> = unique_uris.into_iter().collect();

        let cached_profile_flags = self.cache.check_user_profiles_cached(&dids).await;
        let cached_post_flags = self.cache.check_posts_cached(&uris).await;

        let uncached_dids: Vec<String> = dids
            .iter()
            .zip(cached_profile_flags)
            .filter(|(_, is_cached)| !*is_cached)
            .map(|(did, _)| did.clone())
            .collect();

        let uncached_uris: Vec<String> = uris
            .iter()
            .zip(cached_post_flags)
            .filter(|(_, is_cached)| !*is_cached)
            .map(|(uri, _)| uri.clone())
            .collect();

        let profiles_future = async {
            if uncached_dids.is_empty() {
                return Ok(vec![]);
            }
            self.bluesky_client
                .bulk_fetch_profiles(&uncached_dids)
                .await
        };

        let posts_future = async {
            if uncached_uris.is_empty() {
                return Ok(vec![]);
            }
            self.bluesky_client.bulk_fetch_posts(&uncached_uris).await
        };

        let (profiles_result, posts_result) = tokio::join!(profiles_future, posts_future);

        if let Ok(profiles) = profiles_result {
            for (did, maybe_profile) in uncached_dids.iter().zip(profiles) {
                if let Some(profile) = maybe_profile {
                    self.cache.set_user_profile(did.clone(), profile).await;
                }
            }
        }

        if let Ok(posts) = posts_result {
            for (uri, maybe_post) in uncached_uris.iter().zip(posts) {
                if let Some(post) = maybe_post {
                    self.cache.set_post(uri.clone(), post).await;
                }
            }
        }

        let results = self.hydrate_messages(messages).await;

        let elapsed = start_time.elapsed();
        info!(
            "Hydrated batch of {} messages in {:?}",
            results.len(),
            elapsed
        );

        Ok(results)
    }

    async fn hydrate_messages(&self, messages: Vec<JetstreamMessage>) -> Vec<EnrichedRecord> {
        use futures::stream::FuturesUnordered;
        use futures::StreamExt;

        let mut futures = FuturesUnordered::new();

        // Spawn all hydration tasks concurrently
        for message in messages {
            let hydrator = self.clone();
            futures.push(async move {
                hydrator.hydrate_message(message).await
            });
        }

        let mut results = Vec::with_capacity(futures.len());

        // Collect results as they complete
        while let Some(result) = futures.next().await {
            match result {
                Ok(enriched) => results.push(enriched),
                Err(e) => {
                    debug!("Failed to hydrate message: {}", e);
                }
            }
        }

        results
    }

    pub fn get_cache(&self) -> &TurboCache {
        &self.cache
    }
}
