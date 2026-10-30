use crate::client::{PostFetchOutcome, PostFetcher, ProfileFetcher};
use crate::hydration::entity_cache::{CacheKind, PersistentEntityCache};
use crate::hydration::TurboCache;
use crate::models::bluesky::{BlueskyPost, BlueskyProfile};
use crate::models::jetstream::{JetstreamMessage, MessageKind, OperationType};
use crate::models::{TurboError, TurboResult};
use std::collections::HashMap;
use std::sync::Arc;
use std::time::Instant;

/// Resolves unique RAM misses through optional disk storage and existing API
/// fetchers, then reconstructs the original order and cardinality.
pub struct CacheMissResolver<P, Po> {
    cache: TurboCache,
    profile_fetcher: Arc<P>,
    post_fetcher: Arc<Po>,
    persistent: Option<Arc<PersistentEntityCache>>,
}

impl<P, Po> Clone for CacheMissResolver<P, Po> {
    fn clone(&self) -> Self {
        Self {
            cache: self.cache.clone(),
            profile_fetcher: Arc::clone(&self.profile_fetcher),
            post_fetcher: Arc::clone(&self.post_fetcher),
            persistent: self.persistent.clone(),
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
            persistent: None,
        }
    }

    pub fn with_persistent_cache(mut self, persistent: Arc<PersistentEntityCache>) -> Self {
        self.persistent = Some(persistent);
        self
    }

    pub fn persistent_cache(&self) -> Option<&Arc<PersistentEntityCache>> {
        self.persistent.as_ref()
    }

    /// Applied before resolving any entities referenced by the observed event.
    /// Both tiers keep bounded entity fences; unrelated cache entries survive.
    pub fn observe_message(&self, message: &JetstreamMessage) {
        let invalidation = match message.kind {
            MessageKind::Identity | MessageKind::Account => {
                Some((CacheKind::Profile, message.did.to_string()))
            }
            MessageKind::Commit => message.commit.as_ref().and_then(|commit| {
                match (commit.collection.as_deref(), commit.operation_type) {
                    (Some("app.bsky.feed.post"), OperationType::Update | OperationType::Delete) => {
                        message.extract_at_uri().map(|uri| (CacheKind::Post, uri))
                    }
                    (Some("app.bsky.actor.profile"), _) => {
                        Some((CacheKind::Profile, message.did.to_string()))
                    }
                    _ => None,
                }
            }),
            _ => None,
        };
        if let Some((kind, key)) = invalidation {
            self.cache.invalidate_with(kind, &key, || {
                if let Some(disk) = &self.persistent {
                    disk.invalidate(kind, &key);
                }
            });
        }
    }

    pub async fn resolve_profile(&self, did: &str) -> TurboResult<Option<Arc<BlueskyProfile>>> {
        Ok(self
            .resolve_profiles(&[did])
            .await?
            .into_iter()
            .next()
            .flatten())
    }

    pub async fn resolve_post(&self, uri: &str) -> TurboResult<PostFetchOutcome> {
        self.resolve_posts(&[uri])
            .await?
            .into_iter()
            .next()
            .ok_or_else(|| TurboError::InvalidApiResponse("missing post outcome".into()))
    }

