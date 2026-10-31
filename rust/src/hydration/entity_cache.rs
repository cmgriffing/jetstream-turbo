//! Optional SQLite entity cache. Recovery writes remain independent of this queue.
use serde::{de::DeserializeOwned, Serialize};
use sqlx::{QueryBuilder, Row, Sqlite, SqlitePool};
use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};
use tokio::sync::{Notify, Semaphore};

const PAYLOAD_VERSION: i64 = 1;
const MAX_ENTRY_BYTES: usize = 1024 * 1024;
const MAX_QUEUE_BYTES: usize = 8 * 1024 * 1024;
// Exactly two rows, independent of the number or payload size of cached entities.
const LOGICAL_BYTES_SQL: &str = "SELECT COALESCE(SUM(logical_bytes),0) FROM hydration_cache_usage";

#[derive(Clone, Copy, Debug, Hash, Eq, PartialEq)]
pub enum CacheKind {
    Profile,
    Post,
}

impl CacheKind {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Profile => "profile",
            Self::Post => "post",
        }
    }
    fn table(self) -> &'static str {
        match self {
            Self::Profile => "profile_cache",
            Self::Post => "post_cache",
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use sqlx::sqlite::SqlitePoolOptions;
    use std::sync::atomic::AtomicI64;

    #[test]
    fn promotion_delay_consumes_original_freshness() {
        let sampled_at = Instant::now();
        let hit = DiskHit {
            value: Arc::new("value"),
            age: Duration::from_secs(299),
            remaining: Duration::from_secs(1),
            sampled_at,
        };
        assert_eq!(
            hit.freshness_at(sampled_at + Duration::from_millis(900)),
            Some((Duration::from_millis(299_900), Duration::from_millis(100)))
        );
        assert!(hit
            .freshness_at(sampled_at + Duration::from_secs(1))
            .is_none());
        assert!(hit
            .freshness_at(sampled_at + Duration::from_secs(2))
            .is_none());
    }

    async fn fixture(config: PersistentCacheConfig) -> (PersistentEntityCache, Arc<AtomicI64>) {
        let pool = SqlitePoolOptions::new()
            .max_connections(1)
            .connect("sqlite::memory:")
            .await
            .unwrap();
        let now = Arc::new(AtomicI64::new(1_000_000));
        let clock = Arc::clone(&now);
        let cache = PersistentEntityCache::new_with_clock(
            pool,
            config,
            Arc::new(move || clock.load(Ordering::SeqCst)),
        )
        .await
        .unwrap();
        (cache, now)
    }

    #[tokio::test]
    async fn original_deadline_ttl_reduction_and_clock_validation() {
        let (cache, now) = fixture(PersistentCacheConfig::default()).await;
        let fetched_at = cache.now_ms();
        cache.store(
            CacheKind::Profile,
            "one".into(),
            &"value",
            fetched_at,
            cache.generation(),
            cache.next_fetch_order(),
        );
        assert!(cache.flush().await);
        now.store(fetched_at + 299_000, Ordering::SeqCst);
        let hit = cache
            .read::<String>(CacheKind::Profile, &["one".into()])
            .await
            .remove("one")
            .unwrap();
        assert_eq!(hit.remaining, Duration::from_secs(1));
        assert_eq!(hit.age, Duration::from_secs(299));
        let shortened = PersistentEntityCache::new_with_clock(
            cache.inner.pool.clone(),
            PersistentCacheConfig {
                profile_ttl: Duration::from_secs(60),
                ..PersistentCacheConfig::default()
            },
            Arc::clone(&cache.inner.clock),
        )
        .await
        .unwrap();
        assert!(shortened
            .read::<String>(CacheKind::Profile, &["one".into()])
            .await
            .is_empty());
        assert!(shortened.flush().await);
        assert_eq!(
            sqlx::query_scalar::<_, i64>("SELECT COUNT(*) FROM profile_cache")
                .fetch_one(&cache.inner.pool)
                .await
                .unwrap(),
            0
        );
        now.store(fetched_at, Ordering::SeqCst);
        cache.store(
            CacheKind::Profile,
            "future".into(),
            &"value",
            fetched_at + 1,
            cache.generation(),
            cache.next_fetch_order(),
        );
        assert!(cache.flush().await);
        assert!(cache
            .read::<String>(CacheKind::Profile, &["future".into()])
            .await
            .is_empty());
    }

    #[tokio::test]
    async fn late_refresh_and_invalidation_cannot_resurrect_values() {
        let (cache, _) = fixture(PersistentCacheConfig::default()).await;
        let at = cache.now_ms();
        let old_order = cache.next_fetch_order();
        let new_order = cache.next_fetch_order();
        cache.store(
            CacheKind::Post,
            "post".into(),
            &"new",
            at,
            cache.generation(),
            new_order,
        );
        assert!(cache.flush().await);
        cache.store(
            CacheKind::Post,
            "post".into(),
            &"old",
            at,
            cache.generation(),
            old_order,
        );
        assert!(cache.flush().await);
        assert_eq!(
            cache
                .read::<String>(CacheKind::Post, &["post".into()])
                .await["post"]
                .value
                .as_str(),
            "new"
        );
        let old_generation = cache.generation();
        cache.invalidate(CacheKind::Post, "post");
        cache.store(
            CacheKind::Post,
            "post".into(),
            &"stale",
            at,
            old_generation,
            cache.next_fetch_order(),
        );
        assert!(cache
            .read::<String>(CacheKind::Post, &["post".into()])
            .await
            .is_empty());
        assert!(cache.flush().await);
        assert_eq!(
            sqlx::query_scalar::<_, i64>("SELECT COUNT(*) FROM post_cache")
                .fetch_one(&cache.inner.pool)
                .await
                .unwrap(),
            0
        );
    }

    #[tokio::test]
    async fn author_invalidation_is_targeted_durable_and_fences_older_post_fetches() {
        let (cache, _) = fixture(PersistentCacheConfig::default()).await;
        let at = cache.now_ms();
        let old_generation = cache.generation();
        for i in 0..40 {
            cache.store(
                CacheKind::Post,
                format!("at://did:plc:author/app.bsky.feed.post/{i}"),
                &"old",
                at,
                old_generation,
                cache.next_fetch_order(),
            );
        }
        let unrelated = "at://did:plc:author-other/app.bsky.feed.post/one";
        cache.store(
            CacheKind::Post,
            unrelated.into(),
            &"keep",
            at,
            old_generation,
            cache.next_fetch_order(),
        );
        assert!(cache.flush().await);
        cache.invalidate(CacheKind::Profile, "did:plc:author");
        // Both the read and a delayed write are fenced while bounded deletion
        // drains the author's rows. An unrelated old fetch may still persist.
        assert!(cache
            .read::<String>(
                CacheKind::Post,
                &["at://did:plc:author/app.bsky.feed.post/0".into()]
            )
            .await
            .is_empty());
        cache.store(
            CacheKind::Post,
            "at://did:plc:author/app.bsky.feed.post/late".into(),
            &"stale",
            at,
            old_generation,
            cache.next_fetch_order(),
        );
        cache.store(
            CacheKind::Profile,
            "did:plc:other".into(),
            &"keep",
            at,
            old_generation,
            cache.next_fetch_order(),
        );
        assert!(cache.flush().await);
        let restarted = PersistentEntityCache::new_with_clock(
            cache.inner.pool.clone(),
            PersistentCacheConfig::default(),
            Arc::clone(&cache.inner.clock),
        )
        .await
        .unwrap();
        assert_eq!(
            sqlx::query_scalar::<_, i64>("SELECT COUNT(*) FROM post_cache")
                .fetch_one(&cache.inner.pool)
                .await
                .unwrap(),
            1
        );
        assert_eq!(
            restarted
                .read::<String>(CacheKind::Post, &[unrelated.into()])
                .await[unrelated]
                .value
                .as_str(),
            "keep"
        );
        assert_eq!(
            restarted
                .read::<String>(CacheKind::Profile, &["did:plc:other".into()])
                .await
                .len(),
            1
        );
        assert!(
            cache.inner.pending.lock().unwrap().fences.len() <= cache.inner.config.queue_capacity
        );
    }

    #[tokio::test]
    async fn missing_results_are_ordered_against_already_active_sql_writes() {
        for missing_is_newer in [false, true] {
            let (cache, _) = fixture(PersistentCacheConfig::default()).await;
            let old = cache.begin_fetch();
            let new = cache.begin_fetch();
            let (write, missing) = if missing_is_newer {
                (old, new)
            } else {
                (new, old)
            };
            // Hold the sole connection after the worker has chosen its write,
            // exposing the gap between its fence check and SQLite execution.
            let connection = cache.inner.pool.acquire().await.unwrap();
            cache.store(
                CacheKind::Post,
                "ordered".into(),
                &"value",
                cache.now_ms(),
                write,
                cache.next_fetch_order(),
            );
            tokio::time::timeout(Duration::from_secs(1), async {
                loop {
                    if cache.inner.pending.lock().unwrap().active {
                        break;
                    }
                    tokio::task::yield_now().await;
                }
            })
            .await
            .unwrap();
            cache.invalidate_if_current(Some(missing), CacheKind::Post, "ordered");
            drop(connection);
            assert!(cache.flush().await);
            let hits = cache
                .read::<String>(CacheKind::Post, &["ordered".into()])
                .await;
            assert_eq!(hits.contains_key("ordered"), !missing_is_newer);
        }
    }

    #[tokio::test]
    async fn queue_and_invalidation_metadata_stay_bounded_under_overload() {
        let (cache, _) = fixture(PersistentCacheConfig {
            queue_capacity: 2,
            ..PersistentCacheConfig::default()
        })
        .await;
        // No await: the worker cannot consume this current-thread burst.
        for i in 0..1000 {
            cache.store(
                CacheKind::Profile,
                i.to_string(),
                &"value",
                cache.now_ms(),
                cache.generation(),
                cache.next_fetch_order(),
            );
        }
        {
            let pending = cache.inner.pending.lock().unwrap();
            assert!(pending.writes.len() <= 2);
            assert!(pending.bytes <= MAX_QUEUE_BYTES);
        }
        for i in 0..1000 {
            cache.invalidate(CacheKind::Profile, &i.to_string());
        }
        {
            let pending = cache.inner.pending.lock().unwrap();
            assert!(pending.invalidations.len() <= 2);
            assert!(pending.clear_all);
            assert!(pending.writes.is_empty());
        }
        assert!(cache.flush().await);
        assert!(cache
            .read::<String>(CacheKind::Profile, &["0".into()])
            .await
            .is_empty());
    }

    #[tokio::test]
    async fn cleanup_expires_below_budget_and_evicts_cold_entries_in_chunks() {
        let (cache, now) = fixture(PersistentCacheConfig::default()).await;
        for i in 0..4 {
            now.fetch_add(10, Ordering::SeqCst);
            cache.store(
                CacheKind::Post,
                i.to_string(),
                &"value",
                cache.now_ms(),
                cache.generation(),
                cache.next_fetch_order(),
            );
            assert!(cache.flush().await);
        }
        cache.maintain(0, 2, false).await.unwrap();
        let remaining: Vec<String> =
            sqlx::query_scalar("SELECT entity_key FROM post_cache ORDER BY entity_key")
                .fetch_all(&cache.inner.pool)
                .await
                .unwrap();
        assert_eq!(remaining, ["2", "3"]);
        now.fetch_add(300_001, Ordering::SeqCst);
        assert_eq!(cache.maintain(u64::MAX, 1, false).await.unwrap(), 1);
        assert_eq!(cache.maintain(u64::MAX, 1, false).await.unwrap(), 1);
        assert_eq!(
            sqlx::query_scalar::<_, i64>("SELECT COUNT(*) FROM post_cache")
                .fetch_one(&cache.inner.pool)
                .await
                .unwrap(),
            0
        );
    }

    #[tokio::test]
    async fn malformed_and_unsupported_entries_are_removed_without_values() {
        let (cache, _) = fixture(PersistentCacheConfig::default()).await;
        for (key, payload, version) in [("malformed", "{", 1), ("version", "\"value\"", 999)] {
            sqlx::query("INSERT INTO profile_cache VALUES(?,?,?,?,?,?,?,0)")
                .bind(key)
                .bind(payload)
                .bind(version)
                .bind(cache.now_ms())
                .bind(cache.now_ms() + 300_000)
                .bind(1)
                .bind(cache.now_ms())
                .execute(&cache.inner.pool)
                .await
                .unwrap();
        }
        assert!(cache
            .read::<String>(CacheKind::Profile, &["malformed".into(), "version".into()])
            .await
            .is_empty());
        assert!(cache.flush().await);
        assert_eq!(
            sqlx::query_scalar::<_, i64>("SELECT COUNT(*) FROM profile_cache")
                .fetch_one(&cache.inner.pool)
                .await
                .unwrap(),
            0
        );
    }

    async fn assert_usage_exact(pool: &SqlitePool) {
        for kind in [CacheKind::Profile, CacheKind::Post] {
            let measured = sqlx::query(&format!(
                "SELECT COALESCE(SUM(size_bytes),0) AS bytes,COUNT(*) AS entries FROM {}",
                kind.table()
            ))
            .fetch_one(pool)
            .await
            .unwrap();
            let recorded = sqlx::query(
                "SELECT logical_bytes,entry_count FROM hydration_cache_usage WHERE kind=?",
            )
            .bind(kind.as_str())
            .fetch_one(pool)
            .await
            .unwrap();
            assert_eq!(
                recorded.get::<i64, _>("logical_bytes"),
                measured.get::<i64, _>("bytes"),
                "{} byte accounting",
                kind.as_str()
            );
            assert_eq!(
                recorded.get::<i64, _>("entry_count"),
                measured.get::<i64, _>("entries"),
                "{} entry accounting",
                kind.as_str()
            );
        }
    }

    #[tokio::test]
    async fn usage_triggers_cover_bulk_upsert_rollback_eviction_delete_clear_and_restart() {
        let (cache, _) = fixture(PersistentCacheConfig::default()).await;
        let pool = &cache.inner.pool;
        assert_usage_exact(pool).await;
        for kind in [CacheKind::Profile, CacheKind::Post] {
            let mut query=QueryBuilder::<Sqlite>::new(format!("INSERT INTO {}(entity_key,payload,payload_version,fetched_at_ms,expires_at_ms,size_bytes,last_access_ms,generation) ",kind.table()));
            query.push_values(0..70, |mut row, i| {
                let key = format!("{}-{i}", kind.as_str());
                let payload = "\"value\"";
                let size = (key.len() + payload.len() + 128) as i64;
                row.push_bind(key)
                    .push_bind(payload)
                    .push_bind(PAYLOAD_VERSION)
                    .push_bind(cache.now_ms())
                    .push_bind(cache.now_ms() + 300_000)
                    .push_bind(size)
                    .push_bind(cache.now_ms() + i)
                    .push_bind(0);
            });
            query.build().execute(pool).await.unwrap();
        }
        assert_usage_exact(pool).await;
        sqlx::query("INSERT INTO profile_cache(entity_key,payload,payload_version,fetched_at_ms,expires_at_ms,size_bytes,last_access_ms,generation) VALUES('profile-69','\"updated\"',1,?,?,250,?,0) ON CONFLICT(entity_key) DO UPDATE SET payload=excluded.payload,size_bytes=excluded.size_bytes")
            .bind(cache.now_ms()).bind(cache.now_ms()+300_000).bind(cache.now_ms()).execute(pool).await.unwrap();
        assert_usage_exact(pool).await;
        let before: i64 = sqlx::query_scalar(LOGICAL_BYTES_SQL)
            .fetch_one(pool)
            .await
            .unwrap();
        let mut tx = pool.begin().await.unwrap();
        sqlx::query(
            "UPDATE profile_cache SET size_bytes=size_bytes+1000 WHERE entity_key='profile-69'",
        )
        .execute(&mut *tx)
        .await
        .unwrap();
        sqlx::query("DELETE FROM post_cache WHERE entity_key='post-69'")
            .execute(&mut *tx)
            .await
            .unwrap();
        tx.rollback().await.unwrap();
        assert_eq!(
            sqlx::query_scalar::<_, i64>(LOGICAL_BYTES_SQL)
                .fetch_one(pool)
                .await
                .unwrap(),
            before
        );
        assert_usage_exact(pool).await;

        assert_eq!(cache.maintain(0, 5, false).await.unwrap(), 5);
        assert_usage_exact(pool).await;
        sqlx::query("DELETE FROM profile_cache WHERE entity_key IN (SELECT entity_key FROM profile_cache LIMIT 10)").execute(pool).await.unwrap();
        assert_usage_exact(pool).await;
        let expected: i64 = sqlx::query_scalar(LOGICAL_BYTES_SQL)
            .fetch_one(pool)
            .await
            .unwrap();
        let restarted = PersistentEntityCache::new_with_clock(
            pool.clone(),
            PersistentCacheConfig::default(),
            Arc::clone(&cache.inner.clock),
        )
        .await
        .unwrap();
        assert_eq!(restarted.diagnostics().logical_bytes, expected as u64);
        assert_usage_exact(pool).await;

        restarted.invalidate_all();
        assert!(restarted.flush().await);
        assert_usage_exact(pool).await;
        assert_eq!(
            sqlx::query_scalar::<_, i64>(LOGICAL_BYTES_SQL)
                .fetch_one(pool)
                .await
                .unwrap(),
            0
        );
    }

    #[tokio::test]
    async fn missing_usage_schema_backfills_once_and_survives_repeated_initialization() {
        let (cache, _) = fixture(PersistentCacheConfig::default()).await;
        cache.store(
            CacheKind::Profile,
            "legacy".into(),
            &"value",
            cache.now_ms(),
            cache.generation(),
            cache.next_fetch_order(),
        );
        assert!(cache.flush().await);
        let pool = &cache.inner.pool;
        for table in ["profile_cache", "post_cache"] {
            for operation in ["insert", "update", "delete"] {
                sqlx::query(&format!("DROP TRIGGER {table}_usage_{operation}"))
                    .execute(pool)
                    .await
                    .unwrap();
            }
        }
        sqlx::query("DROP TABLE hydration_cache_usage")
            .execute(pool)
            .await
            .unwrap();
        ensure_schema(pool).await.unwrap();
        assert_usage_exact(pool).await;
        let expected: i64 = sqlx::query_scalar(LOGICAL_BYTES_SQL)
            .fetch_one(pool)
            .await
            .unwrap();
        assert!(expected > 0);
        ensure_schema(pool).await.unwrap();
        assert_eq!(
            sqlx::query_scalar::<_, i64>(LOGICAL_BYTES_SQL)
                .fetch_one(pool)
                .await
                .unwrap(),
            expected
        );
        sqlx::query("DELETE FROM profile_cache")
            .execute(pool)
            .await
            .unwrap();
        assert_usage_exact(pool).await;
        assert_eq!(
            sqlx::query_scalar::<_, i64>(LOGICAL_BYTES_SQL)
                .fetch_one(pool)
                .await
                .unwrap(),
            0
        );
    }
}

