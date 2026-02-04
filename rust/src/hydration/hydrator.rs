use std::sync::Arc;
use std::time::Instant;
use tracing::{debug, info};
use crate::models::{
    jetstream::JetstreamMessage,
    enriched::{EnrichedRecord, HydratedMetadata},
    bluesky::BlueskyProfile,
    errors::TurboError,
    TurboResult,
};
use crate::hydration::TurboCache;
use crate::client::BlueskyClient;

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
        if let Some(at_uri) = message.extract_at_uri() {
            let mut author_profile = self.cache.get_user_profile(author_did).await;
            
            if author_profile.is_none() {
                let profiles = self.bluesky_client
                    .bulk_fetch_profiles(&[author_did.to_string()])
                    .await?;
                
                if let Some(profile) = profiles.into_iter().next().flatten() {
                    author_profile = Some(profile.clone());
                    self.cache.set_user_profile(author_did.to_string(), profile).await;
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
    
    pub async fn hydrate_batch(&self, messages: Vec<JetstreamMessage>) -> TurboResult<Vec<EnrichedRecord>> {
        let start_time = Instant::now();
        let mut results = Vec::with_capacity(messages.len());
        
        // Collect all unique DIDs that need fetching
        let mut unique_dids = std::collections::HashSet::new();
        for message in &messages {
            unique_dids.insert(message.extract_did().to_string());
            for did in message.extract_mentioned_dids() {
                unique_dids.insert(did.to_string());
            }
        }
        
        let dids: Vec<String> = unique_dids.into_iter().collect();
        
        // Check which profiles are already cached
        let cached_flags = self.cache.check_user_profiles_cached(&dids).await;
        let mut uncached_dids = Vec::new();
        
        for (did, is_cached) in dids.iter().zip(cached_flags) {
            if !is_cached {
                uncached_dids.push(did.clone());
            }
        }
        
        // Bulk fetch uncached profiles
        if !uncached_dids.is_empty() {
            let profiles = self.bluesky_client.bulk_fetch_profiles(&uncached_dids).await?;
            
            for (did, maybe_profile) in uncached_dids.iter().zip(profiles) {
                if let Some(profile) = maybe_profile {
                    self.cache.set_user_profile(did.clone(), profile).await;
                }
            }
        }
        
        // Hydrate each message
        for message in messages {
            let enriched = self.hydrate_message(message).await?;
            results.push(enriched);
        }
        
        let elapsed = start_time.elapsed();
        info!("Hydrated batch of {} messages in {:?}", results.len(), elapsed);
        
        Ok(results)
    }
    
    pub fn get_cache(&self) -> &TurboCache {
        &self.cache
    }
}