    pub async fn resolve_profiles<S: AsRef<str>>(
        &self,
        dids: &[S],
    ) -> TurboResult<Vec<Option<Arc<BlueskyProfile>>>> {
        if dids.is_empty() {
            return Ok(Vec::new());
        }
        let cached = self.cache.get_user_profiles(dids);
        if cached.iter().all(Option::is_some) {
            return Ok(cached);
        }
        let generation = self.cache.generation();
        let mut unique = HashMap::with_capacity(dids.len());
        for (did, value) in dids.iter().zip(cached) {
            unique.entry(did.as_ref().to_string()).or_insert(value);
        }
        let misses: Vec<String> = unique
            .iter()
            .filter(|(_, value)| value.is_none())
            .map(|(key, _)| key.clone())
            .collect();
        if let Some(disk) = &self.persistent {
            for (key, hit) in disk
                .read::<BlueskyProfile>(CacheKind::Profile, &misses)
                .await
            {
                // Reject entries whose serialized identity does not match their key.
                if hit.value.did.as_ref() != key {
                    disk.invalidate(CacheKind::Profile, &key);
                    continue;
                }
                self.cache
                    .if_current(generation, CacheKind::Profile, &key, || {
                        let Some((age, remaining)) = hit.freshness() else {
                            return;
                        };
                        self.cache.set_user_profile_with_freshness(
                            key.clone(),
                            Arc::clone(&hit.value),
                            age,
                            remaining,
                        );
                        unique.insert(key.clone(), Some(hit.value));
                    });
            }
        }
        let mut misses: Vec<String> = unique
            .iter()
            .filter(|(_, value)| value.is_none())
            .map(|(key, _)| key.clone())
            .collect();
        // Stable ordering also makes coordinated mock and production batches reproducible.
        misses.sort();
        if !misses.is_empty() {
            metrics::counter!("hydration_api_entities_total","kind"=>"profile")
                .increment(misses.len() as u64);
            let (generation, (fetched_at, disk_generation, refresh_order)) =
                self.cache.begin_fetch(|| {
                    self.persistent.as_ref().map_or((0, 0, 0), |disk| {
                        (disk.now_ms(), disk.begin_fetch(), disk.next_fetch_order())
                    })
                });
            let started = Instant::now();
            let fetched = self.profile_fetcher.bulk_fetch_profiles(&misses).await?;
            if fetched.len() != misses.len() {
                return Err(TurboError::InvalidApiResponse(format!(
                    "profile outcome cardinality mismatch: requested {}, received {}",
                    misses.len(),
                    fetched.len()
                )));
            }
            for (did, value) in misses.into_iter().zip(fetched) {
                if let Some(profile) = &value {
                    self.cache
                        .finish_fetch(generation, CacheKind::Profile, &did, || {
                            let ttl = self
                                .persistent
                                .as_ref()
                                .map_or(self.cache.profile_ttl(), |disk| {
                                    disk.ttl(CacheKind::Profile)
                                });
                            self.cache.set_user_profile_with_freshness(
                                did.clone(),
                                Arc::clone(profile),
                                started.elapsed(),
                                ttl.saturating_sub(started.elapsed()),
                            );
                            if let Some(disk) = &self.persistent {
                                disk.store(
                                    CacheKind::Profile,
                                    did.clone(),
                                    profile.as_ref(),
                                    fetched_at,
                                    disk_generation,
                                    refresh_order,
                                );
                            }
                        });
                } else {
                    self.cache.invalidate_if_current(
                        Some(generation),
                        CacheKind::Profile,
                        &did,
                        || {
                            if let Some(disk) = &self.persistent {
                                disk.invalidate_if_current(
                                    Some(disk_generation),
                                    CacheKind::Profile,
                                    &did,
                                );
                            }
                        },
                    );
                }
                unique.insert(did, value);
            }
        }
        Ok(dids
            .iter()
            .map(|did| unique.get(did.as_ref()).cloned().flatten())
            .collect())
    }

