use crate::client::{PostFetchOutcome, PostFetcher, ProfileFetcher};
use crate::hydration::resolver::CacheMissResolver;
use crate::hydration::TurboCache;
use crate::models::bluesky::BlueskyProfile;
use crate::models::{
    enriched::{EnrichedRecord, HydrationQuality, ReferencedPost},
    jetstream::JetstreamMessage,
    record_view::{FacetFeature, RecordView},
    TurboResult,
};
use crate::utils::serde_utils::string_utils::is_valid_at_uri;
use ahash::RandomState as AHashState;
use std::collections::{HashMap, HashSet};
use std::sync::Arc;
use std::time::Instant;
use tracing::info;

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, serde::Deserialize, serde::Serialize)]
#[serde(rename_all = "snake_case")]
pub enum HydrationExecutionMode {
    #[default]
    Sequential,
    Parallel,
}

struct MessageContext {
    message: JetstreamMessage,
    is_post: bool,
    /// Index into the batch's resolved-profiles vec for the author profile.
    profile_slot: usize,
    /// Indices into the resolved-profiles vec for mentioned DIDs.
    mentioned_slots: Vec<usize>,
    post_uris: Vec<String>,
}

/// Extract mentioned DIDs and referenced post URIs from a record view in a
/// single pass (the two kinds share the reply/embed traversals).
fn extract_refs_from_view(rv: &RecordView<'_>) -> (Vec<String>, Vec<String>) {
    let mut mentioned_dids = Vec::new();
    let mut uris = Vec::new();

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
        if !uri.is_empty() && is_valid_at_uri(uri) {
            uris.push(uri.to_string());
        }
    }

    mentioned_dids.sort();
    mentioned_dids.dedup();
    uris.sort();
    uris.dedup();
    (mentioned_dids, uris)
}

/// Extract the DID from an AT-URI (`at://did:plc:abc123/...`).
#[inline(always)]
fn extract_did_from_at_uri(uri: &str) -> Option<&str> {
    uri.strip_prefix("at://").and_then(|s| s.split('/').next())
}

/// Assign `did` a stable slot in the dedup'd did list (and resolved-profiles
/// vec), inserting it if new. Returns the slot index.
fn slot_for(
    slots: &mut HashMap<Arc<str>, usize, AHashState>,
    dids: &mut Vec<Arc<str>>,
    did: Arc<str>,
) -> usize {
    use std::collections::hash_map::Entry;
    match slots.entry(did) {
        Entry::Occupied(entry) => *entry.get(),
        Entry::Vacant(entry) => {
            let slot = dids.len();
            dids.push(Arc::clone(entry.key()));
            entry.insert(slot);
            slot
        }
    }
}

pub struct Hydrator<P, Po> {
    resolver: CacheMissResolver<P, Po>,
    execution_mode: HydrationExecutionMode,
}

impl<P, Po> Clone for Hydrator<P, Po> {
    fn clone(&self) -> Self {
        Self {
            resolver: self.resolver.clone(),
            execution_mode: self.execution_mode,
        }
    }
}

