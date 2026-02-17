use crate::client::BlueskyAuthClient;
use crate::models::{
    bluesky::{BlueskyPost, BlueskyProfile, GetPostsBulkResponse, GetProfilesResponse},
    errors::{TurboError, TurboResult},
};
use crate::utils::serde_utils::string_utils::is_valid_at_uri;
use governor::{Quota, RateLimiter};
use reqwest::{Client, StatusCode};
use std::num::NonZeroU32;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::RwLock;
use tracing::{error, info, instrument, trace, warn};

const REQUESTS_PER_SECOND_MS: u64 = 1000 / 10;
const BATCH_SIZE: usize = 25;

async fn handle_rate_limit_response(
    response: &reqwest::Response,
    attempt: u32,
    retry_delay: Duration,
) -> Option<Duration> {
    if let Some(retry_after) = response.headers().get("retry-after") {
        if let Ok(value) = retry_after.to_str() {
            if let Ok(seconds) = value.parse::<u64>() {
                trace!(
                    "Rate limited: Retry-After header suggests {} seconds",
                    seconds
                );
                return Some(Duration::from_secs(seconds));
            }
        }
    }

    let backoff_ms = retry_delay.as_millis() as u64 * (2u64.pow(attempt.min(5)));
    Some(Duration::from_millis(backoff_ms))
}

pub struct BlueskyClient {
    http_client: Client,
    session_strings: Arc<RwLock<Vec<String>>>,
    refresh_jwt: Arc<RwLock<Option<String>>>,
    expires_at: Arc<RwLock<Option<String>>>,
    auth_client: Option<Arc<BlueskyAuthClient>>,
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
    profile_batches_total: AtomicU64,
    profile_batches_partial: AtomicU64,
    post_batches_total: AtomicU64,
    post_batches_partial: AtomicU64,
}

impl BlueskyClient {
    pub fn new(session_strings: Vec<String>, auth_client: Option<Arc<BlueskyAuthClient>>) -> Self {
        let quota = Quota::with_period(Duration::from_millis(REQUESTS_PER_SECOND_MS))
            .expect("Valid quota")
            .allow_burst(NonZeroU32::new(1).unwrap());

        // Configure HTTP client with connection pooling for optimal performance
        let http_client = Client::builder()
            .timeout(Duration::from_secs(30))
            .connect_timeout(Duration::from_secs(10))
            .user_agent("jetstream-turbo/0.1.0")
            // Connection pool settings
            .pool_max_idle_per_host(10)
            .pool_idle_timeout(Duration::from_secs(30))
            .tcp_keepalive(Duration::from_secs(60))
            .tcp_nodelay(true)
            .build()
            .expect("Failed to create HTTP client");

        Self {
            http_client,
            session_strings: Arc::new(RwLock::new(session_strings)),
            refresh_jwt: Arc::new(RwLock::new(None)),
            expires_at: Arc::new(RwLock::new(None)),
            auth_client,
            rate_limiter: Arc::new(RateLimiter::direct(quota)),
            api_base_url: "https://bsky.social/xrpc".to_string(),
            max_retries: 3,
            retry_delay_ms: 200,
            retry_delay: Duration::from_millis(200),
            profile_batches_total: AtomicU64::new(0),
            profile_batches_partial: AtomicU64::new(0),
            post_batches_total: AtomicU64::new(0),
            post_batches_partial: AtomicU64::new(0),
        }
    }

    #[instrument(name = "bulk_fetch_profiles", skip(self, dids), fields(count, chunks))]
    pub async fn bulk_fetch_profiles(
        &self,
        dids: &[String],
    ) -> TurboResult<Vec<Option<BlueskyProfile>>> {
        tracing::Span::current().record("count", dids.len());

        let mut profiles: Vec<Option<BlueskyProfile>> = Vec::with_capacity(dids.len());
        let chunks = dids.chunks(BATCH_SIZE).count();
        tracing::Span::current().record("chunks", chunks);

        // Process in chunks of BATCH_SIZE
        for chunk in dids.chunks(BATCH_SIZE) {
            self.profile_batches_total.fetch_add(1, Ordering::Relaxed);
            if chunk.len() < BATCH_SIZE {
                self.profile_batches_partial.fetch_add(1, Ordering::Relaxed);
            }
            let chunk_profiles = self.fetch_profiles_batch(chunk).await?;
            profiles.extend(chunk_profiles);
        }

        self.log_partial_percentage("profiles");

        Ok(profiles)
    }

