use crate::models::{
    bluesky::{BlueskyPost, BlueskyProfile, GetPostsBulkResponse, GetProfilesResponse},
    errors::{TurboError, TurboResult},
};
use crate::utils::serde_utils::string_utils::is_valid_at_uri;
use governor::{Quota, RateLimiter};
use reqwest::{Client, StatusCode};
use std::num::NonZeroU32;
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::RwLock;
use tracing::{error, info, trace, warn};

const REQUESTS_PER_SECOND: u32 = 15;

pub struct BlueskyClient {
    http_client: Client,
    session_strings: Arc<RwLock<Vec<String>>>,
    rate_limiter: Arc<
        RateLimiter<
            governor::state::NotKeyed,
            governor::state::InMemoryState,
            governor::clock::DefaultClock,
        >,
    >,
    api_base_url: String,
    max_retries: u32,
    #[allow(dead_code)]
    retry_delay_ms: u64,
    retry_delay: Duration,
}

impl BlueskyClient {
    pub fn new(session_strings: Vec<String>) -> Self {
        let quota = Quota::per_second(NonZeroU32::new(REQUESTS_PER_SECOND).unwrap());
        Self {
            http_client: Client::builder()
                .timeout(Duration::from_secs(30))
                .user_agent("jetstream-turbo/0.1.0")
                .build()
                .expect("Failed to create HTTP client"),
            session_strings: Arc::new(RwLock::new(session_strings)),
            rate_limiter: Arc::new(RateLimiter::direct(quota)), // ~6 requests per second
            api_base_url: "https://bsky.social/xrpc".to_string(),
            max_retries: 3,
            retry_delay_ms: 200,
            retry_delay: Duration::from_millis(200),
        }
    }

    pub async fn bulk_fetch_profiles(
        &self,
        dids: &[String],
    ) -> TurboResult<Vec<Option<BlueskyProfile>>> {
        let mut profiles: Vec<Option<BlueskyProfile>> = Vec::with_capacity(dids.len());

        // Process in chunks of 25 (Bluesky API limit)
        for chunk in dids.chunks(25) {
            let chunk_profiles = self.fetch_profiles_batch(chunk).await?;
            profiles.extend(chunk_profiles);
        }

        Ok(profiles)
    }

    async fn fetch_profiles_batch(
        &self,
        dids: &[String],
    ) -> TurboResult<Vec<Option<BlueskyProfile>>> {
        let url = format!("{}/app.bsky.actor.getProfiles", self.api_base_url);

        let session_string = self.get_session_string().await?;

        let mut attempt = 0;
        loop {
            // Rate limit check
            self.rate_limiter.until_ready().await;

            // Build query parameters for actors
            let mut query_params: Vec<(&str, &str)> = Vec::new();
            for did in dids {
                query_params.push(("actors", did));
            }

            let response = self
                .http_client
                .get(&url)
                .header("Authorization", format!("Bearer {session_string}"))
                .query(&query_params)
                .send()
                .await;

            match response {
                Ok(resp) => match resp.status() {
                    StatusCode::OK => {
                        let body = resp.text().await?;
                        trace!("Profiles response: {}", &body[..body.len().min(500)]);
                        let profiles_response: GetProfilesResponse = serde_json::from_str(&body)
                            .map_err(|e| {
                                error!(
                                    "Failed to parse profiles: {} - body: {}",
                                    e,
                                    &body[..body.len().min(500)]
                                );
                                TurboError::InvalidApiResponse(format!("Failed to decode: {}", e))
                            })?;
                        let mut result = vec![None; dids.len()];

                        for (i, profile) in profiles_response.profiles.into_iter().enumerate() {
                            if i < result.len() {
                                result[i] = Some(profile.into());
                            }
                        }

                        return Ok(result);
                    }
                    StatusCode::TOO_MANY_REQUESTS => {
                        warn!("Rate limited, waiting before retry");
                        tokio::time::sleep(self.retry_delay * 2).await;
                    }
                    StatusCode::UNAUTHORIZED => {
                        error!("Unauthorized - session may be invalid: {}", session_string);
                        return Err(TurboError::PermissionDenied(
                            "Invalid session token".to_string(),
                        ));
                    }
                    status => {
                        let error_text = resp.text().await.unwrap_or_default();
                        error!("API error {}: {}", status, error_text);
                        return Err(TurboError::InvalidApiResponse(format!(
                            "Status {status}: {error_text}"
                        )));
                    }
                },
                Err(e) => {
                    error!("HTTP request failed: {}", e);
                    if attempt >= self.max_retries {
                        return Err(TurboError::HttpRequest(e));
                    }
                }
            }

            attempt += 1;
            if attempt <= self.max_retries {
                tokio::time::sleep(self.retry_delay * attempt).await;
            }
        }
    }

