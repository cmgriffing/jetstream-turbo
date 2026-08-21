use crate::client::HydrationFailure;
use crate::models::bluesky::{BlueskyPost, BlueskyProfile};
use ahash::RandomState;
use lru::LruCache;
use std::collections::HashMap;
use std::num::NonZeroUsize;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, RwLock};
use std::time::{Duration, Instant};
use tracing::{instrument, trace};

/// Time-to-live for cached profiles and posts (matches the previous moka TTL).
const PROFILE_TTL: Duration = Duration::from_secs(300);

/// A cached value plus its insertion time, used to implement lazy TTL expiry
/// (and FIFO eviction when the cache is at capacity).
struct CacheEntry<T> {
    value: Arc<T>,
    inserted_at: Instant,
}

impl<T> CacheEntry<T> {
    #[inline(always)]
    fn expired(&self, ttl: Duration, now: Instant) -> bool {
        now.duration_since(self.inserted_at) >= ttl
    }
}

#[derive(Clone)]
pub struct TurboCache {
    user_cache: Arc<RwLock<HashMap<String, CacheEntry<BlueskyProfile>, RandomState>>>,
    post_cache: Arc<RwLock<HashMap<String, CacheEntry<BlueskyPost>, RandomState>>>,
    negative_post_cache: Arc<Mutex<LruCache<String, NegativePostEntry>>>,
    expired_negative_posts: Arc<Mutex<LruCache<String, ()>>>,
    negative_post_ttl: Duration,
    negative_post_capacity: usize,
    clock: Arc<dyn Fn() -> Instant + Send + Sync>,
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
    pub negative_post_hits: AtomicU64,
    pub negative_post_evictions: AtomicU64,
    pub post_recoveries: AtomicU64,
    pub post_found: AtomicU64,
    pub post_missing: AtomicU64,
    pub post_unavailable: AtomicU64,
    pub partial_records: AtomicU64,
    pub isolation_broad_outage: AtomicU64,
    pub isolation_singleton_poison: AtomicU64,
    pub isolation_budget_exhausted: AtomicU64,
}

#[derive(Debug, Clone)]
struct NegativePostEntry {
    failure: HydrationFailure,
    expires_at: Instant,
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
            negative_post_hits: AtomicU64::new(self.negative_post_hits.load(Ordering::Relaxed)),
            negative_post_evictions: AtomicU64::new(
                self.negative_post_evictions.load(Ordering::Relaxed),
            ),
            post_recoveries: AtomicU64::new(self.post_recoveries.load(Ordering::Relaxed)),
            post_found: AtomicU64::new(self.post_found.load(Ordering::Relaxed)),
            post_missing: AtomicU64::new(self.post_missing.load(Ordering::Relaxed)),
            post_unavailable: AtomicU64::new(self.post_unavailable.load(Ordering::Relaxed)),
            partial_records: AtomicU64::new(self.partial_records.load(Ordering::Relaxed)),
            isolation_broad_outage: AtomicU64::new(
                self.isolation_broad_outage.load(Ordering::Relaxed),
            ),
            isolation_singleton_poison: AtomicU64::new(
                self.isolation_singleton_poison.load(Ordering::Relaxed),
            ),
            isolation_budget_exhausted: AtomicU64::new(
                self.isolation_budget_exhausted.load(Ordering::Relaxed),
            ),
        }
    }
}

impl TurboCache {
    pub fn new(user_cache_size: usize, post_cache_size: usize) -> Self {
        Self::new_with_negative_cache(
            user_cache_size,
            post_cache_size,
            20_000,
            Duration::from_secs(5 * 60),
        )
    }

    pub fn new_with_negative_cache(
        user_cache_size: usize,
        post_cache_size: usize,
        negative_post_capacity: usize,
        negative_post_ttl: Duration,
    ) -> Self {
        Self::new_with_clock(
            user_cache_size,
            post_cache_size,
            negative_post_capacity,
            negative_post_ttl,
            Arc::new(Instant::now),
        )
    }