    pub async fn resolve_posts<S: AsRef<str>>(
        &self,
        uris: &[S],
    ) -> TurboResult<Vec<PostFetchOutcome>> {
        if uris.is_empty() {
            return Ok(Vec::new());
        }
        let cached = self.cache.get_posts_by_keys(uris);
        if cached.iter().all(Option::is_some) {
            return Ok(cached
                .into_iter()
                .flatten()
                .map(PostFetchOutcome::Found)
                .collect());
        }
        let generation = self.cache.generation();
        let mut unique = HashMap::with_capacity(uris.len());
        for (uri, value) in uris.iter().zip(cached) {
            unique.entry(uri.as_ref().to_string()).or_insert_with(|| {
                value.map(PostFetchOutcome::Found).or_else(|| {
                    self.cache
                        .get_unavailable_post(uri.as_ref())
                        .map(PostFetchOutcome::TemporarilyUnavailable)
                })
            });
        }
        let misses: Vec<String> = unique
            .iter()
            .filter(|(_, value)| value.is_none())
            .map(|(key, _)| key.clone())
            .collect();
        if let Some(disk) = &self.persistent {
            for (key, hit) in disk.read::<BlueskyPost>(CacheKind::Post, &misses).await {
                if hit.value.uri != key
                    || crate::hydration::entity_cache::post_author(&key)
                        != Some(hit.value.author.did.as_ref())
                {
                    disk.invalidate(CacheKind::Post, &key);
                    continue;
                }
                self.cache
                    .if_current(generation, CacheKind::Post, &key, || {
                        let Some((age, remaining)) = hit.freshness() else {
                            return;
                        };
                        self.cache.set_post_with_freshness(
                            key.clone(),
                            Arc::clone(&hit.value),
                            age,
                            remaining,
                        );
                        unique.insert(key.clone(), Some(PostFetchOutcome::Found(hit.value)));
                    });
            }
        }
        let mut misses: Vec<String> = unique
            .iter()
            .filter(|(_, value)| value.is_none())
            .map(|(key, _)| key.clone())
            .collect();
        misses.sort();
        if !misses.is_empty() {
            metrics::counter!("hydration_api_entities_total","kind"=>"post")
                .increment(misses.len() as u64);
            let (generation, (fetched_at, disk_generation, refresh_order)) =
                self.cache.begin_fetch(|| {
                    self.persistent.as_ref().map_or((0, 0, 0), |disk| {
                        (disk.now_ms(), disk.begin_fetch(), disk.next_fetch_order())
                    })
                });
            let started = Instant::now();
            let fetched = self.post_fetcher.bulk_fetch_posts(&misses).await?;
            if fetched.len() != misses.len() {
                return Err(TurboError::InvalidApiResponse(format!(
                    "post outcome cardinality mismatch: requested {}, received {}",
                    misses.len(),
                    fetched.len()
                )));
            }
            for (uri, outcome) in misses.into_iter().zip(fetched) {
                self.cache.record_post_outcome(&outcome);
                if matches!(outcome, PostFetchOutcome::Missing) {
                    self.cache.invalidate_if_current(
                        Some(generation),
                        CacheKind::Post,
                        &uri,
                        || {
                            self.cache.complete_post_resolution(&uri, "missing");
                            if let Some(disk) = &self.persistent {
                                disk.invalidate_if_current(
                                    Some(disk_generation),
                                    CacheKind::Post,
                                    &uri,
                                );
                            }
                        },
                    );
                    metrics::counter!("optional_hydration_post_outcomes_total","outcome"=>"missing").increment(1);
                } else {
                    self.cache.finish_fetch(generation,CacheKind::Post,&uri,||match &outcome {
                    PostFetchOutcome::Found(post)=> {
                        let ttl=self.persistent.as_ref().map_or(self.cache.post_ttl(),|disk|disk.ttl(CacheKind::Post));
                        self.cache.set_post_with_freshness(uri.clone(),Arc::clone(post),started.elapsed(),ttl.saturating_sub(started.elapsed()));
                        if let Some(disk)=&self.persistent {disk.store(CacheKind::Post,uri.clone(),post.as_ref(),fetched_at,disk_generation,refresh_order);}
                        metrics::counter!("optional_hydration_post_outcomes_total","outcome"=>"found").increment(1);
                    }
                    PostFetchOutcome::Missing=> {}
                    PostFetchOutcome::TemporarilyUnavailable(failure)=> {
                        self.cache.set_unavailable_post(uri.clone(),failure.clone());
                        metrics::counter!("optional_hydration_post_outcomes_total", "outcome" => "temporarily_unavailable", "category" => failure.category.as_str(), "status_class" => failure.status_class.clone().unwrap_or_else(|| "none".to_string()), "isolation" => failure.isolation.as_ref().map_or("none", |value| value.as_str())).increment(1);
                        tracing::warn!(operation=failure.operation.as_str(),category=failure.category.as_str(),status_class=failure.status_class.as_deref(),attempts=failure.attempts,request_fingerprint=failure.request_fingerprint,isolation=failure.isolation.as_ref().map(|value|value.as_str()),"Optional referenced-post hydration degraded");
                    }
                });
                }
                unique.insert(uri, Some(outcome));
            }
        }
        Ok(uris
            .iter()
            .map(|uri| {
                unique
                    .get(uri.as_ref())
                    .cloned()
                    .flatten()
                    .unwrap_or(PostFetchOutcome::Missing)
            })
            .collect())
    }

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
            author: test_profile(
                crate::hydration::entity_cache::post_author(uri).unwrap_or("did:plc:author"),
            ),
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
    async fn mixed_missing_profiles_and_posts_persist_successful_peers() {
        use crate::hydration::entity_cache::{PersistentCacheConfig, PersistentEntityCache};
        let disk = Arc::new(
            PersistentEntityCache::new(
                sqlx::SqlitePool::connect("sqlite::memory:").await.unwrap(),
                PersistentCacheConfig::default(),
            )
            .await
            .unwrap(),
        );
        let profiles = Arc::new(MockProfileFetcher::new());
        profiles.add_profile(test_profile("did:plc:present")).await;
        let posts = Arc::new(MockPostFetcher::new());
        let present = "at://did:plc:present/app.bsky.feed.post/one";
        let missing = "at://did:plc:missing/app.bsky.feed.post/one";
        posts.add_post(test_post(present)).await;
        let resolver = CacheMissResolver::new(TurboCache::new(10, 10), profiles, posts)
            .with_persistent_cache(Arc::clone(&disk));
        let profiles = resolver
            .resolve_profiles(&["did:plc:missing", "did:plc:present"])
            .await
            .unwrap();
        let posts = resolver.resolve_posts(&[missing, present]).await.unwrap();
        assert!(profiles[0].is_none() && profiles[1].is_some());
        assert!(matches!(posts[0], PostFetchOutcome::Missing));
        assert!(matches!(posts[1], PostFetchOutcome::Found(_)));
        assert!(disk.flush().await);
        assert_eq!(
            disk.read::<BlueskyProfile>(CacheKind::Profile, &["did:plc:present".into()])
                .await
                .len(),
            1
        );
        assert_eq!(
            disk.read::<BlueskyPost>(CacheKind::Post, &[present.into()])
                .await
                .len(),
            1
        );
    }

