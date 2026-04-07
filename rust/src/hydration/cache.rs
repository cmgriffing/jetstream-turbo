use crate::models::bluesky::{BlueskyPost, BlueskyProfile};
use ahash::RandomState;
use moka::sync::Cache as MokaCache;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::Duration;
use tracing::{instrument, trace};

#[derive(Clone)]
pub struct TurboCache {
    user_cache: MokaCache<String, Arc<BlueskyProfile>, RandomState>,
    post_cache: MokaCache<String, Arc<BlueskyPost>, RandomState>,
    user_capacity: usize,
    post_capacity: usize,
    metrics: Arc<CacheMetrics>,
}

#[derive(Debug, Default)]
pub struct CacheMetrics {
    pub user_hits: AtomicU64,
    pub user_misses: AtomicU64,
    pub post_hits: AtomicU64,
    pub post_misses: AtomicU64,
    pub total_requests: AtomicU64,
    pub cache_evictions: AtomicU64,
}

impl Clone for CacheMetrics {
    fn clone(&self) -> Self {
        Self {
            user_hits: AtomicU64::new(self.user_hits.load(Ordering::Relaxed)),
            user_misses: AtomicU64::new(self.user_misses.load(Ordering::Relaxed)),
            post_hits: AtomicU64::new(self.post_hits.load(Ordering::Relaxed)),
            post_misses: AtomicU64::new(self.post_misses.load(Ordering::Relaxed)),
            total_requests: AtomicU64::new(self.total_requests.load(Ordering::Relaxed)),
            cache_evictions: AtomicU64::new(self.cache_evictions.load(Ordering::Relaxed)),
        }
    }
}

impl TurboCache {
    pub fn new(user_cache_size: usize, post_cache_size: usize) -> Self {
        let metrics = Arc::new(CacheMetrics::default());

        let user_metrics = Arc::clone(&metrics);
        let user_cache = MokaCache::builder()
            .max_capacity(user_cache_size as u64)
            .time_to_live(Duration::from_secs(300))
            .eviction_listener(move |_k, _v, _cause| {
                user_metrics.cache_evictions.fetch_add(1, Ordering::Relaxed);
            })
            .build_with_hasher(RandomState::default());

        let post_metrics = Arc::clone(&metrics);
        let post_cache = MokaCache::builder()
            .max_capacity(post_cache_size as u64)
            .time_to_live(Duration::from_secs(300))
            .eviction_listener(move |_k, _v, _cause| {
                post_metrics.cache_evictions.fetch_add(1, Ordering::Relaxed);
            })
            .build_with_hasher(RandomState::default());

        Self {
            user_cache,
            post_cache,
            user_capacity: user_cache_size,
            post_capacity: post_cache_size,
            metrics,
        }
    }

    pub fn get_entry_counts(&self) -> (u64, u64) {
        (self.user_cache.entry_count(), self.post_cache.entry_count())
    }

    pub fn get_capacity_limits(&self) -> (usize, usize) {
        (self.user_capacity, self.post_capacity)
    }

    pub fn get_user_profile(&self, did: &str) -> Option<Arc<BlueskyProfile>> {
        if let Some(profile) = self.user_cache.get(did) {
            self.metrics.user_hits.fetch_add(1, Ordering::Relaxed);
            return Some(profile);
        }

        self.metrics.user_misses.fetch_add(1, Ordering::Relaxed);
        None
    }

    pub fn get_user_profiles(&self, dids: &[String]) -> Vec<Option<Arc<BlueskyProfile>>> {
        let mut profiles = Vec::with_capacity(dids.len());
        let mut hits = 0_u64;

        for did in dids {
            match self.user_cache.get(did) {
                Some(profile) => {
                    hits += 1;
                    profiles.push(Some(profile));
                }
                None => profiles.push(None),
            }
        }

        let total = dids.len() as u64;
        if hits > 0 {
            self.metrics.user_hits.fetch_add(hits, Ordering::Relaxed);
        }
        let misses = total.saturating_sub(hits);
        if misses > 0 {
            self.metrics
                .user_misses
                .fetch_add(misses, Ordering::Relaxed);
        }

        profiles
    }

    pub fn set_user_profile(&self, did: String, profile: Arc<BlueskyProfile>) {
        self.user_cache.insert(did.clone(), profile);
        trace!("Cached user profile: {}", did);
    }

    pub fn get_post(&self, uri: &str) -> Option<Arc<BlueskyPost>> {
        if let Some(post) = self.post_cache.get(uri) {
            self.metrics.post_hits.fetch_add(1, Ordering::Relaxed);
            return Some(post);
        }

        self.metrics.post_misses.fetch_add(1, Ordering::Relaxed);
        None
    }

    pub fn get_posts(&self, uris: &[String]) -> Vec<Option<Arc<BlueskyPost>>> {
        let mut posts = Vec::with_capacity(uris.len());
        let mut hits = 0_u64;

        for uri in uris {
            match self.post_cache.get(uri) {
                Some(post) => {
                    hits += 1;
                    posts.push(Some(post));
                }
                None => posts.push(None),
            }
        }

        let total = uris.len() as u64;
        if hits > 0 {
            self.metrics.post_hits.fetch_add(hits, Ordering::Relaxed);
        }
        let misses = total.saturating_sub(hits);
        if misses > 0 {
            self.metrics
                .post_misses
                .fetch_add(misses, Ordering::Relaxed);
        }

        posts
    }

    pub fn set_post(&self, uri: String, post: Arc<BlueskyPost>) {
        self.post_cache.insert(uri.clone(), post);
        trace!("Cached post: {}", uri);
    }