impl<P, Po> Hydrator<P, Po>
where
    P: ProfileFetcher + Send + Sync + 'static,
    Po: PostFetcher + Send + Sync + 'static,
{
    pub fn new(cache: TurboCache, profile_fetcher: Arc<P>, post_fetcher: Arc<Po>) -> Self {
        Self::new_with_mode(
            cache,
            profile_fetcher,
            post_fetcher,
            HydrationExecutionMode::Sequential,
        )
    }

    pub fn new_with_mode(
        cache: TurboCache,
        profile_fetcher: Arc<P>,
        post_fetcher: Arc<Po>,
        execution_mode: HydrationExecutionMode,
    ) -> Self {
        Self {
            resolver: CacheMissResolver::new(cache, profile_fetcher, post_fetcher),
            execution_mode,
        }
    }

    pub async fn hydrate_message(&self, message: JetstreamMessage) -> TurboResult<EnrichedRecord> {
        self.hydrate_batch(vec![message])
            .await?
            .into_iter()
            .next()
            .ok_or_else(|| {
                crate::models::TurboError::Internal("hydrator returned no record".to_string())
            })
    }

    #[allow(clippy::too_many_arguments)]
    fn hydrate_one(
        &self,
        message: JetstreamMessage,
        is_post: bool,
        profile_slot: usize,
        mentioned_slots: Vec<usize>,
        post_uris: Vec<String>,
        profiles: &[Option<Arc<BlueskyProfile>>],
        post_outcomes: &HashMap<String, PostFetchOutcome, AHashState>,
        processed_at: chrono::DateTime<chrono::Utc>,
        hydration_start: Instant,
    ) -> TurboResult<(EnrichedRecord, Instant)> {
        let mut enriched = EnrichedRecord::new_with_timestamp(message, processed_at);
        enriched.hydrated_metadata.hydration_quality = HydrationQuality::Complete;

        tracing::Span::current().record("did", enriched.message.extract_did());

        if is_post {
            let author_profile = profiles.get(profile_slot).cloned().flatten();
            let hit = author_profile.is_some();
            tracing::Span::current().record("cache_hit", hit);

            enriched.hydrated_metadata.author_profile = author_profile;
        }

        for slot in &mentioned_slots {
            if let Some(profile) = profiles.get(*slot).cloned().flatten() {
                enriched.hydrated_metadata.add_mentioned_profile(profile);
            }
        }

        for uri in post_uris {
            let outcome = post_outcomes.get(uri.as_str()).ok_or_else(|| {
                crate::models::TurboError::InvalidApiResponse(
                    "missing post outcome for requested URI".to_string(),
                )
            })?;
            match outcome {
                PostFetchOutcome::Found(post) => {
                    enriched
                        .hydrated_metadata
                        .add_referenced_post(ReferencedPost {
                            uri: post.uri.clone(),
                            cid: post.cid.clone(),
                            text: post.text.clone(),
                            author_did: Arc::clone(&post.author.did),
                            author_handle: Some(post.author.handle.clone()),
                            created_at: post.created_at,
                            reply_count: post.reply_count,
                            like_count: post.like_count,
                            repost_count: post.repost_count,
                        });
                }
                PostFetchOutcome::Missing => {}
                PostFetchOutcome::TemporarilyUnavailable(failure) => {
                    enriched.hydrated_metadata.add_degradation(failure.clone());
                }
            }
        }

        if enriched.hydrated_metadata.hydration_quality == HydrationQuality::Partial {
            self.resolver.cache().record_partial_record();
            metrics::counter!("optional_hydration_partial_records_total").increment(1);
        }

        // One monotonic read per message: sequential records share the boundary
        // instant, so this record's duration is measured from the previous
        // record's completion (or the batch start for the first).
        let hydration_end = Instant::now();
        enriched.metrics.hydration_time_ms =
            hydration_end.duration_since(hydration_start).as_millis() as u64;
        Ok((enriched, hydration_end))
    }

    pub async fn hydrate_batch(
        &self,
        messages: Vec<JetstreamMessage>,
    ) -> TurboResult<Vec<EnrichedRecord>> {
        let start_time = Instant::now();

        let message_count = messages.len();
        tracing::Span::current().record("message_count", message_count);

        let mut contexts = Vec::with_capacity(message_count);

        // Build contexts and dedup in a single pass, assigning each unique did
        // a slot into the resolved-profiles list so hydrate_one can index it
        // directly (no per-message hash lookup).
        let mut did_slots: HashMap<Arc<str>, usize, AHashState> =
            HashMap::with_capacity_and_hasher(contexts.len().max(1), AHashState::default());
        let mut dids: Vec<Arc<str>> = Vec::with_capacity(message_count);
        let mut unique_uris: HashSet<String, AHashState> =
            HashSet::with_hasher(AHashState::default());

        for message in messages {
            let is_post = message
                .commit
                .as_ref()
                .and_then(|c| c.collection.as_ref())
                .is_some();

            let (mentioned_dids, post_uris) = message
                .commit
                .as_ref()
                .and_then(|c| c.record.as_ref())
                .map(|r| extract_refs_from_view(&RecordView::new(r)))
                .unwrap_or_default();

            let profile_slot = slot_for(&mut did_slots, &mut dids, Arc::clone(&message.did));
            let mentioned_slots = mentioned_dids
                .into_iter()
                .map(|did| slot_for(&mut did_slots, &mut dids, Arc::from(did)))
                .collect();
            for uri in &post_uris {
                unique_uris.insert(uri.clone());
            }

            contexts.push(MessageContext {
                message,
                is_post,
                profile_slot,
                mentioned_slots,
                post_uris,
            });
        }

        let unique_dids_count = dids.len();
        let unique_uris_count = unique_uris.len();
        tracing::Span::current().record("unique_dids", unique_dids_count);
        tracing::Span::current().record("unique_uris", unique_uris_count);

        let uris: Vec<String> = unique_uris.into_iter().collect();

        let cache_check_start = Instant::now();
        // Aligned with `dids` (slot order); `Option::None` = profile does not exist.
        let (profiles, post_outcomes) = match self.execution_mode {
            HydrationExecutionMode::Sequential => {
                let profiles = self.resolver.resolve_profiles(&dids).await?;
                let posts = self.resolver.resolve_posts(&uris).await?;
                (profiles, posts)
            }
            HydrationExecutionMode::Parallel => {
                // These futures share the existing batch deadline imposed by the
                // orchestrator. They do not spawn tasks, so dropping this future
                // cancels both branches and releases coordinator guards normally.
                tokio::try_join!(
                    self.resolver.resolve_profiles(&dids),
                    self.resolver.resolve_posts(&uris)
                )?
            }
        };
        if post_outcomes.len() != uris.len() {
            return Err(crate::models::TurboError::InvalidApiResponse(format!(
                "post outcome cardinality mismatch: requested {}, received {}",
                uris.len(),
                post_outcomes.len()
            )));
        }
        let post_outcomes = uris
            .into_iter()
            .zip(post_outcomes)
            .collect::<HashMap<String, _, AHashState>>();
        let api_fetch_time = cache_check_start.elapsed().as_millis() as u64;
        tracing::Span::current().record("api_fetch_time_ms", api_fetch_time);

        let hydrate_start = Instant::now();
        let results = self.hydrate_contexts(contexts, &profiles, &post_outcomes)?;
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

    fn hydrate_contexts(
        &self,
        contexts: Vec<MessageContext>,
        profiles: &[Option<Arc<BlueskyProfile>>],
        post_outcomes: &HashMap<String, PostFetchOutcome, AHashState>,
    ) -> TurboResult<Vec<EnrichedRecord>> {
        let mut results = Vec::with_capacity(contexts.len());
        let processed_at = chrono::Utc::now();
        let mut hydration_start = Instant::now();
        for ctx in contexts {
            let (enriched, hydration_end) = self.hydrate_one(
                ctx.message,
                ctx.is_post,
                ctx.profile_slot,
                ctx.mentioned_slots,
                ctx.post_uris,
                profiles,
                post_outcomes,
                processed_at,
                hydration_start,
            )?;
            hydration_start = hydration_end;
            results.push(enriched);
        }
        Ok(results)
    }

    pub fn get_cache(&self) -> &TurboCache {
        self.resolver.cache()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::client::{BlueskyOperation, HydrationFailure, UpstreamFailureCategory};
    use crate::models::bluesky::BlueskyPost;
    use crate::testing::fixtures::{create_post_message, create_profile, create_reply_message};
    use crate::testing::mocks::{MockPostFetcher, MockProfileFetcher};

    fn test_post(uri: &str) -> BlueskyPost {
        BlueskyPost {
            uri: uri.to_string(),
            cid: "bafyreireferenced".to_string(),
            author: create_profile("did:plc:parent"),
            text: "referenced text".to_string(),
            created_at: chrono::Utc::now(),
            embed: None,
            reply: None,
            facets: None,
            labels: None,
            like_count: Some(2),
            repost_count: Some(1),
            reply_count: Some(3),
        }
    }

    fn unavailable() -> PostFetchOutcome {
        PostFetchOutcome::TemporarilyUnavailable(HydrationFailure {
            operation: BlueskyOperation::Posts,
            category: UpstreamFailureCategory::ServerError,
            status_class: Some("5xx".to_string()),
            attempts: 4,
            request_fingerprint: "safe-fingerprint".to_string(),
            isolation: None,
        })
    }

    #[tokio::test]
    async fn shared_unavailable_reference_marks_only_affected_records_partial() {
        let post_fetcher = Arc::new(MockPostFetcher::new());
        let uri = "at://did:plc:parent/app.bsky.feed.post/shared";
        post_fetcher
            .add_outcome(uri.to_string(), unavailable())
            .await;
        let hydrator = Hydrator::new(
            TurboCache::new(20, 20),
            Arc::new(MockProfileFetcher::new()),
            Arc::clone(&post_fetcher),
        );

        let records = hydrator
            .hydrate_batch(vec![
                create_reply_message(1, "did:plc:parent", "shared"),
                create_reply_message(2, "did:plc:parent", "shared"),
                create_post_message(3),
            ])
            .await
            .unwrap();

        assert_eq!(
            records[0].hydrated_metadata.hydration_quality,
            HydrationQuality::Partial
        );
        assert_eq!(
            records[1].hydrated_metadata.hydration_quality,
            HydrationQuality::Partial
        );
        assert_eq!(
            records[2].hydrated_metadata.hydration_quality,
            HydrationQuality::Complete
        );
        assert_eq!(
            post_fetcher.requested_uris.lock().await[0],
            vec![uri.to_string()]
        );
    }

    #[tokio::test]
    async fn found_reference_is_attached_to_record() {
        let post_fetcher = Arc::new(MockPostFetcher::new());
        let uri = "at://did:plc:parent/app.bsky.feed.post/found";
        post_fetcher.add_post(test_post(uri)).await;
        let hydrator = Hydrator::new(
            TurboCache::new(20, 20),
            Arc::new(MockProfileFetcher::new()),
            post_fetcher,
        );

        let record = hydrator
            .hydrate_message(create_reply_message(1, "did:plc:parent", "found"))
            .await
            .unwrap();

        assert_eq!(
            record.hydrated_metadata.hydration_quality,
            HydrationQuality::Complete
        );
        assert_eq!(record.hydrated_metadata.referenced_posts.len(), 1);
        assert_eq!(record.hydrated_metadata.referenced_posts[0].uri, uri);
    }
}
