use crate::client::{PostFetcher, ProfileFetcher};
use crate::hydration::TurboCache;
use crate::models::TurboResult;
use std::sync::Arc;
use std::time::Duration;
use tracing::{info, trace};

pub struct DataFetcher<P, Po> {
    cache: TurboCache,
    profile_fetcher: Arc<P>,
    post_fetcher: Arc<Po>,
    #[allow(dead_code)]
    request_timeout: Duration,
}

impl<P, Po> DataFetcher<P, Po>
where
    P: ProfileFetcher + Send + Sync + 'static,
    Po: PostFetcher + Send + Sync + 'static,
{
    pub fn new(cache: TurboCache, profile_fetcher: Arc<P>, post_fetcher: Arc<Po>) -> Self {
        Self {
            cache,
            profile_fetcher,
            post_fetcher,
            request_timeout: Duration::from_secs(30),
        }
    }

    pub async fn fetch_missing_profiles(&self, dids: &[String]) -> TurboResult<usize> {
        if dids.is_empty() {
            return Ok(0);
        }

        // Check which profiles are missing from cache
        let cached_flags = self.cache.check_user_profiles_cached(dids);
        let missing_dids: Vec<String> = dids
            .iter()
            .zip(cached_flags)
            .filter_map(|(did, is_cached)| if !is_cached { Some(did.clone()) } else { None })
            .collect();

        if missing_dids.is_empty() {
            return Ok(0);
        }

        info!("Fetching {} missing profiles from API", missing_dids.len());

        // Fetch missing profiles in batches
        let mut fetched_count = 0;
        for chunk in missing_dids.chunks(25) {
            let profiles = self.profile_fetcher.bulk_fetch_profiles(chunk).await?;

            for (did, maybe_profile) in chunk.iter().zip(profiles) {
                if let Some(profile) = maybe_profile {
                    self.cache
                        .set_user_profile(did.clone(), Arc::new(profile));
                    fetched_count += 1;
                }
            }
        }

        trace!("Fetched {} profiles from API", fetched_count);
        Ok(fetched_count)
    }

    pub async fn fetch_missing_posts(&self, uris: &[String]) -> TurboResult<usize> {
        if uris.is_empty() {
            return Ok(0);
        }

        // Check which posts are missing from cache
        let cached_flags = self.cache.check_posts_cached(uris);
        let missing_uris: Vec<String> = uris
            .iter()
            .zip(cached_flags)
            .filter_map(|(uri, is_cached)| if !is_cached { Some(uri.clone()) } else { None })
            .collect();

        if missing_uris.is_empty() {
            return Ok(0);
        }

        info!("Fetching {} missing posts from API", missing_uris.len());

        // Fetch missing posts
        let mut fetched_count = 0;
        for chunk in missing_uris.chunks(10) {
            let posts = self.post_fetcher.bulk_fetch_posts(chunk).await?;

            for (uri, maybe_post) in chunk.iter().zip(posts) {
                if let Some(post) = maybe_post {
                    self.cache.set_post(uri.clone(), Arc::new(post));
                    fetched_count += 1;
                }
            }
        }

        trace!("Fetched {} posts from API", fetched_count);
        Ok(fetched_count)
    }

    pub async fn prefetch_related_data(
        &self,
        mentions: &[String],
        referenced_posts: &[String],
    ) -> TurboResult<(usize, usize)> {
        let profiles_fetched = self.fetch_missing_profiles(mentions).await?;
        let posts_fetched = self.fetch_missing_posts(referenced_posts).await?;

        Ok((profiles_fetched, posts_fetched))
    }

    pub fn get_cache(&self) -> &TurboCache {
        &self.cache
    }
}