    #[instrument(name = "cache_check_profiles", skip(self), fields(count))]
    pub fn check_user_profiles_cached(&self, dids: &[String]) -> Vec<bool> {
        tracing::Span::current().record("count", dids.len());
        dids.iter()
            .map(|did| self.user_cache.contains_key(did))
            .collect()
    }

    #[instrument(name = "cache_check_posts", skip(self), fields(count))]
    pub fn check_posts_cached(&self, uris: &[String]) -> Vec<bool> {
        tracing::Span::current().record("count", uris.len());
        uris.iter()
            .map(|uri| self.post_cache.contains_key(uri))
            .collect()
    }

    pub fn get_metrics(&self) -> CacheMetricsSnapshot {
        let user_hits = self.metrics.user_hits.load(Ordering::Relaxed);
        let user_misses = self.metrics.user_misses.load(Ordering::Relaxed);
        let post_hits = self.metrics.post_hits.load(Ordering::Relaxed);
        let post_misses = self.metrics.post_misses.load(Ordering::Relaxed);

        CacheMetricsSnapshot {
            user_hits,
            user_misses,
            post_hits,
            post_misses,
            total_requests: user_hits + user_misses + post_hits + post_misses,
            cache_evictions: self.metrics.cache_evictions.load(Ordering::Relaxed),
        }
    }

    pub fn clear(&self) {
        self.user_cache.invalidate_all();
        self.post_cache.invalidate_all();
        trace!("Cleared all caches");
    }

    pub fn get_hit_rates(&self) -> (f64, f64) {
        let user_hits = self.metrics.user_hits.load(Ordering::Relaxed);
        let user_misses = self.metrics.user_misses.load(Ordering::Relaxed);
        let post_hits = self.metrics.post_hits.load(Ordering::Relaxed);
        let post_misses = self.metrics.post_misses.load(Ordering::Relaxed);

        let user_hit_rate = if user_hits + user_misses > 0 {
            user_hits as f64 / (user_hits + user_misses) as f64
        } else {
            0.0
        };

        let post_hit_rate = if post_hits + post_misses > 0 {
            post_hits as f64 / (post_hits + post_misses) as f64
        } else {
            0.0
        };

        (user_hit_rate, post_hit_rate)
    }
}

#[derive(Debug, Clone, Default)]
pub struct CacheMetricsSnapshot {
    pub user_hits: u64,
    pub user_misses: u64,
    pub post_hits: u64,
    pub post_misses: u64,
    pub total_requests: u64,
    pub cache_evictions: u64,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn test_user_profile_cache() {
        let cache = TurboCache::new(100, 100);

        let result = cache.get_user_profile("did:plc:test");
        assert!(result.is_none());

        let profile = BlueskyProfile {
            did: "did:plc:test".into(),
            handle: "test.bsky.social".to_string(),
            display_name: Some("Test User".to_string()),
            description: None,
            avatar: None,
            banner: None,
            followers_count: Some(0),
            follows_count: Some(0),
            posts_count: Some(0),
            indexed_at: None,
            created_at: None,
            labels: None,
        };

        cache.set_user_profile("did:plc:test".to_string(), Arc::new(profile.clone()));

        let result = cache.get_user_profile("did:plc:test");
        assert!(result.is_some());
        assert_eq!(result.unwrap().did.as_ref(), "did:plc:test");

        let metrics = cache.get_metrics();
        assert_eq!(metrics.user_hits, 1);
        assert_eq!(metrics.user_misses, 1);
    }

    #[tokio::test]
    async fn test_post_cache() {
        let cache = TurboCache::new(100, 100);

        let post = BlueskyPost {
            uri: "at://did:plc:test/app.bsky.feed.post/test".to_string(),
            cid: "bafyrei".to_string(),
            author: BlueskyProfile {
                did: "did:plc:test".into(),
                handle: "test.bsky.social".to_string(),
                display_name: None,
                description: None,
                avatar: None,
                banner: None,
                followers_count: Some(0),
                follows_count: Some(0),
                posts_count: Some(0),
                indexed_at: None,
                created_at: None,
                labels: None,
            },
            text: "Hello world".to_string(),
            created_at: chrono::Utc::now(),
            embed: None,
            reply: None,
            facets: None,
            labels: None,
            like_count: None,
            repost_count: None,
            reply_count: None,
        };

        cache.get_post("at://did:plc:test/app.bsky.feed.post/notfound");

        cache.set_post(
            "at://did:plc:test/app.bsky.feed.post/test".to_string(),
            Arc::new(post.clone()),
        );

        let result = cache.get_post("at://did:plc:test/app.bsky.feed.post/test");
        assert!(result.is_some());
        assert_eq!(result.unwrap().text, "Hello world");

        let metrics = cache.get_metrics();
        assert_eq!(metrics.post_hits, 1);
        assert_eq!(metrics.post_misses, 1);
    }

    #[tokio::test]
    async fn test_hit_rates() {
        let cache = TurboCache::new(10, 10);

        cache.get_user_profile("did:plc:test1");
        cache.get_user_profile("did:plc:test2");

        let profile = BlueskyProfile {
            did: "did:plc:test1".into(),
            handle: "test1.bsky.social".to_string(),
            display_name: None,
            description: None,
            avatar: None,
            banner: None,
            followers_count: Some(0),
            follows_count: Some(0),
            posts_count: Some(0),
            indexed_at: None,
            created_at: None,
            labels: None,
        };

        cache.set_user_profile("did:plc:test1".to_string(), Arc::new(profile));
        cache.get_user_profile("did:plc:test1");

        let (user_hit_rate, post_hit_rate) = cache.get_hit_rates();
        assert_eq!(user_hit_rate, 1.0 / 3.0);
        assert_eq!(post_hit_rate, 0.0);
    }
}
