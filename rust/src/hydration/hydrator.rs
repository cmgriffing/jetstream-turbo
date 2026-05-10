use crate::client::{PostFetcher, ProfileFetcher};
use crate::hydration::resolver::CacheMissResolver;
use crate::hydration::TurboCache;
use crate::models::{
    enriched::EnrichedRecord,
    jetstream::JetstreamMessage,
    record_view::{FacetFeature, RecordView},
    TurboResult,
};
use crate::utils::serde_utils::string_utils::is_valid_at_uri;
use std::sync::Arc;
use std::time::Instant;
use tracing::{info, trace};

struct MessageContext {
    message: JetstreamMessage,
    author_did: String,
    is_post: bool,
    mentioned_dids: Vec<String>,
}

/// Extract mentioned DIDs from a record view.
///
/// Sources: reply parent/root URIs, mention facets, and embed record URIs.
/// All DIDs are parsed from AT-URIs (`at://did:plc:.../...`).
fn extract_mentioned_dids_from_view(rv: &RecordView<'_>) -> Vec<String> {
    let mut mentioned_dids = Vec::new();

    // From reply references
    if let Some(refs) = rv.reply_refs() {
        if let Some(did) = refs.parent_uri.and_then(extract_did_from_at_uri) {
            if did.starts_with("did:plc:") {
                mentioned_dids.push(did.to_string());
            }
        }
        if let Some(did) = refs.root_uri.and_then(extract_did_from_at_uri) {
            if did.starts_with("did:plc:") {
                mentioned_dids.push(did.to_string());
            }
        }
    }

    // From mention facets
    for facet in rv.facets() {
        for feature in facet.features() {
            if let FacetFeature::Mention { did } = feature {
                if did.starts_with("did:plc:") {
                    mentioned_dids.push(did.to_string());
                }
            }
        }
    }

    // From embed record
    if let Some(uri) = rv.embed_record_uri() {
        if let Some(did) = extract_did_from_at_uri(uri) {
            if did.starts_with("did:plc:") {
                mentioned_dids.push(did.to_string());
            }
        }
    }

    mentioned_dids.sort();
    mentioned_dids.dedup();
    mentioned_dids
}

/// Extract referenced post URIs from a record view.
///
/// Sources: reply parent/root URIs and embed record URIs.
/// All URIs are validated with `is_valid_at_uri`.
fn extract_post_uris_from_view(rv: &RecordView<'_>) -> Vec<String> {
    let mut uris = Vec::new();

    // From reply references
    if let Some(refs) = rv.reply_refs() {
        if let Some(uri) = refs.parent_uri {
            if !uri.is_empty() && is_valid_at_uri(uri) {
                uris.push(uri.to_string());
            }
        }
        if let Some(uri) = refs.root_uri {
            if !uri.is_empty() && is_valid_at_uri(uri) {
                uris.push(uri.to_string());
            }
        }
    }

    // From embed record
    if let Some(uri) = rv.embed_record_uri() {
        if !uri.is_empty() && is_valid_at_uri(uri) {
            uris.push(uri.to_string());
        }
    }

    uris.sort();
    uris.dedup();
    uris
}

/// Extract the DID from an AT-URI (`at://did:plc:abc123/...`).
#[inline(always)]
fn extract_did_from_at_uri(uri: &str) -> Option<&str> {
    uri.strip_prefix("at://")
        .and_then(|s| s.split('/').next())
}

pub struct Hydrator<P, Po> {
    resolver: CacheMissResolver<P, Po>,
}

impl<P, Po> Clone for Hydrator<P, Po> {
    fn clone(&self) -> Self {
        Self {
            resolver: self.resolver.clone(),
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
            resolver: CacheMissResolver::new(cache, profile_fetcher, post_fetcher),
        }
    }

