use crate::models::bluesky::{BlueskyPost, BlueskyProfile};
use dashmap::DashMap;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use tokio::sync::RwLock;
use tracing::{debug, trace};

#[derive(Clone)]
pub struct TurboCache {
    user_cache: Arc<DashMap<String, BlueskyProfile>>,
    post_cache: Arc<DashMap<String, BlueskyPost>>,
    user_cache_size: usize,
    post_cache_size: usize,
    user_count: Arc<AtomicUsize>,
    post_count: Arc<AtomicUsize>,
    metrics: Arc<RwLock<CacheMetrics>>,
}

#[derive(Debug, Default, Clone)]
pub struct CacheMetrics {
    pub user_hits: u64,
    pub user_misses: u64,
    pub post_hits: u64,
    pub post_misses: u64,
    pub total_requests: u64,
    pub cache_evictions: u64,
}

impl TurboCache {
    pub fn new(user_cache_size: usize, post_cache_size: usize) -> Self {
        Self {
            user_cache: Arc::new(DashMap::new()),
            post_cache: Arc::new(DashMap::new()),
            user_cache_size,
            post_cache_size,
            user_count: Arc::new(AtomicUsize::new(0)),
            post_count: Arc::new(AtomicUsize::new(0)),
            metrics: Arc::new(RwLock::new(CacheMetrics::default())),
        }
    }

    pub async fn get_user_profile(&self, did: &str) -> Option<BlueskyProfile> {
        if let Some(profile) = self.user_cache.get(did) {
            self.update_metrics(|m| m.user_hits += 1).await;
            trace!("Cache hit for user profile: {}", did);
            return Some(profile.clone());
        }

        self.update_metrics(|m| m.user_misses += 1).await;
        trace!("Cache miss for user profile: {}", did);
        None
    }

    pub async fn get_user_profiles(&self, dids: &[String]) -> Vec<Option<BlueskyProfile>> {
        let mut results = Vec::with_capacity(dids.len());
        for did in dids {
            results.push(self.get_user_profile(did).await);
        }
        results
    }

    pub async fn set_user_profile(&self, did: String, profile: BlueskyProfile) {
        let current_count = self.user_count.load(Ordering::Relaxed);

        if current_count >= self.user_cache_size {
            self.evict_oldest_users(100);
        }

        if self.user_cache.insert(did.clone(), profile).is_none() {
            self.user_count.fetch_add(1, Ordering::Relaxed);
        }

        debug!("Cached user profile: {}", did);
    }

    fn evict_oldest_users(&self, count: usize) {
        let evict_count = count.min(self.user_cache.len());
        if evict_count == 0 {
            return;
        }

        let keys: Vec<String> = self
            .user_cache
            .iter()
            .take(evict_count)
            .map(|entry| entry.key().clone())
            .collect();

        for key in keys {
            if self.user_cache.remove(&key).is_some() {
                self.user_count.fetch_sub(1, Ordering::Relaxed);
            }
        }
    }

    pub async fn get_post(&self, uri: &str) -> Option<BlueskyPost> {
        if let Some(post) = self.post_cache.get(uri) {
            self.update_metrics(|m| m.post_hits += 1).await;
            trace!("Cache hit for post: {}", uri);
            return Some(post.clone());
        }

        self.update_metrics(|m| m.post_misses += 1).await;
        trace!("Cache miss for post: {}", uri);
        None
    }

    pub async fn get_posts(&self, uris: &[String]) -> Vec<Option<BlueskyPost>> {
        let mut results = Vec::with_capacity(uris.len());
        for uri in uris {
            results.push(self.get_post(uri).await);
        }
        results
    }

    pub async fn set_post(&self, uri: String, post: BlueskyPost) {
        let current_count = self.post_count.load(Ordering::Relaxed);

        if current_count >= self.post_cache_size {
            self.evict_oldest_posts(100);
        }

        if self.post_cache.insert(uri.clone(), post).is_none() {
            self.post_count.fetch_add(1, Ordering::Relaxed);
        }

        debug!("Cached post: {}", uri);
    }

    fn evict_oldest_posts(&self, count: usize) {
        let evict_count = count.min(self.post_cache.len());
        if evict_count == 0 {
            return;
        }

        let keys: Vec<String> = self
            .post_cache
            .iter()
            .take(evict_count)
            .map(|entry| entry.key().clone())
            .collect();

        for key in keys {
            if self.post_cache.remove(&key).is_some() {
                self.post_count.fetch_sub(1, Ordering::Relaxed);
            }
        }
    }

    pub async fn check_user_profiles_cached(&self, dids: &[String]) -> Vec<bool> {
        dids.iter()
            .map(|did| self.user_cache.contains_key(did))
            .collect()
    }

    pub async fn check_posts_cached(&self, uris: &[String]) -> Vec<bool> {
        uris.iter()
            .map(|uri| self.post_cache.contains_key(uri))
            .collect()
    }

    pub async fn get_metrics(&self) -> CacheMetrics {
        self.metrics.read().await.clone()
    }

    pub async fn clear(&self) {
        self.user_cache.clear();
        self.post_cache.clear();
        self.user_count.store(0, Ordering::Relaxed);
        self.post_count.store(0, Ordering::Relaxed);
        debug!("Cleared all caches");
    }

    pub async fn cleanup_concurrent(&self, _max_age: std::time::Duration) {
    }

    pub async fn get_hit_rates(&self) -> (f64, f64) {
        let metrics = self.metrics.read().await;

        let user_hit_rate = if metrics.user_hits + metrics.user_misses > 0 {
            metrics.user_hits as f64 / (metrics.user_hits + metrics.user_misses) as f64
        } else {
            0.0
        };

        let post_hit_rate = if metrics.post_hits + metrics.post_misses > 0 {
            metrics.post_hits as f64 / (metrics.post_hits + metrics.post_misses) as f64
        } else {
            0.0
        };

        (user_hit_rate, post_hit_rate)
    }

    async fn update_metrics<F>(&self, updater: F)
    where
        F: FnOnce(&mut CacheMetrics),
    {
        let mut metrics = self.metrics.write().await;
        updater(&mut metrics);
        metrics.total_requests += 1;
    }
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
            did: "did:plc:test".to_string(),
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
            .set_user_profile("did:plc:test".to_string(), profile.clone())
            .await;

        let result = cache.get_user_profile("did:plc:test").await;
        assert!(result.is_some());
        assert_eq!(result.unwrap().did, "did:plc:test");

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
                did: "did:plc:test".to_string(),
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
            .set_post(
                "at://did:plc:test/app.bsky.feed.post/test".to_string(),
                post.clone(),
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
            did: "did:plc:test1".to_string(),
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
            .set_user_profile("did:plc:test1".to_string(), profile)
            .await;
        cache.get_user_profile("did:plc:test1").await;

        let (user_hit_rate, post_hit_rate) = cache.get_hit_rates().await;
        assert_eq!(user_hit_rate, 0.5);
        assert_eq!(post_hit_rate, 0.0);
    }
}
