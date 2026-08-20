use crate::client::{PostFetchOutcome, PostFetcher, ProfileFetcher};
use crate::hydration::TurboCache;
use crate::models::bluesky::BlueskyProfile;
use crate::models::TurboResult;
use std::sync::Arc;
use tracing::trace;

/// Resolves cache misses for profiles and posts.
///
/// Encapsulates the "check cache → identify uncached → batch-fetch → populate cache"
/// pattern so callers only interact with the cache after resolution.
pub struct CacheMissResolver<P, Po> {
    cache: TurboCache,
    profile_fetcher: Arc<P>,
    post_fetcher: Arc<Po>,
}

impl<P, Po> Clone for CacheMissResolver<P, Po> {
    fn clone(&self) -> Self {
        Self {
            cache: self.cache.clone(),
            profile_fetcher: Arc::clone(&self.profile_fetcher),
            post_fetcher: Arc::clone(&self.post_fetcher),
        }
    }
}

impl<P, Po> CacheMissResolver<P, Po>
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

    // ---- Single-item resolution ----

    /// Ensure a single profile is in cache. Returns the profile if found or
    /// successfully fetched, or `None` if the profile does not exist.
    pub async fn resolve_profile(&self, did: &str) -> TurboResult<Option<Arc<BlueskyProfile>>> {
        if let Some(profile) = self.cache.get_user_profile(did) {
            return Ok(Some(profile));
        }

        let profiles = self
            .profile_fetcher
            .bulk_fetch_profiles(&[did.to_string()])
            .await?;

        if let Some(profile) = profiles.into_iter().next().flatten() {
            let profile_arc = Arc::new(profile);
            self.cache
                .set_user_profile(did.to_string(), Arc::clone(&profile_arc));
            Ok(Some(profile_arc))
        } else {
            Ok(None)
        }
    }

    /// Ensure a single post is in cache.
    pub async fn resolve_post(&self, uri: &str) -> TurboResult<PostFetchOutcome> {
        if let Some(post) = self.cache.get_post(uri) {
            return Ok(PostFetchOutcome::Found((*post).clone()));
        }
        if let Some(failure) = self.cache.get_unavailable_post(uri) {
            return Ok(PostFetchOutcome::TemporarilyUnavailable(failure));
        }

        let posts = self
            .post_fetcher
            .bulk_fetch_posts(&[uri.to_string()])
            .await?;

        if posts.len() != 1 {
            return Err(crate::models::TurboError::InvalidApiResponse(format!(
                "post outcome cardinality mismatch: requested 1, received {}",
                posts.len()
            )));
        }
        let outcome = posts.into_iter().next().expect("length checked");
        match &outcome {
            PostFetchOutcome::Found(post) => {
                self.cache.set_post(uri.to_string(), Arc::new(post.clone()));
            }
            PostFetchOutcome::Missing => {
                self.cache.complete_post_resolution(uri, "missing");
            }
            PostFetchOutcome::TemporarilyUnavailable(failure) => {
                self.cache
                    .set_unavailable_post(uri.to_string(), failure.clone());
            }
        }
        Ok(outcome)
    }

    // ---- Batch resolution ----

    /// Ensure all given profiles are in cache. Returns one profile per DID
    /// (cache hit or freshly fetched), aligned with `dids`; `None` when a
    /// profile does not exist.
    pub async fn resolve_profiles<S: AsRef<str>>(
        &self,
        dids: &[S],
    ) -> TurboResult<Vec<Option<Arc<BlueskyProfile>>>> {
        if dids.is_empty() {
            return Ok(Vec::new());
        }

        let mut profiles = self.cache.get_user_profiles(dids);
        let mut uncached = Vec::new();
        let mut uncached_indexes = Vec::new();
        for (index, profile) in profiles.iter().enumerate() {
            if profile.is_none() {
                uncached.push(dids[index].as_ref().to_string());
                uncached_indexes.push(index);
            }
        }

        if uncached.is_empty() {
            return Ok(profiles);
        }

        let fetched = self.profile_fetcher.bulk_fetch_profiles(&uncached).await?;
        let mut resolved = 0;
        for ((did, index), fetched) in uncached.iter().zip(uncached_indexes).zip(fetched) {
            if let Some(profile) = fetched {
                let profile_arc = Arc::new(profile);
                self.cache
                    .set_user_profile(did.clone(), Arc::clone(&profile_arc));
                profiles[index] = Some(profile_arc);
                resolved += 1;
            }
        }

        trace!("Resolved {}/{} missing profiles", resolved, uncached.len());
        Ok(profiles)
    }

    /// Resolve every URI to exactly one ordered outcome.
    pub async fn resolve_posts<S: AsRef<str>>(
        &self,
        uris: &[S],
    ) -> TurboResult<Vec<PostFetchOutcome>> {
        if uris.is_empty() {
            return Ok(Vec::new());
        }

        let mut outcomes = vec![None; uris.len()];
        let mut uncached = Vec::new();
        let mut uncached_indexes = Vec::new();
        for (index, uri) in uris.iter().enumerate() {
            if let Some(post) = self.cache.get_post(uri.as_ref()) {
                outcomes[index] = Some(PostFetchOutcome::Found((*post).clone()));
            } else if let Some(failure) = self.cache.get_unavailable_post(uri.as_ref()) {
                outcomes[index] = Some(PostFetchOutcome::TemporarilyUnavailable(failure));
            } else {
                uncached.push(uri.as_ref().to_string());
                uncached_indexes.push(index);
            }
        }

        if uncached.is_empty() {
            return Ok(outcomes.into_iter().flatten().collect());
        }

        let fetched = self.post_fetcher.bulk_fetch_posts(&uncached).await?;
        if fetched.len() != uncached.len() {
            return Err(crate::models::TurboError::InvalidApiResponse(format!(
                "post outcome cardinality mismatch: requested {}, received {}",
                uncached.len(),
                fetched.len()
            )));
        }

        for ((uri, index), outcome) in uncached.iter().zip(uncached_indexes).zip(fetched) {
            self.cache.record_post_outcome(&outcome);
            match &outcome {
                PostFetchOutcome::Found(post) => {
                    self.cache.set_post(uri.clone(), Arc::new(post.clone()));
                    metrics::counter!("optional_hydration_post_outcomes_total", "outcome" => "found")
                        .increment(1);
                }
                PostFetchOutcome::Missing => {
                    self.cache.complete_post_resolution(uri, "missing");
                    metrics::counter!("optional_hydration_post_outcomes_total", "outcome" => "missing")
                        .increment(1);
                }
                PostFetchOutcome::TemporarilyUnavailable(failure) => {
                    self.cache
                        .set_unavailable_post(uri.clone(), failure.clone());
                    metrics::counter!(
                        "optional_hydration_post_outcomes_total",
                        "outcome" => "temporarily_unavailable",
                        "category" => failure.category.as_str(),
                        "status_class" => failure.status_class.clone().unwrap_or_else(|| "none".to_string()),
                        "isolation" => failure.isolation.as_ref().map_or("none", |value| value.as_str()),
                    )
                    .increment(1);
                    tracing::warn!(
                        operation = failure.operation.as_str(),
                        category = failure.category.as_str(),
                        status_class = failure.status_class.as_deref(),
                        attempts = failure.attempts,
                        request_fingerprint = failure.request_fingerprint,
                        isolation = failure.isolation.as_ref().map(|value| value.as_str()),
                        "Optional referenced-post hydration degraded"
                    );
                }
            }
            outcomes[index] = Some(outcome);
        }

        trace!("Resolved {} missing post outcomes", uncached.len());
        Ok(outcomes
            .into_iter()
            .map(|outcome| outcome.expect("every URI receives an outcome"))
            .collect())
    }

    /// Access the underlying cache (for reads after resolution).
    pub fn cache(&self) -> &TurboCache {
        &self.cache
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::client::{BlueskyOperation, HydrationFailure, UpstreamFailureCategory};
    use crate::models::bluesky::{BlueskyPost, BlueskyProfile};
    use crate::testing::mocks::{MockPostFetcher, MockProfileFetcher};
    use std::sync::{Arc, Mutex};
    use std::time::{Duration, Instant};

    struct CardinalityMismatchFetcher;

    impl PostFetcher for CardinalityMismatchFetcher {
        async fn bulk_fetch_posts(&self, _uris: &[String]) -> TurboResult<Vec<PostFetchOutcome>> {
            Ok(Vec::new())
        }
    }

    fn test_profile(did: &str) -> BlueskyProfile {
        BlueskyProfile {
            did: Arc::from(did),
            handle: format!("{}.bsky.social", &did[8..]),
            display_name: Some(format!("User {}", &did[8..])),
            description: None,
            avatar: None,
            banner: None,
            followers_count: Some(0),
            follows_count: Some(0),
            posts_count: Some(0),
            indexed_at: None,
            created_at: None,
            labels: None,
        }
    }

    fn test_post(uri: &str) -> BlueskyPost {
        BlueskyPost {
            uri: uri.to_string(),
            cid: "cid".to_string(),
            author: test_profile("did:plc:author"),
            text: "recovered".to_string(),
            created_at: chrono::Utc::now(),
            embed: None,
            reply: None,
            facets: None,
            labels: None,
            like_count: None,
            repost_count: None,
            reply_count: None,
        }
    }

    #[tokio::test]
    async fn resolve_profile_cache_hit() {
        let cache = TurboCache::new(10, 10);
        let profile = test_profile("did:plc:alice");
        cache.set_user_profile("did:plc:alice".to_string(), Arc::new(profile.clone()));

        let profile_fetcher = Arc::new(MockProfileFetcher::new());
        let post_fetcher = Arc::new(MockPostFetcher::new());
        let resolver = CacheMissResolver::new(cache, profile_fetcher, post_fetcher);

        let result = resolver.resolve_profile("did:plc:alice").await.unwrap();
        assert!(result.is_some());
        assert_eq!(result.unwrap().did.as_ref(), "did:plc:alice");
    }

    #[tokio::test]
    async fn resolve_profile_cache_miss_fetches() {
        let cache = TurboCache::new(10, 10);
        let profile = test_profile("did:plc:bob");

        let profile_fetcher = Arc::new(MockProfileFetcher::new());
        profile_fetcher.add_profile(profile.clone()).await;
        let post_fetcher = Arc::new(MockPostFetcher::new());
        let resolver = CacheMissResolver::new(cache, profile_fetcher, post_fetcher);

        let result = resolver.resolve_profile("did:plc:bob").await.unwrap();
        assert!(result.is_some());

        // Should now be cached
        let cached = resolver.cache().get_user_profile("did:plc:bob");
        assert!(cached.is_some());
    }

    #[tokio::test]
    async fn resolve_profile_returns_none_for_missing() {
        let cache = TurboCache::new(10, 10);
        let profile_fetcher = Arc::new(MockProfileFetcher::new());
        let post_fetcher = Arc::new(MockPostFetcher::new());
        let resolver = CacheMissResolver::new(cache, profile_fetcher, post_fetcher);

        let result = resolver.resolve_profile("did:plc:ghost").await.unwrap();
        assert!(result.is_none());
    }

    #[tokio::test]
    async fn resolve_profiles_skips_cached_dids() {
        let cache = TurboCache::new(10, 10);
        let profile = test_profile("did:plc:alice");
        cache.set_user_profile("did:plc:alice".to_string(), Arc::new(profile));

        let bob = test_profile("did:plc:bob");
        let profile_fetcher = Arc::new(MockProfileFetcher::new());
        profile_fetcher.add_profile(bob).await;
        let post_fetcher = Arc::new(MockPostFetcher::new());
        let resolver = CacheMissResolver::new(cache, Arc::clone(&profile_fetcher), post_fetcher);

        let dids = vec!["did:plc:alice".to_string(), "did:plc:bob".to_string()];
        let resolved = resolver.resolve_profiles(&dids).await.unwrap();
        assert_eq!(resolved.len(), 2);
        assert!(resolved.iter().all(|p| p.is_some()));

        assert_eq!(
            profile_fetcher
                .call_count
                .load(std::sync::atomic::Ordering::SeqCst),
            1
        );
    }

    #[tokio::test]
    async fn resolve_profiles_empty_list_returns_zero() {
        let cache = TurboCache::new(10, 10);
        let resolver = CacheMissResolver::new(
            cache,
            Arc::new(MockProfileFetcher::new()),
            Arc::new(MockPostFetcher::new()),
        );

        let empty_dids: Vec<String> = Vec::new();
        let resolved = resolver.resolve_profiles(&empty_dids).await.unwrap();
        assert!(resolved.is_empty());
    }

    #[tokio::test]
    async fn resolve_posts_rejects_outcome_cardinality_mismatch() {
        let resolver = CacheMissResolver::new(
            TurboCache::new(10, 10),
            Arc::new(MockProfileFetcher::new()),
            Arc::new(CardinalityMismatchFetcher),
        );

        let error = resolver
            .resolve_posts(&["at://did:plc:test/app.bsky.feed.post/one".to_string()])
            .await
            .unwrap_err();
        assert!(matches!(
            error,
            crate::models::TurboError::InvalidApiResponse(_)
        ));
    }

    #[tokio::test]
    async fn negative_cache_suppresses_upstream_then_recovers_after_expiry() {
        let start = Instant::now();
        let now = Arc::new(Mutex::new(start));
        let clock_now = Arc::clone(&now);
        let cache = TurboCache::new_with_clock(
            10,
            10,
            10,
            Duration::from_secs(30),
            Arc::new(move || *clock_now.lock().unwrap()),
        );
        let post_fetcher = Arc::new(MockPostFetcher::new());
        let uri = "at://did:plc:test/app.bsky.feed.post/recovery";
        post_fetcher
            .add_outcome(
                uri.to_string(),
                PostFetchOutcome::TemporarilyUnavailable(HydrationFailure {
                    operation: BlueskyOperation::Posts,
                    category: UpstreamFailureCategory::ServerError,
                    status_class: Some("5xx".to_string()),
                    attempts: 4,
                    request_fingerprint: "safe-fingerprint".to_string(),
                    isolation: None,
                }),
            )
            .await;
        let resolver = CacheMissResolver::new(
            cache,
            Arc::new(MockProfileFetcher::new()),
            Arc::clone(&post_fetcher),
        );

        assert!(matches!(
            resolver.resolve_posts(&[uri.to_string()]).await.unwrap()[0],
            PostFetchOutcome::TemporarilyUnavailable(_)
        ));
        assert!(matches!(
            resolver.resolve_posts(&[uri.to_string()]).await.unwrap()[0],
            PostFetchOutcome::TemporarilyUnavailable(_)
        ));
        assert_eq!(
            post_fetcher
                .call_count
                .load(std::sync::atomic::Ordering::SeqCst),
            1
        );

        *now.lock().unwrap() = start + Duration::from_secs(30);
        post_fetcher.add_post(test_post(uri)).await;
        assert!(matches!(
            resolver.resolve_posts(&[uri.to_string()]).await.unwrap()[0],
            PostFetchOutcome::Found(_)
        ));
        assert_eq!(
            post_fetcher
                .call_count
                .load(std::sync::atomic::Ordering::SeqCst),
            2
        );
        assert_eq!(resolver.cache().get_metrics().post_recoveries, 1);
    }
}
