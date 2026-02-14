use crate::models::bluesky::{BlueskyPost, BlueskyProfile};
use lru::LruCache;
use std::num::NonZeroUsize;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use tokio::sync::Mutex;
use tracing::trace;

#[derive(Clone)]
pub struct TurboCache {
    user_cache: Arc<Mutex<LruCache<String, Arc<BlueskyProfile>>>>,
    post_cache: Arc<Mutex<LruCache<String, Arc<BlueskyPost>>>>,
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
        let user_size = NonZeroUsize::new(user_cache_size).unwrap_or(NonZeroUsize::new(1000).unwrap());
        let post_size = NonZeroUsize::new(post_cache_size).unwrap_or(NonZeroUsize::new(1000).unwrap());
        
        Self {
            user_cache: Arc::new(Mutex::new(LruCache::new(user_size))),
            post_cache: Arc::new(Mutex::new(LruCache::new(post_size))),
            metrics: Arc::new(CacheMetrics::default()),
        }
    }

    pub async fn get_user_profile(&self, did: &str) -> Option<Arc<BlueskyProfile>> {
        let mut cache = self.user_cache.lock().await;
        
        if let Some(profile) = cache.get(did) {
            self.metrics.user_hits.fetch_add(1, Ordering::Relaxed);
            self.metrics.total_requests.fetch_add(1, Ordering::Relaxed);
            trace!("Cache hit for user profile: {}", did);
            return Some(Arc::clone(profile));
        }

        self.metrics.user_misses.fetch_add(1, Ordering::Relaxed);
        self.metrics.total_requests.fetch_add(1, Ordering::Relaxed);
        trace!("Cache miss for user profile: {}", did);
        None
    }

    pub async fn get_user_profiles(&self, dids: &[String]) -> Vec<Option<Arc<BlueskyProfile>>> {
        let mut cache = self.user_cache.lock().await;
        let mut results = Vec::with_capacity(dids.len());
        
        for did in dids {
            if let Some(profile) = cache.get(did) {
                self.metrics.user_hits.fetch_add(1, Ordering::Relaxed);
                self.metrics.total_requests.fetch_add(1, Ordering::Relaxed);
                results.push(Some(Arc::clone(profile)));
            } else {
                self.metrics.user_misses.fetch_add(1, Ordering::Relaxed);
                self.metrics.total_requests.fetch_add(1, Ordering::Relaxed);
                results.push(None);
            }
        }
        results
    }

    pub async fn set_user_profile(&self, did: String, profile: Arc<BlueskyProfile>) {
        let mut cache = self.user_cache.lock().await;
        
        if cache.len() >= cache.cap().into() {
            self.metrics.cache_evictions.fetch_add(1, Ordering::Relaxed);
        }
        
        cache.put(did.clone(), profile);
        trace!("Cached user profile: {}", did);
    }

    pub async fn get_post(&self, uri: &str) -> Option<Arc<BlueskyPost>> {
        let mut cache = self.post_cache.lock().await;
        
        if let Some(post) = cache.get(uri) {
            self.metrics.post_hits.fetch_add(1, Ordering::Relaxed);
            self.metrics.total_requests.fetch_add(1, Ordering::Relaxed);
            trace!("Cache hit for post: {}", uri);
            return Some(Arc::clone(post));
        }

        self.metrics.post_misses.fetch_add(1, Ordering::Relaxed);
        self.metrics.total_requests.fetch_add(1, Ordering::Relaxed);
        trace!("Cache miss for post: {}", uri);
        None
    }

    pub async fn get_posts(&self, uris: &[String]) -> Vec<Option<Arc<BlueskyPost>>> {
        let mut cache = self.post_cache.lock().await;
        let mut results = Vec::with_capacity(uris.len());
        
        for uri in uris {
            if let Some(post) = cache.get(uri) {
                self.metrics.post_hits.fetch_add(1, Ordering::Relaxed);
                self.metrics.total_requests.fetch_add(1, Ordering::Relaxed);
                results.push(Some(Arc::clone(post)));
            } else {
                self.metrics.post_misses.fetch_add(1, Ordering::Relaxed);
                self.metrics.total_requests.fetch_add(1, Ordering::Relaxed);
                results.push(None);
            }
        }
        results
    }

    pub async fn set_post(&self, uri: String, post: Arc<BlueskyPost>) {
        let mut cache = self.post_cache.lock().await;
        
        if cache.len() >= cache.cap().into() {
            self.metrics.cache_evictions.fetch_add(1, Ordering::Relaxed);
        }
        
        cache.put(uri.clone(), post);
        trace!("Cached post: {}", uri);
    }

    pub async fn check_user_profiles_cached(&self, dids: &[String]) -> Vec<bool> {
        let cache = self.user_cache.lock().await;
        dids.iter()
            .map(|did| cache.contains(did))
            .collect()
    }

    pub async fn check_posts_cached(&self, uris: &[String]) -> Vec<bool> {
        let cache = self.post_cache.lock().await;
        uris.iter()
            .map(|uri| cache.contains(uri))
            .collect()
    }

    pub async fn get_metrics(&self) -> CacheMetricsSnapshot {
        CacheMetricsSnapshot {
            user_hits: self.metrics.user_hits.load(Ordering::Relaxed),
            user_misses: self.metrics.user_misses.load(Ordering::Relaxed),
            post_hits: self.metrics.post_hits.load(Ordering::Relaxed),
            post_misses: self.metrics.post_misses.load(Ordering::Relaxed),
            total_requests: self.metrics.total_requests.load(Ordering::Relaxed),
            cache_evictions: self.metrics.cache_evictions.load(Ordering::Relaxed),
        }
    }

    pub async fn clear(&self) {
        let mut user_cache = self.user_cache.lock().await;
        let mut post_cache = self.post_cache.lock().await;
        user_cache.clear();
        post_cache.clear();
        trace!("Cleared all caches");
    }

    pub async fn cleanup_concurrent(&self, _max_age: std::time::Duration) {}

    pub async fn get_hit_rates(&self) -> (f64, f64) {
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

        let result = cache.get_user_profile("did:plc:test").await;
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

        cache
            .set_user_profile("did:plc:test".to_string(), Arc::new(profile.clone()))
            .await;

        let result = cache.get_user_profile("did:plc:test").await;
        assert!(result.is_some());
        assert_eq!(result.unwrap().did.as_ref(), "did:plc:test");

        let metrics = cache.get_metrics().await;
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

        cache
            .get_post("at://did:plc:test/app.bsky.feed.post/notfound")
            .await;

        cache
            .set_post(
                "at://did:plc:test/app.bsky.feed.post/test".to_string(),
                Arc::new(post.clone()),
            )
            .await;

        let result = cache
            .get_post("at://did:plc:test/app.bsky.feed.post/test")
            .await;
        assert!(result.is_some());
        assert_eq!(result.unwrap().text, "Hello world");

        let metrics = cache.get_metrics().await;
        assert_eq!(metrics.post_hits, 1);
        assert_eq!(metrics.post_misses, 1);
    }

    #[tokio::test]
    async fn test_hit_rates() {
        let cache = TurboCache::new(10, 10);

        cache.get_user_profile("did:plc:test1").await;
        cache.get_user_profile("did:plc:test2").await;

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

        cache
            .set_user_profile("did:plc:test1".to_string(), Arc::new(profile))
            .await;
        cache.get_user_profile("did:plc:test1").await;

        let (user_hit_rate, post_hit_rate) = cache.get_hit_rates().await;
        assert_eq!(user_hit_rate, 1.0 / 3.0);
        assert_eq!(post_hit_rate, 0.0);
    }
}
