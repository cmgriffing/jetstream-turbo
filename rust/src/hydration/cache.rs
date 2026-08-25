use crate::client::HydrationFailure;
use crate::models::bluesky::{BlueskyPost, BlueskyProfile};
use ahash::RandomState;
use lru::LruCache;
use std::collections::{HashMap, VecDeque};
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
    weight_bytes: u64,
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
    user_order: Arc<Mutex<VecDeque<String>>>,
    post_cache: Arc<RwLock<HashMap<String, CacheEntry<BlueskyPost>, RandomState>>>,
    post_order: Arc<Mutex<VecDeque<String>>>,
    negative_post_cache: Arc<Mutex<LruCache<String, NegativePostEntry>>>,
    expired_negative_posts: Arc<Mutex<LruCache<String, u64>>>,
    negative_post_ttl: Duration,
    negative_post_capacity: usize,
    clock: Arc<dyn Fn() -> Instant + Send + Sync>,
    user_capacity: usize,
    post_capacity: usize,
    user_limit_bytes: u64,
    post_limit_bytes: u64,
    negative_post_limit_bytes: u64,
    user_weight_bytes: Arc<AtomicU64>,
    post_weight_bytes: Arc<AtomicU64>,
    negative_post_weight_bytes: Arc<AtomicU64>,
    expired_negative_post_weight_bytes: Arc<AtomicU64>,
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
    pub user_evictions: AtomicU64,
    pub post_evictions: AtomicU64,
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
    weight_bytes: u64,
}

#[derive(Debug, Clone, Copy)]
struct CacheByteLimits {
    user: u64,
    post: u64,
    negative_post: u64,
}

