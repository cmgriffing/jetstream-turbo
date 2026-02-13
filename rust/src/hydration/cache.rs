use std::sync::Arc;
use std::time::{Duration, Instant};
use dashmap::DashMap;
use lru::LruCache;
use tokio::sync::RwLock;
use tracing::{debug, trace};
use crate::models::bluesky::{BlueskyProfile, BlueskyPost};

/// Thread-safe LRU cache for Turbo data
pub struct TurboCache {
    /// User profiles cache
    users: Arc<RwLock<LruCache<String, BlueskyProfile>>>,
    /// Post cache
    posts: Arc<RwLock<LruCache<String, BlueskyPost>>>,
    /// DashMap for concurrent access when needed
    concurrent_users: Arc<DashMap<String, BlueskyProfile>>,
    concurrent_posts: Arc<DashMap<String, BlueskyPost>>,
    /// Cache metrics
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
            users: Arc::new(RwLock::new(LruCache::new(
                std::num::NonZeroUsize::new(user_cache_size).unwrap()
            ))),
            posts: Arc::new(RwLock::new(LruCache::new(
                std::num::NonZeroUsize::new(post_cache_size).unwrap()
            ))),
            concurrent_users: Arc::new(DashMap::new()),
            concurrent_posts: Arc::new(DashMap::new()),
            metrics: Arc::new(RwLock::new(CacheMetrics::default())),
        }
    }
    
    /// Get user profile from cache, returns None if not found
    pub async fn get_user_profile(&self, did: &str) -> Option<BlueskyProfile> {
        // Try concurrent cache first for faster access
        if let Some(profile) = self.concurrent_users.get(did) {
            self.update_metrics(|m| m.user_hits += 1).await;
            trace!("Cache hit for user profile: {}", did);
            return Some(profile.clone());
        }
        
        // Fall back to LRU cache
        {
            let mut users = self.users.write().await;
            if let Some(profile) = users.get(did) {
                self.update_metrics(|m| m.user_hits += 1).await;
                
                // Also store in concurrent cache for faster access
                self.concurrent_users.insert(did.to_string(), profile.clone());
                
                trace!("Cache hit for user profile: {}", did);
                return Some(profile.clone());
            }
        }
        
        self.update_metrics(|m| m.user_misses += 1).await;
        trace!("Cache miss for user profile: {}", did);
        None
    }
    
    /// Get multiple user profiles from cache
    pub async fn get_user_profiles(&self, dids: &[String]) -> Vec<Option<BlueskyProfile>> {
        let mut results = Vec::with_capacity(dids.len());
        
        for did in dids {
            let profile = self.get_user_profile(did).await;
            results.push(profile);
        }
        
        results
    }
    
    /// Store user profile in cache
    pub async fn set_user_profile(&self, did: String, profile: BlueskyProfile) {
        {
            let mut users = self.users.write().await;
            if let Some(_evicted) = users.put(did.clone(), profile.clone()) {
                self.update_metrics(|m| m.cache_evictions += 1).await;
            }
        }
        
        // Also store in concurrent cache
        self.concurrent_users.insert(did.clone(), profile);
        
        debug!("Cached user profile: {}", did);
    }
    
    /// Get post from cache, returns None if not found
    pub async fn get_post(&self, uri: &str) -> Option<BlueskyPost> {
        // Try concurrent cache first
        if let Some(post) = self.concurrent_posts.get(uri) {
            self.update_metrics(|m| m.post_hits += 1).await;
            trace!("Cache hit for post: {}", uri);
            return Some(post.clone());
        }
        
        // Fall back to LRU cache
        {
            let mut posts = self.posts.write().await;
            if let Some(post) = posts.get(uri) {
                self.update_metrics(|m| m.post_hits += 1).await;
                
                // Also store in concurrent cache
                self.concurrent_posts.insert(uri.to_string(), post.clone());
                
                trace!("Cache hit for post: {}", uri);
                return Some(post.clone());
            }
        }
        
        self.update_metrics(|m| m.post_misses += 1).await;
        trace!("Cache miss for post: {}", uri);
        None
    }
    
    /// Get multiple posts from cache
    pub async fn get_posts(&self, uris: &[String]) -> Vec<Option<BlueskyPost>> {
        let mut results = Vec::with_capacity(uris.len());
        
        for uri in uris {
            let post = self.get_post(uri).await;
            results.push(post);
        }
        
        results
    }
    
    /// Store post in cache
    pub async fn set_post(&self, uri: String, post: BlueskyPost) {
        {
            let mut posts = self.posts.write().await;
            if let Some(_evicted) = posts.put(uri.clone(), post.clone()) {
                self.update_metrics(|m| m.cache_evictions += 1).await;
            }
        }
        
        // Also store in concurrent cache
        self.concurrent_posts.insert(uri.clone(), post);
        
        debug!("Cached post: {}", uri);
    }
    
    /// Check which user profiles are cached
    pub async fn check_user_profiles_cached(&self, dids: &[String]) -> Vec<bool> {
        dids.iter()
            .map(|did| {
                self.concurrent_users.contains_key(did) ||
                self.users.blocking_read().contains(did)
            })
            .collect()
    }
    
    /// Check which posts are cached
    pub async fn check_posts_cached(&self, uris: &[String]) -> Vec<bool> {
        uris.iter()
            .map(|uri| {
                self.concurrent_posts.contains_key(uri) ||
                self.posts.blocking_read().contains(uri)
            })
            .collect()
    }
    
    /// Get cache metrics
    pub async fn get_metrics(&self) -> CacheMetrics {
        self.metrics.read().await.clone()
    }
    
    /// Clear all caches
    pub async fn clear(&self) {
        {
            let mut users = self.users.write().await;
            users.clear();
        }
        {
            let mut posts = self.posts.write().await;
            posts.clear();
        }
        
        self.concurrent_users.clear();
        self.concurrent_posts.clear();
        
        debug!("Cleared all caches");
    }
    
    /// Cleanup old entries from concurrent caches
    pub async fn cleanup_concurrent(&self, _max_age: Duration) {
        let _now = Instant::now();
        
        // Note: DashMap doesn't store creation time, so we implement a simple cleanup
        // by moving items back to LRU cache periodically
        let user_keys: Vec<String> = self.concurrent_users
            .iter()
            .map(|entry| entry.key().clone())
            .collect();
        
        for key in user_keys {
            if let Some((_, profile)) = self.concurrent_users.remove(&key) {
                let mut users = self.users.write().await;
                let _ = users.put(key, profile);
            }
        }
        
        let post_keys: Vec<String> = self.concurrent_posts
            .iter()
            .map(|entry| entry.key().clone())
            .collect();
        
        for key in post_keys {
            if let Some((_, post)) = self.concurrent_posts.remove(&key) {
                let mut posts = self.posts.write().await;
                let _ = posts.put(key, post);
            }
        }
        
        debug!("Cleaned up concurrent caches");
    }
    
    /// Get cache hit rates
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
        
        // Test cache miss
        let result = cache.get_user_profile("did:plc:test").await;
        assert!(result.is_none());
        
        // Test cache set and hit
        let profile = BlueskyProfile {
            did: "did:plc:test".to_string(),
            handle: "test.bsky.social".to_string(),
            display_name: Some("Test User".to_string()),
            description: None,
            avatar: None,
            banner: None,
            followers_count: 0,
            follows_count: 0,
            posts_count: 0,
            indexed_at: None,
            created_at: None,
            labels: None,
        };
        
        cache.set_user_profile("did:plc:test".to_string(), profile.clone()).await;
        
        let result = cache.get_user_profile("did:plc:test").await;
        assert!(result.is_some());
        assert_eq!(result.unwrap().did, "did:plc:test");
        
        // Test metrics
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
                followers_count: 0,
                follows_count: 0,
                posts_count: 0,
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
        
        // Test cache set and hit
        cache.set_post("at://did:plc:test/app.bsky.feed.post/test".to_string(), post.clone()).await;
        
        let result = cache.get_post("at://did:plc:test/app.bsky.feed.post/test").await;
        assert!(result.is_some());
        assert_eq!(result.unwrap().text, "Hello world");
        
        // Test metrics
        let metrics = cache.get_metrics().await;
        assert_eq!(metrics.post_hits, 1);
        assert_eq!(metrics.post_misses, 1);
    }
    
    #[tokio::test]
    async fn test_hit_rates() {
        let cache = TurboCache::new(10, 10);
        
        // Generate some cache activity
        cache.get_user_profile("did:plc:test1").await; // miss
        cache.get_user_profile("did:plc:test2").await; // miss
        
        let profile = BlueskyProfile {
            did: "did:plc:test1".to_string(),
            handle: "test1.bsky.social".to_string(),
            display_name: None,
            description: None,
            avatar: None,
            banner: None,
            followers_count: 0,
            follows_count: 0,
            posts_count: 0,
            indexed_at: None,
            created_at: None,
            labels: None,
        };
        
        cache.set_user_profile("did:plc:test1".to_string(), profile).await;
        cache.get_user_profile("did:plc:test1").await; // hit
        
        let (user_hit_rate, post_hit_rate) = cache.get_hit_rates().await;
        assert_eq!(user_hit_rate, 0.5); // 1 hit, 1 miss = 50%
        assert_eq!(post_hit_rate, 0.0); // 0 hits, 0 misses = 0%
    }
}