    #[tokio::test]
    async fn older_post_outcomes_cannot_replace_a_newer_found_or_missing_result() {
        use crate::hydration::entity_cache::{PersistentCacheConfig, PersistentEntityCache};
        use std::sync::atomic::{AtomicUsize, Ordering};
        use tokio::sync::Notify;
        struct OrderedPosts {
            calls: AtomicUsize,
            started: Notify,
            release: Notify,
            first: PostFetchOutcome,
            second: PostFetchOutcome,
        }
        impl PostFetcher for OrderedPosts {
            async fn bulk_fetch_posts(&self, _: &[String]) -> TurboResult<Vec<PostFetchOutcome>> {
                if self.calls.fetch_add(1, Ordering::SeqCst) == 0 {
                    self.started.notify_one();
                    self.release.notified().await;
                    Ok(vec![self.first.clone()])
                } else {
                    Ok(vec![self.second.clone()])
                }
            }
        }
        let uri = "at://did:plc:test/app.bsky.feed.post/ordered";
        let mut new = test_post(uri);
        new.text = "new".into();
        let found = PostFetchOutcome::Found(Arc::new(new));
        let unavailable = PostFetchOutcome::TemporarilyUnavailable(HydrationFailure {
            operation: BlueskyOperation::Posts,
            category: UpstreamFailureCategory::ServerError,
            status_class: Some("5xx".into()),
            attempts: 1,
            request_fingerprint: "safe".into(),
            isolation: None,
        });
        for (first, second) in [
            (PostFetchOutcome::Missing, found.clone()),
            (unavailable, found.clone()),
            (found.clone(), PostFetchOutcome::Missing),
        ] {
            let disk = Arc::new(
                PersistentEntityCache::new(
                    sqlx::SqlitePool::connect("sqlite::memory:").await.unwrap(),
                    PersistentCacheConfig::default(),
                )
                .await
                .unwrap(),
            );
            let posts = Arc::new(OrderedPosts {
                calls: AtomicUsize::new(0),
                started: Notify::new(),
                release: Notify::new(),
                first,
                second: second.clone(),
            });
            let resolver = CacheMissResolver::new(
                TurboCache::new(10, 10),
                Arc::new(MockProfileFetcher::new()),
                Arc::clone(&posts),
            )
            .with_persistent_cache(Arc::clone(&disk));
            let old_resolver = resolver.clone();
            let old = tokio::spawn(async move { old_resolver.resolve_post(uri).await.unwrap() });
            posts.started.notified().await;
            resolver.resolve_post(uri).await.unwrap();
            posts.release.notify_one();
            old.await.unwrap();
            assert!(resolver.cache().get_unavailable_post(uri).is_none());
            assert!(disk.flush().await);
            let persisted = disk
                .read::<BlueskyPost>(CacheKind::Post, &[uri.into()])
                .await;
            if matches!(second, PostFetchOutcome::Found(_)) {
                assert_eq!(resolver.cache().get_post(uri).unwrap().text, "new");
                assert_eq!(persisted[uri].value.text, "new");
            } else {
                assert!(resolver.cache().get_post(uri).is_none());
                assert!(persisted.is_empty());
            }
        }
    }