    pub(crate) fn new_with_clock(
        user_cache_size: usize,
        post_cache_size: usize,
        negative_post_capacity: usize,
        negative_post_ttl: Duration,
        clock: Arc<dyn Fn() -> Instant + Send + Sync>,
    ) -> Self {
        assert!(
            negative_post_capacity > 0,
            "negative post cache capacity must be positive"
        );
        let metrics = Arc::new(CacheMetrics::default());

        let user_cache = Arc::new(RwLock::new(HashMap::with_capacity_and_hasher(
            user_cache_size.min(1024),
            RandomState::default(),
        )));
        let post_cache = Arc::new(RwLock::new(HashMap::with_capacity_and_hasher(
            post_cache_size.min(1024),
            RandomState::default(),
        )));

        Self {
            user_cache,
            post_cache,
            negative_post_cache: Arc::new(Mutex::new(LruCache::new(
                NonZeroUsize::new(negative_post_capacity).expect("capacity checked"),
            ))),
            expired_negative_posts: Arc::new(Mutex::new(LruCache::new(
                NonZeroUsize::new(negative_post_capacity).expect("capacity checked"),
            ))),
            negative_post_ttl,
            negative_post_capacity,
            clock,
            user_capacity: user_cache_size,
            post_capacity: post_cache_size,
            metrics,
        }
    }

    pub fn get_entry_counts(&self) -> (u64, u64) {
        let users = self
            .user_cache
            .read()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .len();
        let posts = self
            .post_cache
            .read()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .len();
        (users as u64, posts as u64)
    }

    pub fn get_negative_post_entry_count(&self) -> usize {
        self.negative_post_cache
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .len()
    }

    pub fn get_negative_post_capacity(&self) -> usize {
        self.negative_post_capacity
    }

    pub fn get_capacity_limits(&self) -> (usize, usize) {
        (self.user_capacity, self.post_capacity)
    }

    pub fn get_user_profile(&self, did: &str) -> Option<Arc<BlueskyProfile>> {
        let now = (self.clock)();
        let map = self
            .user_cache
            .read()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        match map.get(did) {
            Some(entry) if !entry.expired(PROFILE_TTL, now) => {
                self.metrics.user_hits.fetch_add(1, Ordering::Relaxed);
                Some(Arc::clone(&entry.value))
            }
            _ => {
                self.metrics.user_misses.fetch_add(1, Ordering::Relaxed);
                None
            }
        }
    }

    pub fn get_user_profiles<S: AsRef<str>>(&self, dids: &[S]) -> Vec<Option<Arc<BlueskyProfile>>> {
        let now = (self.clock)();
        let map = self
            .user_cache
            .read()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let mut profiles = Vec::with_capacity(dids.len());
        let mut hits = 0_u64;

        for did in dids {
            match map.get(did.as_ref()) {
                Some(entry) if !entry.expired(PROFILE_TTL, now) => {
                    hits += 1;
                    profiles.push(Some(Arc::clone(&entry.value)));
                }
                _ => profiles.push(None),
            }
        }

        let misses = dids.len() as u64 - hits;
        if hits > 0 {
            self.metrics.user_hits.fetch_add(hits, Ordering::Relaxed);
        }
        if misses > 0 {
            self.metrics
                .user_misses
                .fetch_add(misses, Ordering::Relaxed);
        }

        profiles
    }

    pub fn set_user_profile(&self, did: String, profile: Arc<BlueskyProfile>) {
        self.insert_entry(
            &self.user_cache,
            self.user_capacity,
            did,
            profile,
            &self.metrics.cache_evictions,
        );
        trace!("Cached user profile");
    }