    async fn fetch_profiles_batch(
        &self,
        dids: &[String],
    ) -> TurboResult<Vec<Option<BlueskyProfile>>> {
        let url = format!("{}/app.bsky.actor.getProfiles", self.api_base_url);

        let mut session_string = self.get_session_string().await?;

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
                        warn!("Rate limited (profiles), waiting before retry");
                        if let Some(wait_time) =
                            handle_rate_limit_response(&resp, attempt, self.retry_delay).await
                        {
                            tokio::time::sleep(wait_time).await;
                            continue;
                        }
                        tokio::time::sleep(self.retry_delay * 2).await;
                    }
                    StatusCode::UNAUTHORIZED => {
                        error!("Unauthorized - session may be invalid, attempting refresh");
                        if let Err(e) = self.refresh_session_with_fallback().await {
                            return Err(TurboError::ExpiredToken(format!(
                                "Session refresh failed: {}",
                                e
                            )));
                        }
                        session_string = self.get_session_string().await?;
                        if attempt < self.max_retries {
                            attempt += 1;
                            continue;
                        }
                        return Err(TurboError::PermissionDenied(
                            "Invalid session token".to_string(),
                        ));
                    }
                    StatusCode::BAD_REQUEST => {
                        let error_text = resp.text().await.unwrap_or_default();
                        if let Some(new_session) =
                            self.handle_auth_error_and_refresh(&error_text).await?
                        {
                            session_string = new_session;
                            if attempt < self.max_retries {
                                attempt += 1;
                                continue;
                            }
                        }
                        error!("API error 400: {}", error_text);
                        return Err(TurboError::InvalidApiResponse(format!(
                            "Status 400: {error_text}"
                        )));
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

    #[instrument(
        name = "bulk_fetch_posts",
        skip(self, uris),
        fields(count, valid_count, chunks)
    )]
    pub async fn bulk_fetch_posts(&self, uris: &[String]) -> TurboResult<Vec<Option<BlueskyPost>>> {
        if uris.is_empty() {
            return Ok(vec![]);
        }

        let count = uris.len();
        tracing::Span::current().record("count", count);

        let valid_uris: Vec<String> = uris
            .iter()
            .filter(|uri| !uri.is_empty() && is_valid_at_uri(uri))
            .cloned()
            .collect();

        let valid_count = valid_uris.len();
        tracing::Span::current().record("valid_count", valid_count);

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

        let chunks = valid_uris.chunks(BATCH_SIZE).count();
        tracing::Span::current().record("chunks", chunks);

        for chunk in valid_uris.chunks(BATCH_SIZE) {
            self.post_batches_total.fetch_add(1, Ordering::Relaxed);
            if chunk.len() < BATCH_SIZE {
                self.post_batches_partial.fetch_add(1, Ordering::Relaxed);
            }
            let chunk_posts = self.fetch_posts_bulk(chunk).await?;
            all_posts.extend(chunk_posts);
        }

        self.log_partial_percentage("posts");

        Ok(all_posts)
    }

    async fn fetch_posts_bulk(&self, uris: &[String]) -> TurboResult<Vec<Option<BlueskyPost>>> {
        let url = format!("{}/app.bsky.feed.getPosts", self.api_base_url);

        let mut session_string = self.get_session_string().await?;

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
                        warn!("Rate limited (posts), waiting before retry");
                        if let Some(wait_time) =
                            handle_rate_limit_response(&resp, attempt, self.retry_delay).await
                        {
                            tokio::time::sleep(wait_time).await;
                            continue;
                        }
                        tokio::time::sleep(self.retry_delay * 2).await;
                    }
                    StatusCode::UNAUTHORIZED => {
                        error!("Unauthorized - session may be invalid, attempting refresh");
                        if let Err(e) = self.refresh_session_with_fallback().await {
                            return Err(TurboError::ExpiredToken(format!(
                                "Session refresh failed: {}",
                                e
                            )));
                        }
                        session_string = self.get_session_string().await?;
                        if attempt < self.max_retries {
                            attempt += 1;
                            continue;
                        }
                        return Err(TurboError::PermissionDenied(
                            "Invalid session token".to_string(),
                        ));
                    }
                    StatusCode::BAD_REQUEST => {
                        let error_text = resp.text().await.unwrap_or_default();
                        if let Some(new_session) =
                            self.handle_auth_error_and_refresh(&error_text).await?
                        {
                            session_string = new_session;
                            if attempt < self.max_retries {
                                attempt += 1;
                                continue;
                            }
                        }
                        error!("API error 400: {}", error_text);
                        return Err(TurboError::InvalidApiResponse(format!(
                            "Status 400: {error_text}"
                        )));
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

    pub async fn refresh_sessions(
        &self,
        new_sessions: Vec<String>,
        new_refresh_jwt: Option<String>,
        new_expires_at: Option<String>,
    ) {
        let mut sessions = self.session_strings.write().await;
        *sessions = new_sessions;
        info!("Refreshed {} session strings", sessions.len());

        if let Some(refresh_jwt) = new_refresh_jwt {
            let mut jwt = self.refresh_jwt.write().await;
            *jwt = Some(refresh_jwt);
        }

        if let Some(expires_at) = new_expires_at {
            let mut exp = self.expires_at.write().await;
            *exp = Some(expires_at.clone());
            info!("Session expires at: {}", expires_at);
        }
    }

    pub async fn should_refresh(&self) -> bool {
        let expires_at = self.expires_at.read().await;
        if let Some(ref exp) = *expires_at {
            if let Ok(exp_time) = chrono::DateTime::parse_from_rfc3339(exp) {
                let now = chrono::Utc::now();
                let duration_until_expiry = exp_time.signed_duration_since(now);
                return duration_until_expiry.num_seconds() < 3600;
            }
        }
        true
    }

    pub async fn get_refresh_jwt(&self) -> Option<String> {
        self.refresh_jwt.read().await.clone()
    }

    async fn handle_auth_error_and_refresh(
        &self,
        error_response: &str,
    ) -> TurboResult<Option<String>> {
        let is_expired = error_response.contains("ExpiredToken");

        if is_expired {
            error!("Token expired, full error: {}", error_response);
            self.refresh_session_with_fallback().await?;
            return Ok(Some(self.get_session_string().await?));
        }
        Ok(None)
    }

    pub async fn refresh_session_with_fallback(&self) -> TurboResult<()> {
        if let Some(ref auth_client) = self.auth_client {
            if let Some(refresh_jwt) = self.get_refresh_jwt().await {
                match auth_client.refresh_session(&refresh_jwt).await {
                    Ok(auth_response) => {
                        self.refresh_sessions(
                            vec![auth_response.access_jwt],
                            Some(auth_response.refresh_jwt),
                            auth_response.expires_at,
                        )
                        .await;
                        info!("Session refreshed successfully");
                        return Ok(());
                    }
                    Err(TurboError::ExpiredToken(_)) => {
                        warn!("Refresh token expired, re-authenticating with credentials");
                    }
                    Err(e) => {
                        error!("Session refresh failed: {}", e);
                        return Err(e);
                    }
                }
            }

            match auth_client.authenticate().await {
                Ok(auth_response) => {
                    self.refresh_sessions(
                        vec![auth_response.access_jwt],
                        Some(auth_response.refresh_jwt),
                        auth_response.expires_at,
                    )
                    .await;
                    info!("Re-authenticated successfully");
                    Ok(())
                }
                Err(e) => {
                    error!("Re-authentication failed: {}", e);
                    Err(e)
                }
            }
        } else {
            Err(TurboError::ExpiredToken(
                "No auth client available for re-authentication".to_string(),
            ))
        }
    }

    pub async fn get_session_count(&self) -> usize {
        self.session_strings.read().await.len()
    }

    fn log_partial_percentage(&self, name: &str) {
        let total = if name == "profiles" {
            self.profile_batches_total.load(Ordering::Relaxed)
        } else {
            self.post_batches_total.load(Ordering::Relaxed)
        };

        if total > 0 && total % 10 == 0 {
            let partial = if name == "profiles" {
                self.profile_batches_partial.load(Ordering::Relaxed)
            } else {
                self.post_batches_partial.load(Ordering::Relaxed)
            };

            let pct = (partial as f64 / total as f64) * 100.0;
            info!(
                "{} batch partial rate: {:.1}% ({}/{})",
                name, pct, partial, total
            );
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn test_bluesky_client_creation() {
        let sessions = vec!["session1:::bsky.social".to_string()];
        let client = BlueskyClient::new(sessions, None);
        assert_eq!(client.get_session_count().await, 1);
    }

    #[tokio::test]
    async fn test_refresh_sessions() {
        let client = BlueskyClient::new(vec!["old_session".to_string()], None);
        assert_eq!(client.get_session_count().await, 1);

        client
            .refresh_sessions(
                vec![
                    "new_session1:::bsky.social".to_string(),
                    "new_session2:::bsky.social".to_string(),
                ],
                Some("new_refresh_jwt".to_string()),
                Some("2024-01-01T00:00:00.000Z".to_string()),
            )
            .await;

        assert_eq!(client.get_session_count().await, 2);
    }
}