struct PositiveCacheInsert<'a, T> {
    map: &'a Arc<RwLock<HashMap<String, CacheEntry<T>, RandomState>>>,
    order: &'a Arc<Mutex<VecDeque<String>>>,
    capacity: usize,
    current_weight_bytes: &'a AtomicU64,
    limit_bytes: u64,
    kind_evictions: &'a AtomicU64,
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
            user_evictions: AtomicU64::new(self.user_evictions.load(Ordering::Relaxed)),
            post_evictions: AtomicU64::new(self.post_evictions.load(Ordering::Relaxed)),
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
        Self::new_with_memory_limits(
            user_cache_size,
            post_cache_size,
            negative_post_capacity,
            negative_post_ttl,
            conservative_limit(user_cache_size, 8 * 1024),
            conservative_limit(post_cache_size, 64 * 1024),
            conservative_limit(negative_post_capacity, 4 * 1024),
        )
    }

    pub fn new_with_memory_limits(
        user_cache_size: usize,
        post_cache_size: usize,
        negative_post_capacity: usize,
        negative_post_ttl: Duration,
        user_limit_bytes: u64,
        post_limit_bytes: u64,
        negative_post_limit_bytes: u64,
    ) -> Self {
        Self::new_with_clock_and_memory_limits(
            user_cache_size,
            post_cache_size,
            negative_post_capacity,
            negative_post_ttl,
            Arc::new(Instant::now),
            CacheByteLimits {
                user: user_limit_bytes,
                post: post_limit_bytes,
                negative_post: negative_post_limit_bytes,
            },
        )
    }

    #[cfg(test)]
    pub(crate) fn new_with_clock(
        user_cache_size: usize,
        post_cache_size: usize,
        negative_post_capacity: usize,
        negative_post_ttl: Duration,
        clock: Arc<dyn Fn() -> Instant + Send + Sync>,
    ) -> Self {
        Self::new_with_clock_and_memory_limits(
            user_cache_size,
            post_cache_size,
            negative_post_capacity,
            negative_post_ttl,
            clock,
            CacheByteLimits {
                user: conservative_limit(user_cache_size, 8 * 1024),
                post: conservative_limit(post_cache_size, 64 * 1024),
                negative_post: conservative_limit(negative_post_capacity, 4 * 1024),
            },
        )
    }

    fn new_with_clock_and_memory_limits(
        user_cache_size: usize,
        post_cache_size: usize,
        negative_post_capacity: usize,
        negative_post_ttl: Duration,
        clock: Arc<dyn Fn() -> Instant + Send + Sync>,
        limits: CacheByteLimits,
    ) -> Self {
        assert!(user_cache_size > 0, "user cache capacity must be positive");
        assert!(post_cache_size > 0, "post cache capacity must be positive");
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
            user_order: Arc::new(Mutex::new(VecDeque::with_capacity(
                user_cache_size.min(1024),
            ))),
            post_cache,
            post_order: Arc::new(Mutex::new(VecDeque::with_capacity(
                post_cache_size.min(1024),
            ))),
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
            user_limit_bytes: limits.user.max(1),
            post_limit_bytes: limits.post.max(1),
            negative_post_limit_bytes: limits.negative_post.max(1),
            user_weight_bytes: Arc::new(AtomicU64::new(0)),
            post_weight_bytes: Arc::new(AtomicU64::new(0)),
            negative_post_weight_bytes: Arc::new(AtomicU64::new(0)),
            expired_negative_post_weight_bytes: Arc::new(AtomicU64::new(0)),
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
        let weight_bytes = estimated_serialized_weight(&did, profile.as_ref());
        self.insert_entry(
            PositiveCacheInsert {
                map: &self.user_cache,
                order: &self.user_order,
                capacity: self.user_capacity,
                current_weight_bytes: &self.user_weight_bytes,
                limit_bytes: self.user_limit_bytes,
                kind_evictions: &self.metrics.user_evictions,
            },
            did,
            profile,
            weight_bytes,
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

    /// Insert into a map-backed cache with a bounded FIFO key ring. Expiration
    /// remains lazy and pressure reclamation prunes both structures.
    fn insert_entry<T: Send + Sync + 'static>(
        &self,
        target: PositiveCacheInsert<'_, T>,
        key: String,
        value: Arc<T>,
        weight_bytes: u64,
    ) {
        if weight_bytes > target.limit_bytes {
            self.metrics.cache_evictions.fetch_add(1, Ordering::Relaxed);
            target.kind_evictions.fetch_add(1, Ordering::Relaxed);
            return;
        }
        let mut map = target
            .map
            .write()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let mut order = target
            .order
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let now = (self.clock)();
        if let Some(previous) = map.remove(&key) {
            subtract_weight(target.current_weight_bytes, previous.weight_bytes);
            order.retain(|existing| existing != &key);
        }
        while map.len() >= target.capacity
            || target
                .current_weight_bytes
                .load(Ordering::Relaxed)
                .saturating_add(weight_bytes)
                > target.limit_bytes
        {
            let Some(oldest) = order.pop_front() else {
                break;
            };
            if let Some(evicted) = map.remove(&oldest) {
                subtract_weight(target.current_weight_bytes, evicted.weight_bytes);
                self.metrics.cache_evictions.fetch_add(1, Ordering::Relaxed);
                target.kind_evictions.fetch_add(1, Ordering::Relaxed);
            }
        }
        target
            .current_weight_bytes
            .fetch_add(weight_bytes, Ordering::Relaxed);
        order.push_back(key.clone());
        map.insert(
            key,
            CacheEntry {
                value,
                inserted_at: now,
                weight_bytes,
            },
        );
    }

    pub fn set_post(&self, uri: String, post: Arc<BlueskyPost>) {
        self.complete_post_resolution(&uri, "found");
        let weight_bytes = estimated_serialized_weight(&uri, post.as_ref());
        self.insert_entry(
            PositiveCacheInsert {
                map: &self.post_cache,
                order: &self.post_order,
                capacity: self.post_capacity,
                current_weight_bytes: &self.post_weight_bytes,
                limit_bytes: self.post_limit_bytes,
                kind_evictions: &self.metrics.post_evictions,
            },
            uri,
            post,
            weight_bytes,
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
            if let Some(expired) = cache.pop(uri) {
                subtract_weight(&self.negative_post_weight_bytes, expired.weight_bytes);
            }
            let expired_weight = estimated_key_weight(uri);
            let mut expired_posts = self
                .expired_negative_posts
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            if let Some((_, replaced_weight)) = expired_posts.push(uri.to_string(), expired_weight)
            {
                subtract_weight(&self.expired_negative_post_weight_bytes, replaced_weight);
            }
            self.expired_negative_post_weight_bytes
                .fetch_add(expired_weight, Ordering::Relaxed);
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
        if let Some(removed) = self
            .post_cache
            .write()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .remove(&uri)
        {
            subtract_weight(&self.post_weight_bytes, removed.weight_bytes);
        }
        let weight_bytes = estimated_serialized_weight(&uri, &failure);
        if weight_bytes > self.negative_post_limit_bytes {
            self.metrics
                .negative_post_evictions
                .fetch_add(1, Ordering::Relaxed);
            return;
        }
        let expires_at = (self.clock)() + self.negative_post_ttl;
        let mut cache = self
            .negative_post_cache
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let mut expired_posts = self
            .expired_negative_posts
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        if let Some(previous) = cache.pop(&uri) {
            subtract_weight(&self.negative_post_weight_bytes, previous.weight_bytes);
        }
        if let Some(expired_weight) = expired_posts.pop(&uri) {
            subtract_weight(&self.expired_negative_post_weight_bytes, expired_weight);
        }
        while cache.len() >= self.negative_post_capacity
            || self
                .negative_post_weight_bytes
                .load(Ordering::Relaxed)
                .saturating_add(
                    self.expired_negative_post_weight_bytes
                        .load(Ordering::Relaxed),
                )
                .saturating_add(weight_bytes)
                > self.negative_post_limit_bytes
        {
            if let Some((_, expired_weight)) = expired_posts.pop_lru() {
                subtract_weight(&self.expired_negative_post_weight_bytes, expired_weight);
            } else if let Some((_, evicted)) = cache.pop_lru() {
                subtract_weight(&self.negative_post_weight_bytes, evicted.weight_bytes);
            } else {
                break;
            }
            self.metrics
                .negative_post_evictions
                .fetch_add(1, Ordering::Relaxed);
            metrics::counter!("optional_hydration_negative_cache_evictions_total").increment(1);
        }
        self.negative_post_weight_bytes
            .fetch_add(weight_bytes, Ordering::Relaxed);
        cache.put(
            uri,
            NegativePostEntry {
                failure,
                expires_at,
                weight_bytes,
            },
        );
    }

    pub fn clear_unavailable_post(&self, uri: &str) -> bool {
        let removed = self
            .negative_post_cache
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .pop(uri);
        if let Some(entry) = removed {
            subtract_weight(&self.negative_post_weight_bytes, entry.weight_bytes);
            true
        } else {
            false
        }
    }

    pub fn complete_post_resolution(&self, uri: &str, outcome: &'static str) {
        let was_active = self.clear_unavailable_post(uri);
        let was_expired = self
            .expired_negative_posts
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .pop(uri);
        if let Some(expired_weight) = was_expired {
            subtract_weight(&self.expired_negative_post_weight_bytes, expired_weight);
        }
        if was_active || was_expired.is_some() {
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
        uris.iter().map(|uri| map.contains_key(uri)).collect()
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
            user_evictions: self.metrics.user_evictions.load(Ordering::Relaxed),
            post_evictions: self.metrics.post_evictions.load(Ordering::Relaxed),
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
        let mut users = self
            .user_cache
            .write()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        users.clear();
        users.shrink_to_fit();
        drop(users);
        let mut posts = self
            .post_cache
            .write()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        posts.clear();
        posts.shrink_to_fit();
        drop(posts);
        let mut user_order = self
            .user_order
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        user_order.clear();
        user_order.shrink_to_fit();
        drop(user_order);
        let mut post_order = self
            .post_order
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        post_order.clear();
        post_order.shrink_to_fit();
        drop(post_order);
        self.negative_post_cache
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .clear();
        self.expired_negative_posts
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .clear();
        self.user_weight_bytes.store(0, Ordering::Relaxed);
        self.post_weight_bytes.store(0, Ordering::Relaxed);
        self.negative_post_weight_bytes.store(0, Ordering::Relaxed);
        self.expired_negative_post_weight_bytes
            .store(0, Ordering::Relaxed);
        trace!("Cleared all caches");
    }

    pub fn memory_snapshot(&self) -> CacheMemorySnapshot {
        let user_entries = self
            .user_cache
            .read()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .len();
        let post_entries = self
            .post_cache
            .read()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .len();
        let negative_post_entries = self
            .negative_post_cache
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .len()
            .saturating_add(
                self.expired_negative_posts
                    .lock()
                    .unwrap_or_else(|poisoned| poisoned.into_inner())
                    .len(),
            );
        CacheMemorySnapshot {
            user_entries,
            user_entry_limit: self.user_capacity,
            user_bytes: self.user_weight_bytes.load(Ordering::Relaxed),
            user_limit_bytes: self.user_limit_bytes,
            post_entries,
            post_entry_limit: self.post_capacity,
            post_bytes: self.post_weight_bytes.load(Ordering::Relaxed),
            post_limit_bytes: self.post_limit_bytes,
            negative_post_entries,
            negative_post_entry_limit: self.negative_post_capacity.saturating_mul(2),
            negative_post_bytes: self
                .negative_post_weight_bytes
                .load(Ordering::Relaxed)
                .saturating_add(
                    self.expired_negative_post_weight_bytes
                        .load(Ordering::Relaxed),
                ),
            negative_post_limit_bytes: self.negative_post_limit_bytes,
            user_evictions: self.metrics.user_evictions.load(Ordering::Relaxed),
            post_evictions: self.metrics.post_evictions.load(Ordering::Relaxed),
            negative_post_evictions: self.metrics.negative_post_evictions.load(Ordering::Relaxed),
        }
    }

    pub fn reclaim_expired(&self) -> usize {
        let now = (self.clock)();
        let users = reclaim_expired_entries(
            &self.user_cache,
            &self.user_order,
            &self.user_weight_bytes,
            now,
        );
        let posts = reclaim_expired_entries(
            &self.post_cache,
            &self.post_order,
            &self.post_weight_bytes,
            now,
        );
        let mut negative = self
            .negative_post_cache
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let expired = negative
            .iter()
            .filter(|(_, entry)| entry.expires_at <= now)
            .map(|(key, _)| key.clone())
            .collect::<Vec<_>>();
        for key in &expired {
            if let Some(entry) = negative.pop(key) {
                subtract_weight(&self.negative_post_weight_bytes, entry.weight_bytes);
            }
        }
        drop(negative);
        let mut expired_posts = self
            .expired_negative_posts
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let expired_markers = expired_posts.len();
        expired_posts.clear();
        self.expired_negative_post_weight_bytes
            .store(0, Ordering::Relaxed);
        users + posts + expired.len() + expired_markers
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
    pub user_evictions: u64,
    pub post_evictions: u64,
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

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, serde::Serialize)]
pub struct CacheMemorySnapshot {
    pub user_entries: usize,
    pub user_entry_limit: usize,
    pub user_bytes: u64,
    pub user_limit_bytes: u64,
    pub post_entries: usize,
    pub post_entry_limit: usize,
    pub post_bytes: u64,
    pub post_limit_bytes: u64,
    pub negative_post_entries: usize,
    pub negative_post_entry_limit: usize,
    pub negative_post_bytes: u64,
    pub negative_post_limit_bytes: u64,
    pub user_evictions: u64,
    pub post_evictions: u64,
    pub negative_post_evictions: u64,
}

fn conservative_limit(capacity: usize, maximum_item_bytes: u64) -> u64 {
    u64::try_from(capacity)
        .unwrap_or(u64::MAX)
        .saturating_mul(maximum_item_bytes)
        .max(1)
}

fn estimated_serialized_weight<T: serde::Serialize>(key: &str, value: &T) -> u64 {
    const ENTRY_OVERHEAD_BYTES: u64 = 128;
    let serialized = serde_json::to_vec(value)
        .ok()
        .and_then(|value| u64::try_from(value.len()).ok())
        .unwrap_or(u64::MAX);
    u64::try_from(key.len())
        .unwrap_or(u64::MAX)
        // Positive caches retain the key in both the map and FIFO ring. This
        // deliberately overestimates negative LRU entries, which is safe.
        .saturating_mul(2)
        .saturating_add(serialized)
        .saturating_add(ENTRY_OVERHEAD_BYTES)
}

fn estimated_key_weight(key: &str) -> u64 {
    const ENTRY_OVERHEAD_BYTES: u64 = 128;
    u64::try_from(key.len())
        .unwrap_or(u64::MAX)
        .saturating_add(ENTRY_OVERHEAD_BYTES)
}

fn subtract_weight(weight: &AtomicU64, amount: u64) {
    let _ = weight.fetch_update(Ordering::Relaxed, Ordering::Relaxed, |current| {
        Some(current.saturating_sub(amount))
    });
}

fn reclaim_expired_entries<T>(
    cache: &RwLock<HashMap<String, CacheEntry<T>, RandomState>>,
    order: &Mutex<VecDeque<String>>,
    weight: &AtomicU64,
    now: Instant,
) -> usize {
    let mut cache = cache
        .write()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    let expired = cache
        .iter()
        .filter(|(_, entry)| entry.expired(PROFILE_TTL, now))
        .map(|(key, _)| key.clone())
        .collect::<Vec<_>>();
    for key in &expired {
        if let Some(entry) = cache.remove(key) {
            subtract_weight(weight, entry.weight_bytes);
        }
    }
    if !expired.is_empty() {
        cache.shrink_to_fit();
        let mut order = order
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        order.retain(|key| cache.contains_key(key));
        order.shrink_to_fit();
    }
    expired.len()
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

        assert!(
            cache.get_user_profile("did:plc:a").is_none(),
            "oldest evicted"
        );
        assert!(cache.get_user_profile("did:plc:b").is_some());
        assert!(cache.get_user_profile("did:plc:c").is_some());
        assert_eq!(cache.get_metrics().cache_evictions, 1);
    }

    #[test]
    fn cache_rejects_single_value_larger_than_byte_limit() {
        let cache =
            TurboCache::new_with_memory_limits(10, 10, 10, Duration::from_secs(30), 1, 1, 1);
        cache.set_user_profile(
            "did:plc:large".to_string(),
            Arc::new(BlueskyProfile {
                did: "did:plc:large".into(),
                handle: "large.bsky.social".to_string(),
                display_name: Some("large".repeat(100)),
                description: None,
                avatar: None,
                banner: None,
                followers_count: None,
                follows_count: None,
                posts_count: None,
                indexed_at: None,
                created_at: None,
                labels: None,
            }),
        );

        assert_eq!(cache.get_entry_counts().0, 0);
        assert_eq!(cache.memory_snapshot().user_bytes, 0);
        assert_eq!(cache.memory_snapshot().user_evictions, 1);
    }

    #[test]
    fn reclaim_expired_releases_tracked_cache_bytes() {
        let start = Instant::now();
        let now = Arc::new(Mutex::new(start));
        let cache_clock = Arc::clone(&now);
        let cache = TurboCache::new_with_clock(
            10,
            10,
            10,
            Duration::from_secs(30),
            Arc::new(move || *cache_clock.lock().expect("cache clock poisoned")),
        );
        cache.set_user_profile(
            "did:plc:expires".to_string(),
            Arc::new(BlueskyProfile {
                did: "did:plc:expires".into(),
                handle: "expires.bsky.social".to_string(),
                display_name: None,
                description: None,
                avatar: None,
                banner: None,
                followers_count: None,
                follows_count: None,
                posts_count: None,
                indexed_at: None,
                created_at: None,
                labels: None,
            }),
        );
        assert!(cache.memory_snapshot().user_bytes > 0);
        *now.lock().expect("cache clock poisoned") = start + PROFILE_TTL;

        assert_eq!(cache.reclaim_expired(), 1);
        assert_eq!(cache.memory_snapshot().user_bytes, 0);
    }
}
