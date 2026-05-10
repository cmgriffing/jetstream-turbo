use crate::client::{PostFetcher, ProfileFetcher};
use crate::hydration::TurboCache;
use crate::models::bluesky::{BlueskyPost, BlueskyProfile};
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
    pub async fn resolve_profile(
        &self,
        did: &str,
    ) -> TurboResult<Option<Arc<BlueskyProfile>>> {
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
    pub async fn resolve_post(&self, uri: &str) -> TurboResult<Option<Arc<BlueskyPost>>> {
        if let Some(post) = self.cache.get_post(uri) {
            return Ok(Some(post));
        }

        let posts = self
            .post_fetcher
            .bulk_fetch_posts(&[uri.to_string()])
            .await?;

        if let Some(post) = posts.into_iter().next().flatten() {
            let post_arc = Arc::new(post);
            self.cache
                .set_post(uri.to_string(), Arc::clone(&post_arc));
            Ok(Some(post_arc))
        } else {
            Ok(None)
        }
    }

    // ---- Batch resolution ----

    /// Ensure all given profiles are in cache. Returns the number successfully
    /// fetched (cache hits are not counted).
    pub async fn resolve_profiles(&self, dids: &[String]) -> TurboResult<usize> {
        if dids.is_empty() {
            return Ok(0);
        }

        let cached_flags = self.cache.check_user_profiles_cached(dids);
        let uncached: Vec<String> = dids
            .iter()
            .enumerate()
            .filter(|(i, _)| !cached_flags[*i])
            .map(|(_, did)| did.clone())
            .collect();

        if uncached.is_empty() {
            return Ok(0);
        }

        let profiles = self.profile_fetcher.bulk_fetch_profiles(&uncached).await?;
        let mut resolved = 0;

        for (did, maybe_profile) in uncached.iter().zip(profiles) {
            if let Some(profile) = maybe_profile {
                self.cache.set_user_profile(did.clone(), Arc::new(profile));
                resolved += 1;
            }
        }

        trace!("Resolved {}/{} missing profiles", resolved, uncached.len());
        Ok(resolved)
    }

    /// Ensure all given posts are in cache.
    pub async fn resolve_posts(&self, uris: &[String]) -> TurboResult<usize> {
        if uris.is_empty() {
            return Ok(0);
        }

        let cached_flags = self.cache.check_posts_cached(uris);
        let uncached: Vec<String> = uris
            .iter()
            .enumerate()
            .filter(|(i, _)| !cached_flags[*i])
            .map(|(_, uri)| uri.clone())
            .collect();

        if uncached.is_empty() {
            return Ok(0);
        }

        let posts = self.post_fetcher.bulk_fetch_posts(&uncached).await?;
        let mut resolved = 0;

        for (uri, maybe_post) in uncached.iter().zip(posts) {
            if let Some(post) = maybe_post {
                self.cache.set_post(uri.clone(), Arc::new(post));
                resolved += 1;
            }
        }

        trace!("Resolved {}/{} missing posts", resolved, uncached.len());
        Ok(resolved)
    }

    /// Access the underlying cache (for reads after resolution).
    pub fn cache(&self) -> &TurboCache {
        &self.cache
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::models::bluesky::BlueskyProfile;
    use crate::testing::mocks::{MockPostFetcher, MockProfileFetcher};
    use std::sync::Arc;

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
        assert_eq!(resolved, 1);

        assert_eq!(profile_fetcher.call_count.load(std::sync::atomic::Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn resolve_profiles_empty_list_returns_zero() {
        let cache = TurboCache::new(10, 10);
        let resolver = CacheMissResolver::new(
            cache,
            Arc::new(MockProfileFetcher::new()),
            Arc::new(MockPostFetcher::new()),
        );

        let resolved = resolver.resolve_profiles(&[]).await.unwrap();
        assert_eq!(resolved, 0);
    }
}