    pub async fn hydrate_message(&self, message: JetstreamMessage) -> TurboResult<EnrichedRecord> {
        let author_did = message.extract_did().to_string();
        let at_uri = message.extract_at_uri();
        let is_post = at_uri.is_some();

        let mentioned_dids: Vec<String> = message
            .commit
            .as_ref()
            .and_then(|c| c.record.as_ref())
            .map(|r| extract_mentioned_dids_from_view(&RecordView::new(r)))
            .unwrap_or_default();

        self.hydrate_one(
            message,
            author_did,
            is_post,
            mentioned_dids,
            chrono::Utc::now(),
        )
        .await
    }

    async fn hydrate_one(
        &self,
        message: JetstreamMessage,
        author_did: String,
        is_post: bool,
        mentioned_dids: Vec<String>,
        processed_at: chrono::DateTime<chrono::Utc>,
    ) -> TurboResult<EnrichedRecord> {
        let start_time = Instant::now();

        tracing::Span::current().record("did", &author_did);

        let mut enriched = EnrichedRecord::new_with_timestamp(message, processed_at);

        if is_post {
            let at_uri = enriched.message.extract_at_uri();
            if let Some(ref uri) = at_uri {
                tracing::Span::current().record("at_uri", uri);
            }

            let author_profile = self.resolver.resolve_profile(&author_did).await?;
            let hit = author_profile.is_some();
            tracing::Span::current().record("cache_hit", hit);

            enriched.hydrated_metadata.author_profile = author_profile;
        }

        for did in &mentioned_dids {
            if let Some(profile) = self.resolver.cache().get_user_profile(did) {
                enriched.hydrated_metadata.add_mentioned_profile(profile);
            }
        }

        enriched.metrics.hydration_time_ms = start_time.elapsed().as_millis() as u64;
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
        let mut contexts = Vec::with_capacity(message_count);

        for message in messages {
            let author_did = message.extract_did().to_string();
            let is_post = message
                .commit
                .as_ref()
                .and_then(|c| c.collection.as_ref())
                .is_some();

            let (mentioned_dids, post_uris) = message
                .commit
                .as_ref()
                .and_then(|c| c.record.as_ref())
                .map(|r| {
                    let rv = RecordView::new(r);
                    (
                        extract_mentioned_dids_from_view(&rv),
                        extract_post_uris_from_view(&rv),
                    )
                })
                .unwrap_or_default();

            unique_dids.insert(author_did.clone());
            for did in &mentioned_dids {
                unique_dids.insert(did.clone());
            }
            for uri in &post_uris {
                unique_uris.insert(uri.clone());
            }

            contexts.push(MessageContext {
                message,
                author_did,
                is_post,
                mentioned_dids,
            });
        }

        let unique_dids_count = unique_dids.len();
        let unique_uris_count = unique_uris.len();
        tracing::Span::current().record("unique_dids", unique_dids_count);
        tracing::Span::current().record("unique_uris", unique_uris_count);

        let dids: Vec<String> = unique_dids.into_iter().collect();
        let uris: Vec<String> = unique_uris.into_iter().collect();

        let cache_check_start = Instant::now();
        self.resolver.resolve_profiles(&dids).await?;
        self.resolver.resolve_posts(&uris).await?;
        let api_fetch_time = cache_check_start.elapsed().as_millis() as u64;
        tracing::Span::current().record("api_fetch_time_ms", api_fetch_time);

        let hydrate_start = Instant::now();
        let results = self.hydrate_contexts(contexts).await;
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

    async fn hydrate_contexts(&self, contexts: Vec<MessageContext>) -> Vec<EnrichedRecord> {
        let mut results = Vec::with_capacity(contexts.len());
        let processed_at = chrono::Utc::now();
        for ctx in contexts {
            match self
                .hydrate_one(
                    ctx.message,
                    ctx.author_did,
                    ctx.is_post,
                    ctx.mentioned_dids,
                    processed_at,
                )
                .await
            {
                Ok(enriched) => results.push(enriched),
                Err(e) => {
                    trace!("Failed to hydrate message: {}", e);
                }
            }
        }
        results
    }

    pub fn get_cache(&self) -> &TurboCache {
        self.resolver.cache()
    }
}