#[derive(Clone, Debug)]
pub struct PersistentCacheConfig {
    pub profile_ttl: Duration,
    pub post_ttl: Duration,
    pub queue_capacity: usize,
    pub timeout: Duration,
}

impl Default for PersistentCacheConfig {
    fn default() -> Self {
        Self {
            profile_ttl: Duration::from_secs(300),
            post_ttl: Duration::from_secs(300),
            queue_capacity: 1024,
            timeout: Duration::from_millis(50),
        }
    }
}

type Key = (CacheKind, String);

/// Result/mutation markers fence the affected entity and its emitted author.
/// Eviction advances a global floor before forgetting any marker, so an
/// arbitrarily delayed fetch cannot overwrite a newer outcome.
#[derive(Default)]
pub(crate) struct InvalidationFences {
    floor: u64,
    profiles: HashMap<String, u64>,
    posts: HashMap<String, u64>,
}

pub(crate) fn post_author(key: &str) -> Option<&str> {
    key.strip_prefix("at://")?.split('/').next()
}

impl InvalidationFences {
    #[cfg(test)]
    pub(crate) fn len(&self) -> usize {
        self.profiles.len() + self.posts.len()
    }
    pub(crate) fn allows(&self, generation: u64, kind: CacheKind, key: &str) -> bool {
        if generation < self.floor {
            return false;
        }
        let newer = |fence: Option<&u64>| fence.is_some_and(|fence| *fence > generation);
        match kind {
            CacheKind::Profile => !newer(self.profiles.get(key)),
            CacheKind::Post => {
                !newer(self.posts.get(key))
                    && !post_author(key).is_some_and(|author| newer(self.profiles.get(author)))
            }
        }
    }