    #[tokio::test]
    async fn older_missing_profile_cannot_remove_a_newer_found_profile() {
        use crate::hydration::entity_cache::{PersistentCacheConfig, PersistentEntityCache};
        use std::sync::atomic::{AtomicUsize, Ordering};
        use tokio::sync::Notify;
        struct OrderedProfiles {
            calls: AtomicUsize,
            started: Notify,
            release: Notify,
        }
        impl ProfileFetcher for OrderedProfiles {
            async fn bulk_fetch_profiles(
                &self,
                dids: &[String],
            ) -> TurboResult<Vec<Option<Arc<BlueskyProfile>>>> {
                if self.calls.fetch_add(1, Ordering::SeqCst) == 0 {
                    self.started.notify_one();
                    self.release.notified().await;
                    Ok(vec![None])
                } else {
                    Ok(vec![Some(Arc::new(test_profile(&dids[0])))])
                }
            }
        }
        let disk = Arc::new(
            PersistentEntityCache::new(
                sqlx::SqlitePool::connect("sqlite::memory:").await.unwrap(),
                PersistentCacheConfig::default(),
            )
            .await
            .unwrap(),
        );
        let profiles = Arc::new(OrderedProfiles {
            calls: AtomicUsize::new(0),
            started: Notify::new(),
            release: Notify::new(),
        });
        let resolver = CacheMissResolver::new(
            TurboCache::new(10, 10),
            Arc::clone(&profiles),
            Arc::new(MockPostFetcher::new()),
        )
        .with_persistent_cache(Arc::clone(&disk));
        let old_resolver = resolver.clone();
        let old = tokio::spawn(async move {
            old_resolver
                .resolve_profile("did:plc:ordered")
                .await
                .unwrap()
        });
        profiles.started.notified().await;
        assert!(resolver
            .resolve_profile("did:plc:ordered")
            .await
            .unwrap()
            .is_some());
        profiles.release.notify_one();
        assert!(old.await.unwrap().is_none());
        assert!(resolver
            .cache()
            .get_user_profile("did:plc:ordered")
            .is_some());
        assert!(disk.flush().await);
        assert_eq!(
            disk.read::<BlueskyProfile>(CacheKind::Profile, &["did:plc:ordered".into()])
                .await
                .len(),
            1
        );
    }

    #[tokio::test]
    async fn unrelated_mutations_preserve_positive_and_negative_cache_entries() {
        use crate::hydration::entity_cache::{PersistentCacheConfig, PersistentEntityCache};
        let disk = Arc::new(
            PersistentEntityCache::new(
                sqlx::SqlitePool::connect("sqlite::memory:").await.unwrap(),
                PersistentCacheConfig::default(),
            )
            .await
            .unwrap(),
        );
        let profiles = Arc::new(MockProfileFetcher::new());
        profiles.add_profile(test_profile("did:plc:keep")).await;
        let posts = Arc::new(MockPostFetcher::new());
        let keep = "at://did:plc:keep/app.bsky.feed.post/one";
        posts.add_post(test_post(keep)).await;
        let resolver = CacheMissResolver::new(
            TurboCache::new(10, 10),
            Arc::clone(&profiles),
            Arc::clone(&posts),
        )
        .with_persistent_cache(Arc::clone(&disk));
        resolver.resolve_profile("did:plc:keep").await.unwrap();
        resolver.resolve_post(keep).await.unwrap();
        let unavailable = "at://did:plc:keep/app.bsky.feed.post/unavailable";
        resolver.cache().set_unavailable_post(
            unavailable.into(),
            HydrationFailure {
                operation: BlueskyOperation::Posts,
                category: UpstreamFailureCategory::ServerError,
                status_class: Some("5xx".into()),
                attempts: 1,
                request_fingerprint: "safe".into(),
                isolation: None,
            },
        );
        assert!(disk.flush().await);
        for event in [
            serde_json::json!({"did":"did:plc:other","kind":"commit","commit":{"operation":"delete","collection":"app.bsky.feed.post","rkey":"one"}}),
            serde_json::json!({"did":"did:plc:other","kind":"identity"}),
        ] {
            resolver.observe_message(&serde_json::from_value(event).unwrap());
        }
        assert!(resolver
            .resolve_profile("did:plc:keep")
            .await
            .unwrap()
            .is_some());
        assert!(matches!(
            resolver.resolve_post(keep).await.unwrap(),
            PostFetchOutcome::Found(_)
        ));
        assert!(matches!(
            resolver.resolve_post(unavailable).await.unwrap(),
            PostFetchOutcome::TemporarilyUnavailable(_)
        ));
        assert_eq!(
            profiles
                .call_count
                .load(std::sync::atomic::Ordering::SeqCst),
            1
        );
        assert_eq!(
            posts.call_count.load(std::sync::atomic::Ordering::SeqCst),
            1
        );
        assert!(disk.flush().await);
        assert_eq!(
            disk.read::<BlueskyProfile>(CacheKind::Profile, &["did:plc:keep".into()])
                .await
                .len(),
            1
        );
        assert_eq!(
            disk.read::<BlueskyPost>(CacheKind::Post, &[keep.into()])
                .await
                .len(),
            1
        );
        resolver.observe_message(
            &serde_json::from_value(serde_json::json!({"did":"did:plc:keep","kind":"identity"}))
                .unwrap(),
        );
        assert!(resolver.cache().get_user_profile("did:plc:keep").is_none());
        assert!(resolver.cache().get_post(keep).is_none());
        assert!(resolver.cache().get_unavailable_post(unavailable).is_none());
        assert!(disk.flush().await);
        assert!(disk
            .read::<BlueskyProfile>(CacheKind::Profile, &["did:plc:keep".into()])
            .await
            .is_empty());
        assert!(disk
            .read::<BlueskyPost>(CacheKind::Post, &[keep.into()])
            .await
            .is_empty());
    }

