use crate::client::BlueskyClient;
use crate::hydration::TurboCache;
use crate::models::{enriched::EnrichedRecord, jetstream::JetstreamMessage, TurboResult};
use std::sync::Arc;
use std::time::Instant;
use tracing::{info, instrument, trace};

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

    #[instrument(name = "hydrate_message", skip(self, message), fields(did, at_uri, cache_hit))]
    pub async fn hydrate_message(&self, message: JetstreamMessage) -> TurboResult<EnrichedRecord> {
        let start_time = Instant::now();
        let mut enriched = EnrichedRecord::new(message.clone());

        let author_did = message.extract_did();
        let at_uri = message.extract_at_uri().map(|s| s.to_string());

        tracing::Span::current().record("did", &author_did);
        if let Some(ref uri) = at_uri {
            tracing::Span::current().record("at_uri", uri);
        }

        let mentioned_dids = message.extract_mentioned_dids();

        if let Some(_at_uri) = message.extract_at_uri() {
            let mut author_profile = self.cache.get_user_profile(author_did).await;

            let hit = author_profile.is_some();
            tracing::Span::current().record("cache_hit", hit);

            if !hit {
                let profiles = self
                    .bluesky_client
                    .bulk_fetch_profiles(&[author_did.to_string()])
                    .await?;

                if let Some(profile) = profiles.into_iter().next().flatten() {
                    let profile_arc = Arc::new(profile);
                    author_profile = Some(Arc::clone(&profile_arc));
                    self.cache
                        .set_user_profile(author_did.to_string(), profile_arc)
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

        trace!("Hydrated message for DID: {}", author_did);
        Ok(enriched)
    }

    #[instrument(name = "hydrate_batch", skip(self, messages), fields(message_count, unique_dids, unique_uris, cache_check_time_ms, api_fetch_time_ms, hydrate_time_ms, total_time_ms))]
    pub async fn hydrate_batch(
        &self,
        messages: Vec<JetstreamMessage>,
    ) -> TurboResult<Vec<EnrichedRecord>> {
        let start_time = Instant::now();

        let message_count = messages.len();
        tracing::Span::current().record("message_count", message_count);

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

        let unique_dids_count = unique_dids.len();
        let unique_uris_count = unique_uris.len();
        tracing::Span::current().record("unique_dids", unique_dids_count);
        tracing::Span::current().record("unique_uris", unique_uris_count);

        let dids: Vec<String> = unique_dids.into_iter().collect();
        let uris: Vec<String> = unique_uris.into_iter().collect();

        let cache_check_start = Instant::now();
        let cached_profile_flags = self.cache.check_user_profiles_cached(&dids).await;
        let cached_post_flags = self.cache.check_posts_cached(&uris).await;
        let cache_check_time = cache_check_start.elapsed().as_millis() as u64;
        tracing::Span::current().record("cache_check_time_ms", cache_check_time);

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

        let api_fetch_time = cache_check_start.elapsed().as_millis() as u64 - cache_check_time;
        tracing::Span::current().record("api_fetch_time_ms", api_fetch_time);

        if let Ok(profiles) = profiles_result {
            for (did, maybe_profile) in uncached_dids.iter().zip(profiles) {
                if let Some(profile) = maybe_profile {
                    self.cache
                        .set_user_profile(did.clone(), Arc::new(profile))
                        .await;
                }
            }
        }

        if let Ok(posts) = posts_result {
            for (uri, maybe_post) in uncached_uris.iter().zip(posts) {
                if let Some(post) = maybe_post {
                    self.cache.set_post(uri.clone(), Arc::new(post)).await;
                }
            }
        }

        let hydrate_start = Instant::now();
        let results = self.hydrate_messages(messages).await;
        let hydrate_time = hydrate_start.elapsed().as_millis() as u64;
        tracing::Span::current().record("hydrate_time_ms", hydrate_time);

        let total_time = start_time.elapsed().as_millis() as u64;
        tracing::Span::current().record("total_time_ms", total_time);

        info!(
            "Hydrated batch of {} messages in {:?}",
            results.len(),
            total_time
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
            futures.push(async move { hydrator.hydrate_message(message).await });
        }

        let mut results = Vec::with_capacity(futures.len());

        // Collect results as they complete
        while let Some(result) = futures.next().await {
            match result {
                Ok(enriched) => results.push(enriched),
                Err(e) => {
                    trace!("Failed to hydrate message: {}", e);
                }
            }
        }

        results
    }

    pub fn get_cache(&self) -> &TurboCache {
        &self.cache
    }
}