    pub(crate) fn clear(&mut self, generation: u64) {
        self.floor = generation;
        self.profiles.clear();
        self.posts.clear();
    }

    /// Returns true for an oversized key that cannot retain a bounded fence.
    pub(crate) fn invalidate(
        &mut self,
        generation: u64,
        kind: CacheKind,
        key: &str,
        capacity: usize,
    ) -> bool {
        let entries = self.profiles.len() + self.posts.len();
        if key.len() > 4096 {
            self.clear(generation.saturating_add(1));
            return true;
        }
        let contains = match kind {
            CacheKind::Profile => self.profiles.contains_key(key),
            CacheKind::Post => self.posts.contains_key(key),
        };
        if entries >= capacity && !contains {
            let oldest_profile = self
                .profiles
                .iter()
                .min_by_key(|(_, generation)| **generation)
                .map(|(key, generation)| (CacheKind::Profile, key.clone(), *generation));
            let oldest_post = self
                .posts
                .iter()
                .min_by_key(|(_, generation)| **generation)
                .map(|(key, generation)| (CacheKind::Post, key.clone(), *generation));
            if let Some((kind, key, generation)) = [oldest_profile, oldest_post]
                .into_iter()
                .flatten()
                .min_by_key(|(_, _, generation)| *generation)
            {
                match kind {
                    CacheKind::Profile => {
                        self.profiles.remove(&key);
                    }
                    CacheKind::Post => {
                        self.posts.remove(&key);
                    }
                }
                self.floor = self.floor.max(generation.saturating_add(1));
            }
        }
        match kind {
            CacheKind::Profile => {
                self.profiles.insert(key.to_string(), generation);
            }
            CacheKind::Post => {
                self.posts.insert(key.to_string(), generation);
            }
        }
        false
    }
}