    pub fn get_post(&self, uri: &str) -> Option<Arc<BlueskyPost>> {
        let now = (self.clock)();
        let map = self
            .post_cache
            .read()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        match map.get(uri) {
            Some(entry) if !entry.expired(PROFILE_TTL, now) => {
                self.metrics.post_hits.fetch_add(1, Ordering::Relaxed);
                Some(Arc::clone(&entry.value))
            }
            _ => {
                self.metrics.post_misses.fetch_add(1, Ordering::Relaxed);
                None
            }
        }
    }

    pub fn get_posts(&self, uris: &[String]) -> Vec<Option<Arc<BlueskyPost>>> {
        let now = (self.clock)();
        let map = self
            .post_cache
            .read()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let mut posts = Vec::with_capacity(uris.len());
        let mut hits = 0_u64;

        for uri in uris {
            match map.get(uri) {
                Some(entry) if !entry.expired(PROFILE_TTL, now) => {
                    hits += 1;
                    posts.push(Some(Arc::clone(&entry.value)));
                }
                _ => posts.push(None),
            }
        }

        let misses = uris.len() as u64 - hits;
        if hits > 0 {
            self.metrics.post_hits.fetch_add(hits, Ordering::Relaxed);
        }
        if misses > 0 {
            self.metrics
                .post_misses
                .fetch_add(misses, Ordering::Relaxed);
        }

        posts
    }