    #[tokio::test]
    async fn restart_reuses_disk_and_mixed_repeated_inputs_fetch_only_unique_misses() {
        use crate::hydration::entity_cache::{PersistentCacheConfig, PersistentEntityCache};
        use sqlx::sqlite::{SqliteConnectOptions, SqlitePoolOptions};
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("entities.db");
        let pool = SqlitePoolOptions::new()
            .max_connections(2)
            .connect_with(
                SqliteConnectOptions::new()
                    .filename(&path)
                    .create_if_missing(true),
            )
            .await
            .unwrap();
        let disk = Arc::new(
            PersistentEntityCache::new(pool.clone(), PersistentCacheConfig::default())
                .await
                .unwrap(),
        );
        let profiles = Arc::new(MockProfileFetcher::new());
        profiles.add_profile(test_profile("did:plc:alice")).await;
        let posts = Arc::new(MockPostFetcher::new());
        let uri = "at://did:plc:alice/app.bsky.feed.post/one";
        posts.add_post(test_post(uri)).await;
        let resolver = CacheMissResolver::new(
            TurboCache::new(2, 2),
            Arc::clone(&profiles),
            Arc::clone(&posts),
        )
        .with_persistent_cache(Arc::clone(&disk));
        let result = resolver
            .resolve_profiles(&["did:plc:alice", "did:plc:alice"])
            .await
            .unwrap();
        assert_eq!(result.len(), 2);
        assert!(result.iter().all(Option::is_some));
        assert_eq!(
            profiles.requested_dids.lock().await[0],
            vec!["did:plc:alice"]
        );
        assert!(matches!(
            resolver.resolve_post(uri).await.unwrap(),
            PostFetchOutcome::Found(_)
        ));
        assert!(disk.flush().await);
        drop(resolver);
        drop(disk);
        pool.close().await;

        let pool = SqlitePoolOptions::new()
            .max_connections(2)
            .connect_with(SqliteConnectOptions::new().filename(&path))
            .await
            .unwrap();
        let disk = Arc::new(
            PersistentEntityCache::new(pool, PersistentCacheConfig::default())
                .await
                .unwrap(),
        );
        let profiles = Arc::new(MockProfileFetcher::new());
        profiles.add_profile(test_profile("did:plc:bob")).await;
        let posts = Arc::new(MockPostFetcher::new());
        let cache = TurboCache::new(4, 4);
        cache.set_user_profile("did:plc:ram".into(), Arc::new(test_profile("did:plc:ram")));
        let resolver = CacheMissResolver::new(cache, Arc::clone(&profiles), Arc::clone(&posts))
            .with_persistent_cache(Arc::clone(&disk));
        let requested = [
            "did:plc:bob",
            "did:plc:alice",
            "did:plc:ram",
            "did:plc:bob",
            "did:plc:alice",
        ];
        let result = resolver.resolve_profiles(&requested).await.unwrap();
        assert_eq!(
            result
                .iter()
                .map(|p| p.as_ref().unwrap().did.as_ref())
                .collect::<Vec<_>>(),
            requested
        );
        assert_eq!(
            profiles.requested_dids.lock().await.as_slice(),
            &[vec!["did:plc:bob".to_string()]]
        );
        assert!(matches!(
            resolver.resolve_post(uri).await.unwrap(),
            PostFetchOutcome::Found(_)
        ));
        assert_eq!(
            posts.call_count.load(std::sync::atomic::Ordering::SeqCst),
            0
        );
        assert_eq!(disk.diagnostics().profile_disk_hits, 1);
        assert_eq!(disk.diagnostics().post_disk_hits, 1);
    }