enum Write {
    Value {
        payload: String,
        fetched_at: i64,
        expires_at: i64,
        generation: u64,
        refresh_order: u64,
    },
    Access(i64),
}
impl Write {
    fn bytes(&self) -> usize {
        match self {
            Self::Value { payload, .. } => payload.len(),
            Self::Access(_) => 0,
        }
    }
}

#[derive(Default)]
struct Pending {
    writes: HashMap<Key, Write>,
    invalidations: HashMap<Key, u64>,
    clear_all: bool,
    bytes: usize,
    active: bool,
    fences: InvalidationFences,
}

struct Inner {
    pool: SqlitePool,
    config: PersistentCacheConfig,
    pending: Mutex<Pending>,
    notify: Arc<Notify>,
    generation: AtomicU64,
    fetch_order: AtomicU64,
    growth_allowed: AtomicBool,
    clock: Arc<dyn Fn() -> i64 + Send + Sync>,
    last_warning: AtomicU64,
    counters: Counters,
    logical_budget: AtomicU64,
    logical_bytes: AtomicU64,
    optional_slots: Semaphore,
    accounting: tokio::sync::Mutex<()>,
}

#[derive(Default)]
struct Counters {
    profile_hits: AtomicU64,
    post_hits: AtomicU64,
    profile_misses: AtomicU64,
    post_misses: AtomicU64,
    expired_or_invalid: AtomicU64,
    evictions: AtomicU64,
    bypasses: AtomicU64,
    persistence_errors: AtomicU64,
    persistence_drops: AtomicU64,
}

#[derive(Debug, Clone, Default, Serialize)]
pub struct PersistentCacheDiagnostics {
    pub profile_disk_hits: u64,
    pub post_disk_hits: u64,
    pub profile_disk_misses: u64,
    pub post_disk_misses: u64,
    pub expired_or_invalid: u64,
    pub evictions: u64,
    pub bypasses: u64,
    pub persistence_errors: u64,
    pub persistence_drops: u64,
    pub queue_depth: usize,
    pub queue_bytes: usize,
    pub logical_bytes: u64,
    pub growth_allowed: bool,
}

#[derive(Clone)]
pub struct PersistentEntityCache {
    inner: Arc<Inner>,
}

pub struct DiskHit<T> {
    pub value: Arc<T>,
    pub age: Duration,
    pub remaining: Duration,
    sampled_at: Instant,
}

impl<T> DiskHit<T> {
    /// Time spent decoding, waiting for the rest of a bulk lookup, or waiting
    /// to promote an entry consumes its existing deadline.
    pub fn freshness(&self) -> Option<(Duration, Duration)> {
        self.freshness_at(Instant::now())
    }
    fn freshness_at(&self, now: Instant) -> Option<(Duration, Duration)> {
        let elapsed = now.saturating_duration_since(self.sampled_at);
        let remaining = self.remaining.checked_sub(elapsed)?;
        (!remaining.is_zero()).then_some((self.age.saturating_add(elapsed), remaining))
    }
}

/// Called by normal schema setup and by the offline migration path.
pub async fn ensure_schema(pool: &SqlitePool) -> Result<(), sqlx::Error> {
    // An immediate transaction prevents concurrent writes from escaping the
    // one-time backfill between installing counters and their triggers.
    let mut tx = pool.begin_with("BEGIN IMMEDIATE").await?;
    sqlx::query("CREATE TABLE IF NOT EXISTS hydration_cache_usage (kind TEXT PRIMARY KEY NOT NULL CHECK(kind IN ('profile','post')), logical_bytes INTEGER NOT NULL CHECK(logical_bytes >= 0), entry_count INTEGER NOT NULL CHECK(entry_count >= 0))")
        .execute(&mut *tx).await?;
    for kind in [CacheKind::Profile, CacheKind::Post] {
        let table = kind.table();
        sqlx::query(&format!("CREATE TABLE IF NOT EXISTS {table} (entity_key TEXT PRIMARY KEY NOT NULL, payload TEXT NOT NULL, payload_version INTEGER NOT NULL, fetched_at_ms INTEGER NOT NULL, expires_at_ms INTEGER NOT NULL, size_bytes INTEGER NOT NULL, last_access_ms INTEGER NOT NULL, generation INTEGER NOT NULL DEFAULT 0)"))
            .execute(&mut *tx).await?;
        sqlx::query(&format!(
            "CREATE INDEX IF NOT EXISTS idx_{table}_expiry ON {table}(expires_at_ms)"
        ))
        .execute(&mut *tx)
        .await?;
        sqlx::query(&format!(
            "CREATE INDEX IF NOT EXISTS idx_{table}_lru ON {table}(last_access_ms,entity_key)"
        ))
        .execute(&mut *tx)
        .await?;
        let initialized: bool =
            sqlx::query_scalar("SELECT EXISTS(SELECT 1 FROM hydration_cache_usage WHERE kind=?)")
                .bind(kind.as_str())
                .fetch_one(&mut *tx)
                .await?;
        if !initialized {
            // This is a schema migration, never a periodic/startup rescan once
            // its transaction commits. New deployments have empty cache tables.
            sqlx::query(&format!("INSERT INTO hydration_cache_usage(kind,logical_bytes,entry_count) SELECT ?,COALESCE(SUM(size_bytes),0),COUNT(*) FROM {table}"))
                .bind(kind.as_str()).execute(&mut *tx).await?;
        }
        for (operation, clause, update) in [
            (
                "insert",
                "AFTER INSERT",
                "logical_bytes=logical_bytes+NEW.size_bytes,entry_count=entry_count+1",
            ),
            (
                "update",
                "AFTER UPDATE OF size_bytes",
                "logical_bytes=logical_bytes+NEW.size_bytes-OLD.size_bytes",
            ),
            (
                "delete",
                "AFTER DELETE",
                "logical_bytes=logical_bytes-OLD.size_bytes,entry_count=entry_count-1",
            ),
        ] {
            sqlx::query(&format!("CREATE TRIGGER IF NOT EXISTS {table}_usage_{operation} {clause} ON {table} BEGIN UPDATE hydration_cache_usage SET {update} WHERE kind='{}'; END",kind.as_str()))
                .execute(&mut *tx).await?;
        }
    }
    tx.commit().await?;
    Ok(())
}