    /// Insert into a sharded-map-backed cache, enforcing the capacity cap with
    /// TTL-aware eviction (drop expired entries, then the oldest-inserted one).
    fn insert_entry<T: Send + Sync + 'static>(
        &self,
        map: &Arc<RwLock<HashMap<String, CacheEntry<T>, RandomState>>>,
        capacity: usize,
        key: String,
        value: Arc<T>,
        evictions: &AtomicU64,
    ) {
        let mut map = map.write().unwrap_or_else(|poisoned| poisoned.into_inner());
        let now = (self.clock)();
        if map.len() >= capacity && !map.contains_key(&key) {
            // Opportunistic purge of TTL-expired entries before evicting.
            let ttl = PROFILE_TTL;
            map.retain(|_, entry| !entry.expired(ttl, now));
            if map.len() >= capacity {
                if let Some(oldest) = map
                    .iter()
                    .min_by_key(|(_, entry)| entry.inserted_at)
                    .map(|(k, _)| k.clone())
                {
                    map.remove(&oldest);
                    evictions.fetch_add(1, Ordering::Relaxed);
                }
            }
        }
        map.insert(
            key,
            CacheEntry {
                value,
                inserted_at: now,
            },
        );
    }

    pub fn set_post(&self, uri: String, post: Arc<BlueskyPost>) {
        self.complete_post_resolution(&uri, "found");
        self.insert_entry(
            &self.post_cache,
            self.post_capacity,
            uri,
            post,
            &self.metrics.cache_evictions,
        );
        trace!("Cached referenced post");
    }

    pub fn get_unavailable_post(&self, uri: &str) -> Option<HydrationFailure> {
        let now = (self.clock)();
        let mut cache = self
            .negative_post_cache
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        if cache.peek(uri).is_some_and(|entry| entry.expires_at <= now) {
            cache.pop(uri);
            self.expired_negative_posts
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner())
                .put(uri.to_string(), ());
            return None;
        }
        let failure = cache.get(uri).map(|entry| entry.failure.clone());
        if failure.is_some() {
            self.metrics
                .negative_post_hits
                .fetch_add(1, Ordering::Relaxed);
            metrics::counter!("optional_hydration_negative_cache_hits_total").increment(1);
        }
        failure
    }

    pub fn set_unavailable_post(&self, uri: String, failure: HydrationFailure) {
        self.post_cache
            .write()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .remove(&uri);
        let expires_at = (self.clock)() + self.negative_post_ttl;
        let mut cache = self
            .negative_post_cache
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let is_new = !cache.contains(&uri);
        if is_new && cache.len() == self.negative_post_capacity {
            cache.pop_lru();
            self.metrics
                .negative_post_evictions
                .fetch_add(1, Ordering::Relaxed);
            metrics::counter!("optional_hydration_negative_cache_evictions_total").increment(1);
        }
        cache.put(
            uri,
            NegativePostEntry {
                failure,
                expires_at,
            },
        );
    }

    pub fn clear_unavailable_post(&self, uri: &str) -> bool {
        self.negative_post_cache
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .pop(uri)
            .is_some()
    }

    pub fn complete_post_resolution(&self, uri: &str, outcome: &'static str) {
        let was_active = self.clear_unavailable_post(uri);
        let was_expired = self
            .expired_negative_posts
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .pop(uri)
            .is_some();
        if was_active || was_expired {
            self.metrics.post_recoveries.fetch_add(1, Ordering::Relaxed);
            metrics::counter!("optional_hydration_recoveries_total", "outcome" => outcome)
                .increment(1);
            tracing::info!(
                request_fingerprint = %crate::client::resilience::stable_identifier_fingerprint(&[uri.to_string()]),
                outcome,
                "Referenced post recovered after temporary unavailability"
            );
        }
    }

    pub fn record_post_outcome(&self, outcome: &crate::client::PostFetchOutcome) {
        match outcome {
            crate::client::PostFetchOutcome::Found(_) => {
                self.metrics.post_found.fetch_add(1, Ordering::Relaxed);
            }
            crate::client::PostFetchOutcome::Missing => {
                self.metrics.post_missing.fetch_add(1, Ordering::Relaxed);
            }
            crate::client::PostFetchOutcome::TemporarilyUnavailable(failure) => {
                self.metrics
                    .post_unavailable
                    .fetch_add(1, Ordering::Relaxed);
                match failure.isolation.as_ref() {
                    Some(crate::client::IsolationOutcome::BroadOutage { .. }) => {
                        self.metrics
                            .isolation_broad_outage
                            .fetch_add(1, Ordering::Relaxed);
                    }
                    Some(crate::client::IsolationOutcome::SingletonPoison { .. }) => {
                        self.metrics
                            .isolation_singleton_poison
                            .fetch_add(1, Ordering::Relaxed);
                    }
                    Some(crate::client::IsolationOutcome::BudgetExhausted) => {
                        self.metrics
                            .isolation_budget_exhausted
                            .fetch_add(1, Ordering::Relaxed);
                    }
                    None => {}
                }
            }
        }
    }

    pub fn record_partial_record(&self) {
        self.metrics.partial_records.fetch_add(1, Ordering::Relaxed);
    }

    #[instrument(name = "cache_check_profiles", skip(self), fields(count))]
    pub fn check_user_profiles_cached(&self, dids: &[String]) -> Vec<bool> {
        tracing::Span::current().record("count", dids.len());
        let map = self
            .user_cache
            .read()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        dids.iter().map(|did| map.contains_key(did)).collect()
    }

    #[instrument(name = "cache_check_posts", skip(self), fields(count))]
    pub fn check_posts_cached(&self, uris: &[String]) -> Vec<bool> {
        tracing::Span::current().record("count", uris.len());
        let map = self
            .post_cache
            .read()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        uris.iter()
            .map(|uri| map.contains_key(uri))
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
            negative_post_hits: self.metrics.negative_post_hits.load(Ordering::Relaxed),
            negative_post_evictions: self.metrics.negative_post_evictions.load(Ordering::Relaxed),
            post_recoveries: self.metrics.post_recoveries.load(Ordering::Relaxed),
            post_found: self.metrics.post_found.load(Ordering::Relaxed),
            post_missing: self.metrics.post_missing.load(Ordering::Relaxed),
            post_unavailable: self.metrics.post_unavailable.load(Ordering::Relaxed),
            partial_records: self.metrics.partial_records.load(Ordering::Relaxed),
            isolation_broad_outage: self.metrics.isolation_broad_outage.load(Ordering::Relaxed),
            isolation_singleton_poison: self
                .metrics
                .isolation_singleton_poison
                .load(Ordering::Relaxed),
            isolation_budget_exhausted: self
                .metrics
                .isolation_budget_exhausted
                .load(Ordering::Relaxed),
        }
    }

    pub fn clear(&self) {
        self.user_cache
            .write()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .clear();
        self.post_cache
            .write()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .clear();
        self.negative_post_cache
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .clear();
        self.expired_negative_posts
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .clear();
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
    pub negative_post_hits: u64,
    pub negative_post_evictions: u64,
    pub post_recoveries: u64,
    pub post_found: u64,
    pub post_missing: u64,
    pub post_unavailable: u64,
    pub partial_records: u64,
    pub isolation_broad_outage: u64,
    pub isolation_singleton_poison: u64,
    pub isolation_budget_exhausted: u64,
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::client::{BlueskyOperation, UpstreamFailureCategory};

    fn failure(fingerprint: &str) -> HydrationFailure {
        HydrationFailure {
            operation: BlueskyOperation::Posts,
            category: UpstreamFailureCategory::ServerError,
            status_class: Some("5xx".to_string()),
            attempts: 4,
            request_fingerprint: fingerprint.to_string(),
            isolation: None,
        }
    }

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

    #[test]
    fn negative_post_cache_suppresses_until_expiry_then_allows_recovery() {
        let start = Instant::now();
        let now = Arc::new(Mutex::new(start));
        let clock_now = Arc::clone(&now);
        let cache = TurboCache::new_with_clock(
            10,
            10,
            2,
            Duration::from_secs(30),
            Arc::new(move || *clock_now.lock().unwrap()),
        );
        let uri = "at://did:plc:test/app.bsky.feed.post/temporary";

        cache.set_unavailable_post(uri.to_string(), failure("safe-fingerprint"));
        assert!(cache.get_unavailable_post(uri).is_some());

        *now.lock().unwrap() = start + Duration::from_secs(29);
        assert!(cache.get_unavailable_post(uri).is_some());

        *now.lock().unwrap() = start + Duration::from_secs(30);
        assert!(cache.get_unavailable_post(uri).is_none());
        cache.complete_post_resolution(uri, "missing");

        let metrics = cache.get_metrics();
        assert_eq!(metrics.negative_post_hits, 2);
        assert_eq!(metrics.post_recoveries, 1);
        assert_eq!(cache.get_negative_post_entry_count(), 0);
    }

    #[test]
    fn negative_post_cache_evicts_at_capacity() {
        let cache = TurboCache::new_with_negative_cache(10, 10, 2, Duration::from_secs(30));
        cache.set_unavailable_post("first".to_string(), failure("one"));
        cache.set_unavailable_post("second".to_string(), failure("two"));
        cache.set_unavailable_post("third".to_string(), failure("three"));

        assert!(cache.get_unavailable_post("first").is_none());
        assert!(cache.get_unavailable_post("second").is_some());
        assert!(cache.get_unavailable_post("third").is_some());
        assert_eq!(cache.get_negative_post_entry_count(), 2);
        assert_eq!(cache.get_metrics().negative_post_evictions, 1);
    }

    #[test]
    fn user_cache_evicts_oldest_when_at_capacity() {
        let cache = TurboCache::new(2, 2);
        let profile = |did: &str| {
            cache.set_user_profile(
                did.to_string(),
                Arc::new(BlueskyProfile {
                    did: did.into(),
                    handle: format!("{did}.bsky.social"),
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
                }),
            );
        };
        profile("did:plc:a");
        profile("did:plc:b");
        profile("did:plc:c");

        assert!(cache.get_user_profile("did:plc:a").is_none(), "oldest evicted");
        assert!(cache.get_user_profile("did:plc:b").is_some());
        assert!(cache.get_user_profile("did:plc:c").is_some());
        assert_eq!(cache.get_metrics().cache_evictions, 1);
    }
}