    #[tokio::test]
    async fn disk_failure_falls_through_to_existing_fetchers() {
        use crate::hydration::entity_cache::{PersistentCacheConfig, PersistentEntityCache};
        let pool = sqlx::SqlitePool::connect("sqlite::memory:").await.unwrap();
        let disk = Arc::new(
            PersistentEntityCache::new(pool.clone(), PersistentCacheConfig::default())
                .await
                .unwrap(),
        );
        pool.close().await;
        let profiles = Arc::new(MockProfileFetcher::new());
        profiles.add_profile(test_profile("did:plc:alice")).await;
        let resolver = CacheMissResolver::new(
            TurboCache::new(2, 2),
            Arc::clone(&profiles),
            Arc::new(MockPostFetcher::new()),
        )
        .with_persistent_cache(Arc::clone(&disk));
        assert!(resolver
            .resolve_profile("did:plc:alice")
            .await
            .unwrap()
            .is_some());
        assert_eq!(
            profiles
                .call_count
                .load(std::sync::atomic::Ordering::SeqCst),
            1
        );
        assert!(disk.diagnostics().persistence_errors > 0);
    }

    #[tokio::test]
    async fn mixed_valid_expired_and_malformed_disk_rows_fetch_only_unresolved_entities() {
        use crate::hydration::entity_cache::{PersistentCacheConfig, PersistentEntityCache};
        let pool = sqlx::SqlitePool::connect("sqlite::memory:").await.unwrap();
        let disk = Arc::new(
            PersistentEntityCache::new(pool.clone(), PersistentCacheConfig::default())
                .await
                .unwrap(),
        );
        disk.store(
            CacheKind::Profile,
            "did:plc:fresh".into(),
            &test_profile("did:plc:fresh"),
            disk.now_ms(),
            disk.generation(),
            disk.next_fetch_order(),
        );
        assert!(disk.flush().await);
        for (key, payload, expiry) in [
            (
                "did:plc:expired",
                serde_json::to_string(&test_profile("did:plc:expired")).unwrap(),
                disk.now_ms() - 1,
            ),
            ("did:plc:malformed", "{".into(), disk.now_ms() + 300_000),
        ] {
            sqlx::query("INSERT INTO profile_cache VALUES(?,?,1,?,?,100,?,0)")
                .bind(key)
                .bind(payload)
                .bind(disk.now_ms() - 1000)
                .bind(expiry)
                .bind(disk.now_ms())
                .execute(&pool)
                .await
                .unwrap();
        }
        let profiles = Arc::new(MockProfileFetcher::new());
        profiles.add_profile(test_profile("did:plc:expired")).await;
        profiles
            .add_profile(test_profile("did:plc:malformed"))
            .await;
        let resolver = CacheMissResolver::new(
            TurboCache::new(4, 4),
            Arc::clone(&profiles),
            Arc::new(MockPostFetcher::new()),
        )
        .with_persistent_cache(Arc::clone(&disk));
        let requested = [
            "did:plc:fresh",
            "did:plc:expired",
            "did:plc:malformed",
            "did:plc:fresh",
        ];
        let result = resolver.resolve_profiles(&requested).await.unwrap();
        assert_eq!(
            result
                .iter()
                .map(|value| value.as_ref().unwrap().did.as_ref())
                .collect::<Vec<_>>(),
            requested
        );
        assert_eq!(
            profiles.requested_dids.lock().await.as_slice(),
            &[vec![
                "did:plc:expired".to_string(),
                "did:plc:malformed".to_string()
            ]]
        );
        assert_eq!(disk.diagnostics().profile_disk_hits, 1);
        assert!(disk.flush().await);
        assert_eq!(
            disk.read::<BlueskyProfile>(
                CacheKind::Profile,
                &["did:plc:expired".into(), "did:plc:malformed".into()]
            )
            .await
            .len(),
            2
        );
    }