impl PersistentEntityCache {
    pub async fn new(pool: SqlitePool, config: PersistentCacheConfig) -> Result<Self, sqlx::Error> {
        Self::new_with_clock(
            pool,
            config,
            Arc::new(|| chrono::Utc::now().timestamp_millis()),
        )
        .await
    }

    pub(crate) async fn new_with_clock(
        pool: SqlitePool,
        config: PersistentCacheConfig,
        clock: Arc<dyn Fn() -> i64 + Send + Sync>,
    ) -> Result<Self, sqlx::Error> {
        if config.queue_capacity == 0
            || config.timeout.is_zero()
            || config.profile_ttl.is_zero()
            || config.post_ttl.is_zero()
        {
            return Err(sqlx::Error::Protocol(
                "cache queue, timeout and TTLs must be positive".into(),
            ));
        }
        ensure_schema(&pool).await?;
        let logical_bytes: i64 = sqlx::query_scalar(LOGICAL_BYTES_SQL)
            .fetch_one(&pool)
            .await?;
        let optional_connections = pool
            .options()
            .get_max_connections()
            .saturating_sub(1)
            .max(1) as usize;
        let inner = Arc::new(Inner {
            pool,
            config,
            pending: Mutex::new(Pending::default()),
            notify: Arc::new(Notify::new()),
            generation: AtomicU64::new(0),
            fetch_order: AtomicU64::new(0),
            growth_allowed: AtomicBool::new(true),
            clock,
            last_warning: AtomicU64::new(0),
            counters: Counters::default(),
            logical_budget: AtomicU64::new(4096 * 1024 * 1024),
            logical_bytes: AtomicU64::new(logical_bytes.max(0) as u64),
            optional_slots: Semaphore::new(optional_connections),
            accounting: tokio::sync::Mutex::new(()),
        });
        let weak = Arc::downgrade(&inner);
        let notify = Arc::clone(&inner.notify);
        tokio::spawn(async move {
            loop {
                tokio::select! { _ = notify.notified() => {}, _ = tokio::time::sleep(Duration::from_secs(1)) => {} }
                let Some(inner) = weak.upgrade() else { break };
                let cache = Self { inner };
                // One optional operation at a time; yield between operations so
                // checkpoints/publication writes can acquire the shared writer.
                for _ in 0..64 {
                    if !cache.persist_one().await {
                        break;
                    }
                    tokio::time::sleep(Duration::from_millis(2)).await;
                }
            }
        });
        Ok(Self { inner })
    }

    pub fn set_growth_allowed(&self, allowed: bool) {
        self.inner.growth_allowed.store(allowed, Ordering::Release);
    }
    pub fn set_max_bytes(&self, max_bytes: u64) {
        self.inner
            .logical_budget
            .store(max_bytes, Ordering::Release);
    }

    pub fn diagnostics(&self) -> PersistentCacheDiagnostics {
        let c = &self.inner.counters;
        let pending = self.inner.pending.lock().unwrap_or_else(|e| e.into_inner());
        PersistentCacheDiagnostics {
            profile_disk_hits: c.profile_hits.load(Ordering::Relaxed),
            post_disk_hits: c.post_hits.load(Ordering::Relaxed),
            profile_disk_misses: c.profile_misses.load(Ordering::Relaxed),
            post_disk_misses: c.post_misses.load(Ordering::Relaxed),
            expired_or_invalid: c.expired_or_invalid.load(Ordering::Relaxed),
            evictions: c.evictions.load(Ordering::Relaxed),
            bypasses: c.bypasses.load(Ordering::Relaxed),
            persistence_errors: c.persistence_errors.load(Ordering::Relaxed),
            persistence_drops: c.persistence_drops.load(Ordering::Relaxed),
            queue_depth: pending.writes.len()
                + pending.invalidations.len()
                + usize::from(pending.clear_all)
                + usize::from(pending.active),
            queue_bytes: pending.bytes,
            logical_bytes: self.inner.logical_bytes.load(Ordering::Relaxed),
            growth_allowed: self.inner.growth_allowed.load(Ordering::Relaxed),
        }
    }
    pub fn now_ms(&self) -> i64 {
        (self.inner.clock)()
    }
    pub fn generation(&self) -> u64 {
        self.inner.generation.load(Ordering::Acquire)
    }
    pub(crate) fn begin_fetch(&self) -> u64 {
        self.inner.generation.fetch_add(1, Ordering::AcqRel) + 1
    }
    pub fn next_fetch_order(&self) -> u64 {
        self.inner.fetch_order.fetch_add(1, Ordering::Relaxed) + 1
    }
    pub fn ttl(&self, kind: CacheKind) -> Duration {
        match kind {
            CacheKind::Profile => self.inner.config.profile_ttl,
            CacheKind::Post => self.inner.config.post_ttl,
        }
    }

    fn blocked(&self, key: &Key) -> bool {
        let pending = self.inner.pending.lock().unwrap_or_else(|e| e.into_inner());
        Self::blocked_pending(&pending, key)
    }

    fn blocked_pending(pending: &Pending, key: &Key) -> bool {
        pending.clear_all
            || pending.invalidations.contains_key(key)
            || (key.0 == CacheKind::Post
                && post_author(&key.1).is_some_and(|author| {
                    pending
                        .invalidations
                        .contains_key(&(CacheKind::Profile, author.to_string()))
                }))
    }

    fn allows(&self, generation: u64, kind: CacheKind, key: &str) -> bool {
        self.inner
            .pending
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .fences
            .allows(generation, kind, key)
    }

    fn diagnostic(&self, outcome: &'static str) {
        let c = &self.inner.counters;
        match outcome {
            "queue_drop" | "oversize_drop" => {
                c.persistence_drops.fetch_add(1, Ordering::Relaxed);
            }
            "read_error" | "read_timeout" | "persistence_error" | "serialization_error" => {
                c.persistence_errors.fetch_add(1, Ordering::Relaxed);
            }
            _ => {
                c.bypasses.fetch_add(1, Ordering::Relaxed);
            }
        }
        metrics::counter!("hydration_disk_cache_operations_total", "outcome" => outcome)
            .increment(1);
        let now = self.now_ms().max(0) as u64;
        let previous = self.inner.last_warning.load(Ordering::Relaxed);
        if now.saturating_sub(previous) >= 60_000
            && self
                .inner
                .last_warning
                .compare_exchange(previous, now, Ordering::Relaxed, Ordering::Relaxed)
                .is_ok()
        {
            tracing::warn!(outcome, "Optional hydration disk cache bypassed");
        }
    }

