use crate::client::{PostFetcher, ProfileFetcher};
use crate::hydration::TurboCache;
use crate::models::{enriched::EnrichedRecord, jetstream::JetstreamMessage, TurboResult};
use std::sync::Arc;
use std::time::Instant;
use tracing::{info, trace};

pub struct Hydrator<P, Po> {
    cache: TurboCache,
    profile_fetcher: Arc<P>,
    post_fetcher: Arc<Po>,
}

impl<P, Po> Clone for Hydrator<P, Po> {
    fn clone(&self) -> Self {
        Self {
            cache: self.cache.clone(),
            profile_fetcher: Arc::clone(&self.profile_fetcher),
            post_fetcher: Arc::clone(&self.post_fetcher),
        }
    }
}

impl<P, Po> Hydrator<P, Po>
where
    P: ProfileFetcher + Send + Sync + 'static,
    Po: PostFetcher + Send + Sync + 'static,
{
    pub fn new(cache: TurboCache, profile_fetcher: Arc<P>, post_fetcher: Arc<Po>) -> Self {
        Self {
            cache,
            profile_fetcher,
            post_fetcher,
        }
    }

    pub async fn hydrate_message(&self, message: JetstreamMessage) -> TurboResult<EnrichedRecord> {
        let start_time = Instant::now();

        // Extract needed fields as borrowed/owned data before consuming the message
        let author_did_str = message.extract_did();
        let at_uri = message.extract_at_uri();
        let mentioned_dids = message
            .extract_mentioned_dids()
            .into_iter()
            .map(|s| s.to_string())
            .collect::<Vec<_>>();

        tracing::Span::current().record("did", author_did_str);
        if let Some(ref uri) = at_uri {
            tracing::Span::current().record("at_uri", uri);
        }

        // Consume the message without cloning
        let mut enriched = EnrichedRecord::new(message);

        // Hydrate author profile if this message has an at-uri (i.e., is a post)
        if at_uri.is_some() {
            // Use borrowed DID for cache lookup to avoid allocation
            let mut author_profile = self.cache.get_user_profile(author_did_str);

            let hit = author_profile.is_some();
            tracing::Span::current().record("cache_hit", hit);

            if !hit {
                // Allocate owned DID only once for the fetch request and cache insertion
                let author_did_owned = author_did_str.to_string();
                let profiles = self
                    .profile_fetcher
                    .bulk_fetch_profiles(&[author_did_owned.clone()])
                    .await?;

                if let Some(profile) = profiles.into_iter().next().flatten() {
                    let profile_arc = Arc::new(profile);
                    author_profile = Some(Arc::clone(&profile_arc));
                    self.cache
                        .set_user_profile(author_did_owned.clone(), profile_arc);
                }
            }

            enriched.hydrated_metadata.author_profile = author_profile;
        }

        // Process mentions
        for did in &mentioned_dids {
            if let Some(profile) = self.cache.get_user_profile(did) {
                enriched.hydrated_metadata.add_mentioned_profile(profile);
            }
        }

        // Update metrics
        enriched.metrics.hydration_time_ms = start_time.elapsed().as_millis() as u64;

        trace!("Hydrated message for DID: {}", author_did_str);
        Ok(enriched)
    }

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
        let cached_profile_flags = self.cache.check_user_profiles_cached(&dids);
        let cached_post_flags = self.cache.check_posts_cached(&uris);
        let cache_check_time = cache_check_start.elapsed().as_millis() as u64;
        tracing::Span::current().record("cache_check_time_ms", cache_check_time);

        // Partition uncached items: consume `dids` and `uris` vectors,
        // moving the strings directly into `uncached_dids`/`uncached_uris`.
        let uncached_dids: Vec<String> = dids
            .into_iter()
            .enumerate()
            .filter(|(i, _)| !cached_profile_flags[*i])
            .map(|(_, did)| did)
            .collect();

        let uncached_uris: Vec<String> = uris
            .into_iter()
            .enumerate()
            .filter(|(i, _)| !cached_post_flags[*i])
            .map(|(_, uri)| uri)
            .collect();

        // Fetch profiles and posts sequentially to avoid rate limiting
        let profiles_result = async {
            if uncached_dids.is_empty() {
                return Ok(vec![]);
            }
            self.profile_fetcher
                .bulk_fetch_profiles(&uncached_dids)
                .await
        }
        .await;

        let posts_result = async {
            if uncached_uris.is_empty() {
                return Ok(vec![]);
            }
            self.post_fetcher.bulk_fetch_posts(&uncached_uris).await
        }
        .await;

        let api_fetch_time = cache_check_start.elapsed().as_millis() as u64 - cache_check_time;
        tracing::Span::current().record("api_fetch_time_ms", api_fetch_time);

        if let Ok(profiles) = profiles_result {
            for (did, maybe_profile) in uncached_dids.iter().zip(profiles) {
                if let Some(profile) = maybe_profile {
                    self.cache.set_user_profile(did.clone(), Arc::new(profile));
                }
            }
        }

        if let Ok(posts) = posts_result {
            for (uri, maybe_post) in uncached_uris.iter().zip(posts) {
                if let Some(post) = maybe_post {
                    self.cache.set_post(uri.clone(), Arc::new(post));
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
        // Process messages sequentially. Since each hydration involves only cache lookups (no I/O)
        // in typical mock/benchmark scenarios, sequential processing avoids the overhead
        // of spawning concurrent tasks and can be faster for small batches.
        let mut results = Vec::with_capacity(messages.len());
        for message in messages {
            match self.hydrate_message(message).await {
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