    #[tokio::test]
    async fn observed_delete_fences_an_in_flight_fetch_in_both_tiers() {
        use crate::hydration::entity_cache::{PersistentCacheConfig, PersistentEntityCache};
        use tokio::sync::Notify;
        struct DelayedPost {
            started: Notify,
            release: Notify,
        }
        impl PostFetcher for DelayedPost {
            async fn bulk_fetch_posts(
                &self,
                uris: &[String],
            ) -> TurboResult<Vec<PostFetchOutcome>> {
                self.started.notify_one();
                self.release.notified().await;
                Ok(uris
                    .iter()
                    .map(|uri| PostFetchOutcome::Found(Arc::new(test_post(uri))))
                    .collect())
            }
        }
        let disk = Arc::new(
            PersistentEntityCache::new(
                sqlx::SqlitePool::connect("sqlite::memory:").await.unwrap(),
                PersistentCacheConfig::default(),
            )
            .await
            .unwrap(),
        );
        let posts = Arc::new(DelayedPost {
            started: Notify::new(),
            release: Notify::new(),
        });
        let resolver = CacheMissResolver::new(
            TurboCache::new(2, 2),
            Arc::new(MockProfileFetcher::new()),
            Arc::clone(&posts),
        )
        .with_persistent_cache(Arc::clone(&disk));
        let uri = "at://did:plc:test/app.bsky.feed.post/one";
        let task_resolver = resolver.clone();
        let fetching = tokio::spawn(async move { task_resolver.resolve_post(uri).await.unwrap() });
        posts.started.notified().await;
        let message:JetstreamMessage=serde_json::from_value(serde_json::json!({"did":"did:plc:test","kind":"commit","commit":{"operation":"delete","collection":"app.bsky.feed.post","rkey":"one"}})).unwrap();
        resolver.observe_message(&message);
        posts.release.notify_one();
        assert!(matches!(
            fetching.await.unwrap(),
            PostFetchOutcome::Found(_)
        ));
        assert!(resolver.cache().get_post(uri).is_none());
        assert!(disk.flush().await);
        assert!(disk
            .read::<BlueskyPost>(CacheKind::Post, &[uri.into()])
            .await
            .is_empty());
    }

    #[tokio::test]
    async fn comparable_replay_measures_requests_and_throughput_without_extending_ttl() {
        use crate::hydration::entity_cache::{PersistentCacheConfig, PersistentEntityCache};
        use std::sync::atomic::{AtomicU64, Ordering};
        struct RepeatProfiles {
            calls: AtomicU64,
        }
        impl ProfileFetcher for RepeatProfiles {
            async fn bulk_fetch_profiles(
                &self,
                dids: &[String],
            ) -> TurboResult<Vec<Option<Arc<BlueskyProfile>>>> {
                self.calls.fetch_add(1, Ordering::Relaxed);
                tokio::time::sleep(Duration::from_millis(5)).await;
                Ok(dids
                    .iter()
                    .map(|did| Some(Arc::new(test_profile(did))))
                    .collect())
            }
        }
        let dids = (0..20)
            .map(|i| format!("did:plc:person{i}"))
            .collect::<Vec<_>>();
        let api = Arc::new(RepeatProfiles {
            calls: AtomicU64::new(0),
        });
        let posts = Arc::new(MockPostFetcher::new());
        let started = Instant::now();
        for _ in 0..20 {
            let resolver =
                CacheMissResolver::new(TurboCache::new(1, 1), Arc::clone(&api), Arc::clone(&posts));
            assert!(resolver
                .resolve_profiles(&dids)
                .await
                .unwrap()
                .iter()
                .all(Option::is_some));
        }
        let baseline_time = started.elapsed();
        let baseline_calls = api.calls.swap(0, Ordering::Relaxed);
        let disk = Arc::new(
            PersistentEntityCache::new(
                sqlx::SqlitePool::connect("sqlite::memory:").await.unwrap(),
                PersistentCacheConfig::default(),
            )
            .await
            .unwrap(),
        );
        CacheMissResolver::new(TurboCache::new(1, 1), Arc::clone(&api), Arc::clone(&posts))
            .with_persistent_cache(Arc::clone(&disk))
            .resolve_profiles(&dids)
            .await
            .unwrap();
        assert!(disk.flush().await);
        let warmup_calls = api.calls.swap(0, Ordering::Relaxed);
        let started = Instant::now();
        for _ in 0..20 {
            let resolver =
                CacheMissResolver::new(TurboCache::new(1, 1), Arc::clone(&api), Arc::clone(&posts))
                    .with_persistent_cache(Arc::clone(&disk));
            assert!(resolver
                .resolve_profiles(&dids)
                .await
                .unwrap()
                .iter()
                .all(Option::is_some));
        }
        let cached_time = started.elapsed();
        let cached_calls = api.calls.load(Ordering::Relaxed);
        println!("synthetic replay: 400 profiles / 20 fresh-RAM batches; baseline API batches={baseline_calls}, disk API batches={cached_calls}, warmup={warmup_calls}; baseline_ms={}, disk_ms={}, baseline_entities_per_second={:.0}, disk_entities_per_second={:.0}; TTL=300s",baseline_time.as_millis(),cached_time.as_millis(),400.0/baseline_time.as_secs_f64(),400.0/cached_time.as_secs_f64());
        assert_eq!(baseline_calls, 20);
        assert_eq!(warmup_calls, 1);
        assert_eq!(cached_calls, 0);
        assert_eq!(disk.diagnostics().profile_disk_hits, 400);
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