    /// A single deadline covers the whole bulk lookup, including pool wait.
    pub async fn read<T: DeserializeOwned>(
        &self,
        kind: CacheKind,
        keys: &[String],
    ) -> HashMap<String, DiskHit<T>> {
        if keys.is_empty() {
            return HashMap::new();
        }
        let generation = self.generation();
        let read = async {
            let _permit = self
                .inner
                .optional_slots
                .acquire()
                .await
                .map_err(|_| sqlx::Error::PoolClosed)?;
            let mut hits = HashMap::new();
            for keys in keys.chunks(200) {
                let mut query = QueryBuilder::<Sqlite>::new(format!("SELECT entity_key,payload,payload_version,fetched_at_ms,expires_at_ms FROM {} WHERE length(payload) <= {} AND entity_key IN (", kind.table(), MAX_ENTRY_BYTES));
                let mut separated = query.separated(",");
                for key in keys {
                    separated.push_bind(key);
                }
                separated.push_unseparated(")");
                let rows = query.build().fetch_all(&self.inner.pool).await?;
                let now = self.now_ms();
                let sampled_at = Instant::now();
                let ttl = self.ttl(kind).as_millis().min(i64::MAX as u128) as i64;
                for row in rows {
                    let key: String = row.try_get("entity_key")?;
                    if self.blocked(&(kind, key.clone())) {
                        continue;
                    }
                    let fetched_at: i64 = row.try_get("fetched_at_ms")?;
                    let stored_expiry: i64 = row.try_get("expires_at_ms")?;
                    let version: i64 = row.try_get("payload_version")?;
                    let expiry = stored_expiry.min(fetched_at.saturating_add(ttl));
                    let payload: String = row.try_get("payload")?;
                    if version != PAYLOAD_VERSION
                        || fetched_at > now
                        || fetched_at < 0
                        || stored_expiry < fetched_at
                        || expiry <= now
                    {
                        self.inner
                            .counters
                            .expired_or_invalid
                            .fetch_add(1, Ordering::Relaxed);
                        metrics::counter!("hydration_disk_cache_entities_total", "kind" => kind.as_str(), "outcome" => "expired_or_invalid").increment(1);
                        self.discard_invalid(kind, &key);
                        continue;
                    }
                    match serde_json::from_str(&payload) {
                        Ok(value) => {
                            hits.insert(
                                key.clone(),
                                DiskHit {
                                    value: Arc::new(value),
                                    age: Duration::from_millis((now - fetched_at) as u64),
                                    remaining: Duration::from_millis((expiry - now) as u64),
                                    sampled_at,
                                },
                            );
                            self.enqueue((kind, key), Write::Access(now));
                        }
                        Err(_) => {
                            self.inner
                                .counters
                                .expired_or_invalid
                                .fetch_add(1, Ordering::Relaxed);
                            self.discard_invalid(kind, &key);
                        }
                    }
                }
            }
            Ok::<_, sqlx::Error>(hits)
        };
        match tokio::time::timeout(self.inner.config.timeout, read).await {
            Ok(Ok(mut hits)) => {
                hits.retain(|key, _| {
                    self.allows(generation, kind, key) && !self.blocked(&(kind, key.clone()))
                });
                let (hit_counter, miss_counter) = match kind {
                    CacheKind::Profile => (
                        &self.inner.counters.profile_hits,
                        &self.inner.counters.profile_misses,
                    ),
                    CacheKind::Post => (
                        &self.inner.counters.post_hits,
                        &self.inner.counters.post_misses,
                    ),
                };
                hit_counter.fetch_add(hits.len() as u64, Ordering::Relaxed);
                miss_counter.fetch_add(
                    keys.len().saturating_sub(hits.len()) as u64,
                    Ordering::Relaxed,
                );
                metrics::counter!("hydration_disk_cache_entities_total", "kind" => kind.as_str(), "outcome" => "hit").increment(hits.len() as u64);
                hits
            }
            Ok(Err(_)) => {
                self.diagnostic("read_error");
                HashMap::new()
            }
            Err(_) => {
                self.diagnostic("read_timeout");
                HashMap::new()
            }
        }
    }

    pub fn store<T: Serialize>(
        &self,
        kind: CacheKind,
        key: String,
        value: &T,
        fetched_at: i64,
        generation: u64,
        refresh_order: u64,
    ) {
        if !self.inner.growth_allowed.load(Ordering::Acquire)
            || !self.allows(generation, kind, &key)
            || self.inner.logical_bytes.load(Ordering::Relaxed)
                >= self.inner.logical_budget.load(Ordering::Acquire)
        {
            self.diagnostic("growth_bypass");
            return;
        }
        let Ok(payload) = serde_json::to_string(value) else {
            self.diagnostic("serialization_error");
            return;
        };
        if payload.len() > MAX_ENTRY_BYTES {
            self.diagnostic("oversize_drop");
            return;
        }
        let expires_at =
            fetched_at.saturating_add(self.ttl(kind).as_millis().min(i64::MAX as u128) as i64);
        if expires_at <= self.now_ms() {
            return;
        }
        self.enqueue(
            (kind, key),
            Write::Value {
                payload,
                fetched_at,
                expires_at,
                generation,
                refresh_order,
            },
        );
    }

    fn enqueue(&self, key: Key, write: Write) {
        let mut pending = self.inner.pending.lock().unwrap_or_else(|e| e.into_inner());
        if let Write::Value { generation, .. } = &write {
            if !pending.fences.allows(*generation, key.0, &key.1) {
                return;
            }
        }
        if pending.clear_all
            || (pending.invalidations.contains_key(&key) && matches!(write, Write::Access(_)))
        {
            return;
        }
        if matches!(write, Write::Access(_)) && pending.writes.contains_key(&key) {
            return;
        }
        if let (
            Some(Write::Value {
                fetched_at: previous,
                refresh_order: previous_order,
                ..
            }),
            Write::Value {
                fetched_at,
                refresh_order,
                ..
            },
        ) = (pending.writes.get(&key), &write)
        {
            if (previous, previous_order) > (fetched_at, refresh_order) {
                return;
            }
        }
        let old_bytes = pending.writes.get(&key).map_or(0, Write::bytes);
        let next_bytes = pending
            .bytes
            .saturating_sub(old_bytes)
            .saturating_add(write.bytes())
            .saturating_add(if pending.writes.contains_key(&key) {
                0
            } else {
                key.1.len() + 128
            });
        if (!pending.writes.contains_key(&key)
            && pending.writes.len() >= self.inner.config.queue_capacity)
            || next_bytes > MAX_QUEUE_BYTES
        {
            drop(pending);
            self.diagnostic("queue_drop");
            return;
        }
        if let Write::Value { generation, .. } = &write {
            pending
                .fences
                .invalidate(*generation, key.0, &key.1, self.inner.config.queue_capacity);
            if !pending.fences.allows(*generation, key.0, &key.1) {
                return;
            }
        }
        pending.bytes = next_bytes;
        pending.writes.insert(key, write);
        drop(pending);
        self.inner.notify.notify_one();
    }