    pub async fn bulk_fetch_posts(&self, uris: &[String]) -> TurboResult<Vec<Option<BlueskyPost>>> {
        if uris.is_empty() {
            return Ok(vec![]);
        }

        let valid_uris: Vec<String> = uris
            .iter()
            .filter(|uri| !uri.is_empty() && is_valid_at_uri(uri))
            .cloned()
            .collect();

        let filtered_count = uris.len() - valid_uris.len();
        if filtered_count > 0 {
            warn!(
                "Filtered {} invalid URIs out of {}",
                filtered_count,
                uris.len()
            );
            trace!(
                "Invalid URIs: {:?}",
                uris.iter()
                    .filter(|u| !is_valid_at_uri(u))
                    .collect::<Vec<_>>()
            );
        }

        if valid_uris.is_empty() {
            return Ok(vec![]);
        }

        let mut all_posts: Vec<Option<BlueskyPost>> = Vec::with_capacity(valid_uris.len());

        for chunk in valid_uris.chunks(25) {
            let chunk_posts = self.fetch_posts_bulk(chunk).await?;
            all_posts.extend(chunk_posts);
        }

        Ok(all_posts)
    }

    async fn fetch_posts_bulk(&self, uris: &[String]) -> TurboResult<Vec<Option<BlueskyPost>>> {
        let url = format!("{}/app.bsky.feed.getPosts", self.api_base_url);

        let session_string = self.get_session_string().await?;

        let mut attempt = 0;
        loop {
            self.rate_limiter.until_ready().await;

            // Build query parameters for URIs (separate params, not comma-joined)
            let mut query_params: Vec<(&str, &str)> = Vec::new();
            for uri in uris {
                query_params.push(("uris", uri));
            }

            let response = self
                .http_client
                .get(&url)
                .header("Authorization", format!("Bearer {session_string}"))
                .query(&query_params)
                .send()
                .await;

            trace!("Fetching posts for URIs: {:?}", uris);

            match response {
                Ok(resp) => match resp.status() {
                    StatusCode::OK => {
                        let body = resp.text().await?;
                        trace!("Posts response: {}", &body[..body.len().min(500)]);
                        let posts_response: GetPostsBulkResponse = serde_json::from_str(&body)
                            .map_err(|e| {
                                error!(
                                    "Failed to parse posts: {} - body: {}",
                                    e,
                                    &body[..body.len().min(500)]
                                );
                                TurboError::InvalidApiResponse(format!("Failed to decode: {}", e))
                            })?;

                        let mut results = vec![None; uris.len()];
                        for post_response in posts_response.posts {
                            if let Some(uri) = uris.iter().position(|u| u == &post_response.uri) {
                                results[uri] = Some(self.convert_bulk_post_response(post_response));
                            }
                        }

                        return Ok(results);
                    }
                    StatusCode::TOO_MANY_REQUESTS => {
                        warn!("Rate limited, waiting before retry");
                        tokio::time::sleep(self.retry_delay * 2).await;
                    }
                    StatusCode::UNAUTHORIZED => {
                        error!("Unauthorized - session may be invalid: {}", session_string);
                        return Err(TurboError::PermissionDenied(
                            "Invalid session token".to_string(),
                        ));
                    }
                    status => {
                        let error_text = resp.text().await.unwrap_or_default();
                        error!("API error {}: {}", status, error_text);
                        return Err(TurboError::InvalidApiResponse(format!(
                            "Status {status}: {error_text}"
                        )));
                    }
                },
                Err(e) => {
                    error!("HTTP request failed: {}", e);
                    if attempt >= self.max_retries {
                        return Err(TurboError::HttpRequest(e));
                    }
                }
            }

            attempt += 1;
            if attempt <= self.max_retries {
                tokio::time::sleep(self.retry_delay * attempt).await;
            }
        }
    }

    fn convert_bulk_post_response(
        &self,
        response: crate::models::bluesky::GetPostsResponse,
    ) -> BlueskyPost {
        BlueskyPost {
            uri: response.uri,
            cid: response.cid,
            author: response.author.into(),
            text: response
                .record
                .get("text")
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .to_string(),
            created_at: chrono::Utc::now(),
            embed: response.embed.and_then(|e| serde_json::from_value(e).ok()),
            reply: response.reply.and_then(|r| serde_json::from_value(r).ok()),
            facets: response
                .record
                .get("facets")
                .and_then(|v| serde_json::from_value(v.clone()).ok()),
            labels: response.labels,
            like_count: response.like_count,
            repost_count: response.repost_count,
            reply_count: response.reply_count,
        }
    }

    async fn get_session_string(&self) -> TurboResult<String> {
        let sessions = self.session_strings.read().await;

        if sessions.is_empty() {
            return Err(TurboError::PermissionDenied(
                "No valid session strings available".to_string(),
            ));
        }

        Ok(sessions[0].clone())
    }

    pub async fn refresh_sessions(&self, new_sessions: Vec<String>) {
        let mut sessions = self.session_strings.write().await;
        *sessions = new_sessions;
        info!("Refreshed {} session strings", sessions.len());
    }

    pub async fn get_session_count(&self) -> usize {
        self.session_strings.read().await.len()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn test_bluesky_client_creation() {
        let sessions = vec!["session1:::bsky.social".to_string()];
        let client = BlueskyClient::new(sessions);
        assert_eq!(client.get_session_count().await, 1);
    }

    #[tokio::test]
    async fn test_refresh_sessions() {
        let client = BlueskyClient::new(vec!["old_session".to_string()]);
        assert_eq!(client.get_session_count().await, 1);

        client
            .refresh_sessions(vec![
                "new_session1:::bsky.social".to_string(),
                "new_session2:::bsky.social".to_string(),
            ])
            .await;

        assert_eq!(client.get_session_count().await, 2);
    }
}
