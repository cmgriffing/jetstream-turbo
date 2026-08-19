use crate::client::{PostFetchOutcome, PostFetcher, ProfileFetcher};
use crate::hydration::resolver::CacheMissResolver;
use crate::hydration::TurboCache;
use crate::models::{
    bluesky::BlueskyProfile,
    enriched::{EnrichedRecord, HydrationQuality, ReferencedPost},
    jetstream::JetstreamMessage,
    record_view::{FacetFeature, RecordView},
    TurboResult,
};
use crate::utils::serde_utils::string_utils::is_valid_at_uri;
use ahash::{AHashMap, AHashSet};
use std::sync::Arc;
use std::time::Instant;
use tracing::info;

struct MessageContext {
    message: JetstreamMessage,
    is_post: bool,
    /// Index into the batch's `dids` Vec (and the aligned resolved-profiles slice).
    author_index: u32,
    mentioned_indexes: Vec<u32>,
    post_uris: Vec<String>,
}

/// Extract mentioned DIDs and referenced post URIs from a record view in a
/// single traversal (reply refs, mention facets, embed record URI).
fn extract_refs_from_view(rv: &RecordView<'_>) -> (Vec<String>, Vec<String>) {
    let mut mentioned_dids = Vec::new();
    let mut uris = Vec::new();

    if let Some(refs) = rv.reply_refs() {
        if let Some(uri) = refs.parent_uri {
            if !uri.is_empty() && is_valid_at_uri(uri) {
                uris.push(uri.to_string());
            }
            if let Some(did) = extract_did_from_at_uri(uri) {
                if did.starts_with("did:plc:") {
                    mentioned_dids.push(did.to_string());
                }
            }
        }
        if let Some(uri) = refs.root_uri {
            if !uri.is_empty() && is_valid_at_uri(uri) {
                uris.push(uri.to_string());
            }
            if let Some(did) = extract_did_from_at_uri(uri) {
                if did.starts_with("did:plc:") {
                    mentioned_dids.push(did.to_string());
                }
            }
        }
    }

    if let Some(uri) = rv.embed_record_uri() {
        if !uri.is_empty() && is_valid_at_uri(uri) {
            uris.push(uri.to_string());
        }
        if let Some(did) = extract_did_from_at_uri(uri) {
            if did.starts_with("did:plc:") {
                mentioned_dids.push(did.to_string());
            }
        }
    }

    for facet in rv.facets() {
        for feature in facet.features() {
            if let FacetFeature::Mention { did } = feature {
                if did.starts_with("did:plc:") {
                    mentioned_dids.push(did.to_string());
                }
            }
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
        self.hydrate_batch(vec![message])
            .await?
            .into_iter()
            .next()
            .ok_or_else(|| {
                crate::models::TurboError::Internal("hydrator returned no record".to_string())
            })
    }

    // Private hot-path helper; arguments are intentionally explicit to avoid
    // an allocation or struct indirection per message.
    #[allow(clippy::too_many_arguments)]
    fn hydrate_one(
        &self,
        message: JetstreamMessage,
        is_post: bool,
        author_index: u32,
        mentioned_indexes: Vec<u32>,
        post_uris: Vec<String>,
        dids: &[Arc<str>],
        profiles: &[Option<Arc<BlueskyProfile>>],
        post_outcomes: &AHashMap<String, PostFetchOutcome>,
        processed_at: chrono::DateTime<chrono::Utc>,
        span: &tracing::Span,
    ) -> TurboResult<EnrichedRecord> {
        span.record("did", dids[author_index as usize].as_ref());

        let author_profile = if is_post {
            match &profiles[author_index as usize] {
                Some(profile) => {
                    span.record("cache_hit", true);
                    Some(Arc::clone(profile))
                }
                None => {
                    span.record("cache_hit", false);
                    None
                }
            }
        } else {
            None
        };

        let mut enriched = EnrichedRecord::new_with_timestamp(message, processed_at);
        enriched.hydrated_metadata.hydration_quality = HydrationQuality::Complete;
        enriched.hydrated_metadata.author_profile = author_profile;

        for index in mentioned_indexes {
            if let Some(profile) = &profiles[index as usize] {
                enriched
                    .hydrated_metadata
                    .add_mentioned_profile(Arc::clone(profile));
            }
        }

        for uri in post_uris {
            let outcome = post_outcomes.get(&uri).ok_or_else(|| {
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

        // Note: hydration_time_ms is intentionally left at its default (0). In the
        // batch design, cache-miss fetching happens inside resolve_profiles/resolve_posts
        // before hydrate_one runs, so a per-message Instant measurement here would
        // always round to 0 while costing a clock read per message.
        Ok(enriched)
    }

    pub async fn hydrate_batch(
        &self,
        messages: Vec<JetstreamMessage>,
    ) -> TurboResult<Vec<EnrichedRecord>> {
        let start_time = Instant::now();

        let message_count = messages.len();
        tracing::Span::current().record("message_count", message_count);

        let mut unique_uris = AHashSet::new();
        // dids in first-seen order; did_index assigns each unique did its position
        // so messages can attach resolved profiles by index (no per-message hashing).
        let mut dids: Vec<Arc<str>> = Vec::with_capacity(message_count);
        let mut did_index: hashbrown::HashMap<Arc<str>, u32, ahash::RandomState> =
            hashbrown::HashMap::with_capacity_and_hasher(
                message_count,
                ahash::RandomState::default(),
            );
        let mut contexts = Vec::with_capacity(message_count);

        for message in messages {
            let is_post = message
                .commit
                .as_ref()
                .and_then(|c| c.collection.as_ref())
                .is_some();

            let (mentioned_dids, post_uris) =
                match message.commit.as_ref().and_then(|c| c.record.as_ref()) {
                    // Records without reply/embed/facets need no tree build: the
                    // raw wire is scanned directly (fast substring checks).
                    Some(r)
                        if !r.raw().contains("\"reply\"")
                            && !r.raw().contains("\"embed\"")
                            && !r.raw().contains("\"facets\"") =>
                    {
                        (Vec::new(), Vec::new())
                    }
                    Some(r) => extract_refs_from_view(&RecordView::new(r.value())),
                    None => (Vec::new(), Vec::new()),
                };

            let author_index = {
                let (_, v) = did_index
                    .raw_entry_mut()
                    .from_key(message.extract_did())
                    .or_insert_with(|| {
                        let index = dids.len() as u32;
                        let arc: Arc<str> = Arc::from(message.extract_did());
                        dids.push(arc.clone());
                        (arc, index)
                    });
                *v
            };
            let mentioned_indexes = mentioned_dids
                .iter()
                .map(|did| {
                    let (_, v) = did_index
                        .raw_entry_mut()
                        .from_key(did.as_str())
                        .or_insert_with(|| {
                            let index = dids.len() as u32;
                            let arc: Arc<str> = Arc::from(did.as_str());
                            dids.push(arc.clone());
                            (arc, index)
                        });
                    *v
                })
                .collect();
            for uri in &post_uris {
                unique_uris.insert(uri.clone());
            }

            contexts.push(MessageContext {
                message,
                is_post,
                author_index,
                mentioned_indexes,
                post_uris,
            });
        }

        let unique_dids_count = dids.len();
        let unique_uris_count = unique_uris.len();
        tracing::Span::current().record("unique_dids", unique_dids_count);
        tracing::Span::current().record("unique_uris", unique_uris_count);

        let uris: Vec<String> = unique_uris.into_iter().collect();

        let cache_check_start = Instant::now();
        let profiles = self.resolver.resolve_profiles(&dids).await?;
        let post_outcomes = self.resolver.resolve_posts(&uris).await?;
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
            .collect::<AHashMap<_, _>>();
        let api_fetch_time = cache_check_start.elapsed().as_millis() as u64;
        tracing::Span::current().record("api_fetch_time_ms", api_fetch_time);

        let hydrate_start = Instant::now();
        let results = self.hydrate_contexts(contexts, &dids, &profiles, &post_outcomes)?;
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
        dids: &[Arc<str>],
        profiles: &[Option<Arc<BlueskyProfile>>],
        post_outcomes: &AHashMap<String, PostFetchOutcome>,
    ) -> TurboResult<Vec<EnrichedRecord>> {
        let mut results = Vec::with_capacity(contexts.len());
        let processed_at = chrono::Utc::now();
        let span = tracing::Span::current();
        for ctx in contexts {
            let enriched = self.hydrate_one(
                ctx.message,
                ctx.is_post,
                ctx.author_index,
                ctx.mentioned_indexes,
                ctx.post_uris,
                dids,
                profiles,
                post_outcomes,
                processed_at,
                &span,
            )?;
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