    /// A saturated deletion queue falls back to a full clear and bypasses disk
    /// reads until that clear succeeds. Marker eviction only advances a floor.
    pub fn invalidate(&self, kind: CacheKind, key: &str) {
        self.invalidate_if_current(None, kind, key);
    }

    pub(crate) fn invalidate_if_current(
        &self,
        generation: Option<u64>,
        kind: CacheKind,
        key: &str,
    ) {
        let mut pending = self.inner.pending.lock().unwrap_or_else(|e| e.into_inner());
        if generation.is_some_and(|generation| !pending.fences.allows(generation, kind, key)) {
            return;
        }
        let generation =
            generation.unwrap_or_else(|| self.inner.generation.fetch_add(1, Ordering::AcqRel) + 1);
        if pending
            .fences
            .invalidate(generation, kind, key, self.inner.config.queue_capacity)
        {
            Self::queue_clear(&mut pending);
        } else {
            Self::queue_invalidation(
                &mut pending,
                kind,
                key,
                generation,
                self.inner.config.queue_capacity,
            );
        }
        drop(pending);
        self.inner.notify.notify_one();
    }

    // Expiry/decoding failure is local to a row. It must not fence unrelated
    // valid hits from the same bulk read or unrelated in-flight refreshes.
    fn discard_invalid(&self, kind: CacheKind, key: &str) {
        let mut pending = self.inner.pending.lock().unwrap_or_else(|e| e.into_inner());
        Self::queue_invalidation(
            &mut pending,
            kind,
            key,
            self.generation(),
            self.inner.config.queue_capacity,
        );
        drop(pending);
        self.inner.notify.notify_one();
    }

    fn queue_clear(pending: &mut Pending) {
        pending.clear_all = true;
        pending.writes.clear();
        pending.invalidations.clear();
        pending.bytes = 0;
    }

    fn queue_invalidation(
        pending: &mut Pending,
        kind: CacheKind,
        key: &str,
        generation: u64,
        capacity: usize,
    ) {
        let key = (kind, key.to_string());
        if let Some(old) = pending.writes.remove(&key) {
            pending.bytes = pending
                .bytes
                .saturating_sub(old.bytes() + key.1.len() + 128);
        }
        if key.1.len() > 4096
            || (pending.invalidations.len() >= capacity
                && !pending.invalidations.contains_key(&key))
        {
            // Preserve fences even when a row-local corrupt-entry cleanup
            // saturates its queue; invalidation fences are bounded separately.
            Self::queue_clear(pending);
            metrics::counter!("hydration_disk_cache_operations_total", "outcome" => "invalidation_saturation").increment(1);
        } else if !pending.clear_all {
            pending.invalidations.insert(key, generation);
        }
    }

    /// Identity/account changes also affect authors embedded in cached posts.
    /// Clearing the tier avoids an unbounded author-to-post reverse index.
    pub fn invalidate_all(&self) {
        let mut pending = self.inner.pending.lock().unwrap_or_else(|e| e.into_inner());
        let generation = self.inner.generation.fetch_add(1, Ordering::AcqRel) + 1;
        pending.fences.clear(generation);
        Self::queue_clear(&mut pending);
        drop(pending);
        self.inner.notify.notify_one();
    }

    async fn persist_one(&self) -> bool {
        let Ok(_permit) = self.inner.optional_slots.try_acquire() else {
            return false;
        };
        let _accounting = self.inner.accounting.lock().await;
        enum Operation {
            Clear(u64),
            Delete(Key, u64),
            Write(Key, Write),
        }
        let operation = {
            let mut pending = self.inner.pending.lock().unwrap_or_else(|e| e.into_inner());
            let operation = if pending.clear_all {
                Some(Operation::Clear(self.generation()))
            } else if let Some((key, generation)) = pending.invalidations.iter().next() {
                Some(Operation::Delete(key.clone(), *generation))
            } else if let Some(key) = pending.writes.keys().next().cloned() {
                let Some(write) = pending.writes.remove(&key) else {
                    return false;
                };
                pending.bytes = pending
                    .bytes
                    .saturating_sub(write.bytes() + key.1.len() + 128);
                Some(Operation::Write(key, write))
            } else {
                None
            };
            pending.active = operation.is_some();
            operation
        };
        let Some(operation) = operation else {
            return false;
        };
        let result = tokio::time::timeout(self.inner.config.timeout, async {
            let mut clear_complete=true;
            match &operation {
                Operation::Clear(_) => {
                    let mut tx = self.inner.pool.begin().await?;
                    let profiles=sqlx::query("DELETE FROM profile_cache WHERE entity_key IN (SELECT entity_key FROM profile_cache LIMIT 32)").execute(&mut *tx).await?.rows_affected();
                    let posts=sqlx::query("DELETE FROM post_cache WHERE entity_key IN (SELECT entity_key FROM post_cache LIMIT 32)").execute(&mut *tx).await?.rows_affected();
                    clear_complete=profiles<32 && posts<32;
                    tx.commit().await?;
                }
                Operation::Delete((kind,key),_) => {
                    let mut tx = self.inner.pool.begin().await?;
                    sqlx::query(&format!("DELETE FROM {} WHERE entity_key=?",kind.table())).bind(key).execute(&mut *tx).await?;
                    if *kind == CacheKind::Profile {
                        // ReferencedPost emits only the outer author's profile,
                        // not nested quote/reply profiles. AT-URIs carry that
                        // author's DID, so its dependency range stays indexed.
                        let start = format!("at://{key}/");
                        let end = format!("at://{key}0");
                        let posts = sqlx::query("DELETE FROM post_cache WHERE entity_key IN (SELECT entity_key FROM post_cache WHERE entity_key>=? AND entity_key<? LIMIT 32)").bind(start).bind(end).execute(&mut *tx).await?.rows_affected();
                        clear_complete = posts < 32;
                    }
                    tx.commit().await?;
                }
                Operation::Write((kind,key),Write::Access(at)) => { sqlx::query(&format!("UPDATE {} SET last_access_ms=MAX(last_access_ms,?) WHERE entity_key=?",kind.table())).bind(at).bind(key).execute(&self.inner.pool).await?; }
                Operation::Write((kind,key),Write::Value {payload,fetched_at,expires_at,generation,refresh_order}) => {
                    if self.allows(*generation, *kind, key) && self.inner.growth_allowed.load(Ordering::Acquire) {
                        let size=(payload.len()+key.len()+128) as u64;
                        if self.inner.logical_bytes.load(Ordering::Relaxed).saturating_add(size)>self.inner.logical_budget.load(Ordering::Acquire) {
                            self.diagnostic("logical_budget_bypass");return Ok(true);
                        }
                        let changed=sqlx::query(&format!("INSERT INTO {}(entity_key,payload,payload_version,fetched_at_ms,expires_at_ms,size_bytes,last_access_ms,generation) VALUES(?,?,?,?,?,?,?,?) ON CONFLICT(entity_key) DO UPDATE SET payload=excluded.payload,payload_version=excluded.payload_version,fetched_at_ms=excluded.fetched_at_ms,expires_at_ms=excluded.expires_at_ms,size_bytes=excluded.size_bytes,last_access_ms=excluded.last_access_ms,generation=excluded.generation WHERE excluded.fetched_at_ms > fetched_at_ms OR (excluded.fetched_at_ms = fetched_at_ms AND excluded.generation >= generation)", kind.table()))
                            .bind(key).bind(payload).bind(PAYLOAD_VERSION).bind(fetched_at).bind(expires_at).bind((payload.len()+key.len()+128) as i64).bind(self.now_ms()).bind(*refresh_order as i64).execute(&self.inner.pool).await?;
                        // Conservative between maintenance samples: refreshes
                        // may count twice but can never admit excess growth.
                        if changed.rows_affected()>0 {self.inner.logical_bytes.fetch_add(size,Ordering::Relaxed);}
                    }
                }
            }
            Ok::<bool,sqlx::Error>(clear_complete)
        }).await;
        let success = matches!(result, Ok(Ok(_)));
        let clear_complete = matches!(result, Ok(Ok(true)));
        let mut pending = self.inner.pending.lock().unwrap_or_else(|e| e.into_inner());
        pending.active = false;
        if success {
            match operation {
                Operation::Clear(generation)
                    if generation == self.generation() && clear_complete =>
                {
                    pending.clear_all = false;
                    pending.invalidations.clear();
                    self.inner.logical_bytes.store(0, Ordering::Relaxed);
                }
                Operation::Delete(key, generation)
                    if pending.invalidations.get(&key) == Some(&generation) && clear_complete =>
                {
                    pending.invalidations.remove(&key);
                }
                _ => {}
            }
        }
        drop(pending);
        if !success {
            self.diagnostic("persistence_error");
        }
        success
    }

    /// Wait for optional queued work to settle (useful for graceful shutdown).
    pub async fn flush(&self) -> bool {
        self.inner.notify.notify_one();
        tokio::time::timeout(Duration::from_secs(5), async {
            loop {
                let idle = {
                    let pending = self.inner.pending.lock().unwrap_or_else(|e| e.into_inner());
                    !pending.active
                        && pending.writes.is_empty()
                        && pending.invalidations.is_empty()
                        && !pending.clear_all
                };
                if idle {
                    break;
                }
                tokio::time::sleep(Duration::from_millis(5)).await;
            }
        })
        .await
        .is_ok()
    }

    /// Bounded expiry first, then deterministic approximate LRU. Pressure may
    /// evict below the logical budget to free shared SQLite pages for recovery.
    pub async fn maintain(
        &self,
        max_bytes: u64,
        chunk_size: u32,
        pressure: bool,
    ) -> Result<u64, sqlx::Error> {
        let _accounting = self.inner.accounting.lock().await;
        self.set_max_bytes(max_bytes);
        let (deleted, bytes) = maintain_at(
            &self.inner.pool,
            self.now_ms(),
            max_bytes,
            chunk_size,
            pressure,
            self.ttl(CacheKind::Profile),
            self.ttl(CacheKind::Post),
        )
        .await?;
        self.inner.logical_bytes.store(bytes, Ordering::Relaxed);
        self.inner
            .counters
            .evictions
            .fetch_add(deleted, Ordering::Relaxed);
        Ok(deleted)
    }
}

/// Retention continues when the lookup rollback switch is disabled.
pub async fn maintain_pool(
    pool: &SqlitePool,
    max_bytes: u64,
    chunk_size: u32,
    pressure: bool,
    profile_ttl: Duration,
    post_ttl: Duration,
) -> Result<u64, sqlx::Error> {
    maintain_at(
        pool,
        chrono::Utc::now().timestamp_millis(),
        max_bytes,
        chunk_size,
        pressure,
        profile_ttl,
        post_ttl,
    )
    .await
    .map(|(deleted, _)| deleted)
}

async fn maintain_at(
    pool: &SqlitePool,
    now: i64,
    max_bytes: u64,
    chunk_size: u32,
    pressure: bool,
    profile_ttl: Duration,
    post_ttl: Duration,
) -> Result<(u64, u64), sqlx::Error> {
    let chunk_size = chunk_size.clamp(1, 10_000) as i64;
    let mut deleted = 0;
    for kind in [CacheKind::Profile, CacheKind::Post] {
        let expiry_limit = now.saturating_sub(
            match kind {
                CacheKind::Profile => profile_ttl,
                CacheKind::Post => post_ttl,
            }
            .as_millis()
            .min(i64::MAX as u128) as i64,
        );
        deleted+=sqlx::query(&format!("DELETE FROM {} WHERE entity_key IN (SELECT entity_key FROM {} WHERE expires_at_ms<=? OR fetched_at_ms<=? OR fetched_at_ms>? OR payload_version!=? ORDER BY expires_at_ms,entity_key LIMIT ?)",kind.table(),kind.table()))
                .bind(now).bind(expiry_limit).bind(now).bind(PAYLOAD_VERSION).bind(chunk_size).execute(pool).await?.rows_affected();
    }
    let bytes: i64 = sqlx::query_scalar(LOGICAL_BYTES_SQL)
        .fetch_one(pool)
        .await?;
    metrics::gauge!("hydration_disk_cache_logical_bytes").set(bytes as f64);
    if pressure || bytes as u64 > max_bytes {
        let rows=sqlx::query("SELECT kind,entity_key FROM (SELECT 'profile' AS kind,entity_key,last_access_ms FROM profile_cache UNION ALL SELECT 'post' AS kind,entity_key,last_access_ms FROM post_cache) ORDER BY last_access_ms,kind,entity_key LIMIT ?").bind(chunk_size).fetch_all(pool).await?;
        for kind in [CacheKind::Profile, CacheKind::Post] {
            let keys: Vec<String> = rows
                .iter()
                .filter(|row| row.get::<String, _>("kind") == kind.as_str())
                .map(|row| row.get("entity_key"))
                .collect();
            if keys.is_empty() {
                continue;
            }
            let mut query = QueryBuilder::<Sqlite>::new(format!(
                "DELETE FROM {} WHERE entity_key IN (",
                kind.table()
            ));
            let mut separated = query.separated(",");
            for key in keys {
                separated.push_bind(key);
            }
            separated.push_unseparated(")");
            deleted += query.build().execute(pool).await?.rows_affected();
        }
    }
    let remaining: i64 = sqlx::query_scalar(LOGICAL_BYTES_SQL)
        .fetch_one(pool)
        .await?;
    metrics::counter!("hydration_disk_cache_evictions_total").increment(deleted);
    Ok((deleted, remaining.max(0) as u64))
}
