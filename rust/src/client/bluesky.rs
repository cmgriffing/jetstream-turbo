use reqwest::{Client, StatusCode};
use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::RwLock;
use governor::{Quota, RateLimiter};
use tracing::{debug, error, info, warn};
use crate::models::{
    bluesky::{BlueskyProfile, BlueskyPost, GetProfileResponse, GetProfilesResponse, GetPostResponse},
    errors::{TurboError, TurboResult},
};

pub struct BlueskyClient {
    http_client: Client,
    session_strings: Arc<RwLock<Vec<String>>>,
    rate_limiter: Arc<RateLimiter<governor::state::direct::DirectStateHandle, governor::clock::DefaultClock>>,
    api_base_url: String,
    max_retries: u32,
    retry_delay_ms: u64,
}

impl BlueskyClient {
    pub fn new(session_strings: Vec<String>) -> Self {
        Self {
            http_client: Client::builder()
                .timeout(Duration::from_secs(30))
                .user_agent("jetstream-turbo/0.1.0")
                .build()
                .expect("Failed to create HTTP client"),
            session_strings: Arc::new(RwLock::new(session_strings)),
            rate_limiter: Arc::new(RateLimiter::direct(Quota::per_minute(60))), // 60 requests per minute
            api_base_url: "https://bsky.social/xrpc".to_string(),
            max_retries: 3,
            retry_delay_ms: 200,
        }
    }
    
    pub async fn bulk_fetch_profiles(&self, dids: &[String]) -> TurboResult<Vec<Option<BlueskyProfile>>> {
        let mut profiles: Vec<Option<BlueskyProfile>> = Vec::with_capacity(dids.len());
        
        // Process in chunks of 25 (Bluesky API limit)
        for chunk in dids.chunks(25) {
            let chunk_profiles = self.fetch_profiles_batch(chunk).await?;
            profiles.extend(chunk_profiles);
        }
        
        Ok(profiles)
    }
    
    async fn fetch_profiles_batch(&self, dids: &[String]) -> TurboResult<Vec<Option<BlueskyProfile>>> {
        let url = format!("{}/app.bsky.actor.getProfiles", self.api_base_url);
        
        let mut request_body = HashMap::new();
        request_body.insert("actors".to_string(), serde_json::Value::Array(
            dids.iter().map(|did| serde_json::Value::String(did.clone())).collect()
        ));
        
        let session_string = self.get_session_string().await?;
        
        let mut attempt = 0;
        loop {
            // Rate limit check
            self.rate_limiter.until_ready().await;
            
            let response = self.http_client
                .post(&url)
                .header("Authorization", format!("Bearer {}", session_string))
                .json(&request_body)
                .send()
                .await;
            
            match response {
                Ok(resp) => {
                    match resp.status() {
                        StatusCode::OK => {
                            let profiles_response: GetProfilesResponse = resp.json().await?;
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
                            return Err(TurboError::PermissionDenied("Invalid session token".to_string()));
                        }
                        status => {
                            let error_text = resp.text().await.unwrap_or_default();
                            error!("API error {}: {}", status, error_text);
                            return Err(TurboError::InvalidApiResponse(format!(
                                "Status {}: {}", status, error_text
                            )));
                        }
                    }
                }
                Err(e) => {
                    error!("HTTP request failed: {}", e);
                    if attempt >= self.max_retries {
                        return Err(TurboError::HttpRequest(e));
                    }
                }
            }
            
            attempt += 1;
            if attempt <= self.max_retries {
                tokio::time::sleep(self.retry_delay * (attempt as u64)).await;
            }
        }
    }
    
    pub async fn bulk_fetch_posts(&self, uris: &[String]) -> TurboResult<Vec<Option<BlueskyPost>>> {
        let mut posts: Vec<Option<BlueskyPost>> = Vec::with_capacity(uris.len());
        
        // Process in chunks of 25 (Bluesky API limit)
        for chunk in uris.chunks(25) {
            let chunk_posts = self.fetch_posts_batch(chunk).await?;
            posts.extend(chunk_posts);
        }
        
        Ok(posts)
    }
    
    async fn fetch_posts_batch(&self, uris: &[String]) -> TurboResult<Vec<Option<BlueskyPost>>> {
        let mut posts: Vec<Option<BlueskyPost>> = Vec::with_capacity(uris.len());
        
        // Bluesky doesn't have a bulk post endpoint, so we fetch them individually
        // with proper rate limiting
        for uri in uris {
            let post = self.fetch_single_post(uri).await?;
            posts.push(post);
        }
        
        Ok(posts)
    }
    
    async fn fetch_single_post(&self, uri: &str) -> TurboResult<Option<BlueskyPost>> {
        let url = format!("{}/app.bsky.feed.getPostThread", self.api_base_url);
        
        let mut request_body = HashMap::new();
        request_body.insert("uri".to_string(), serde_json::Value::String(uri.to_string()));
        
        let session_string = self.get_session_string().await?;
        
        let mut attempt = 0;
        loop {
            // Rate limit check
            self.rate_limiter.until_ready().await;
            
            let response = self.http_client
                .post(&url)
                .header("Authorization", format!("Bearer {}", session_string))
                .json(&request_body)
                .send()
                .await;
            
            match response {
                Ok(resp) => {
                    match resp.status() {
                        StatusCode::OK => {
                            let thread_response: serde_json::Value = resp.json().await?;
                            
                            // Extract the post from thread structure
                            if let Some(post_data) = thread_response.get("thread") {
                                if let Some(post_obj) = post_data.get("post") {
                                    let post_response: GetPostResponse = serde_json::from_value(post_obj.clone())?;
                                    return Ok(Some(self.convert_post_response(post_response)));
                                }
                            }
                            return Ok(None);
                        }
                        StatusCode::NOT_FOUND => {
                            return Ok(None);
                        }
                        StatusCode::TOO_MANY_REQUESTS => {
                            warn!("Rate limited, waiting before retry");
                            tokio::time::sleep(self.retry_delay * 2).await;
                        }
                        StatusCode::UNAUTHORIZED => {
                            error!("Unauthorized - session may be invalid: {}", session_string);
                            return Err(TurboError::PermissionDenied("Invalid session token".to_string()));
                        }
                        status => {
                            let error_text = resp.text().await.unwrap_or_default();
                            error!("API error {}: {}", status, error_text);
                            return Err(TurboError::InvalidApiResponse(format!(
                                "Status {}: {}", status, error_text
                            )));
                        }
                    }
                }
                Err(e) => {
                    error!("HTTP request failed: {}", e);
                    if attempt >= self.max_retries {
                        return Err(TurboError::HttpRequest(e));
                    }
                }
            }
            
            attempt += 1;
            if attempt <= self.max_retries {
                tokio::time::sleep(self.retry_delay * (attempt as u64)).await;
            }
        }
    }
    
    async fn get_session_string(&self) -> TurboResult<String> {
        let sessions = self.session_strings.read().await;
        
        if sessions.is_empty() {
            return Err(TurboError::PermissionDenied("No valid session strings available".to_string()));
        }
        
        // Round-robin through session strings
        // In a more sophisticated implementation, we could track which sessions are healthy
        Ok(sessions[0].clone())
    }
    
    fn convert_post_response(&self, response: GetPostResponse) -> BlueskyPost {
        BlueskyPost {
            uri: response.uri,
            cid: response.cid,
            author: response.author.into(),
            text: response.record
                .get("text")
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .to_string(),
            created_at: chrono::Utc::now(), // Should parse from record
            embed: response.embed.map(|e| serde_json::from_value(e).ok()).flatten(),
            reply: response.reply.map(|r| serde_json::from_value(r).ok()).flatten(),
            facets: response.record
                .get("facets")
                .and_then(|v| serde_json::from_value(v).ok()),
            labels: response.labels,
            like_count: response.like_count,
            repost_count: response.repost_count,
            reply_count: response.reply_count,
        }
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
    
    #[test]
    fn test_bluesky_client_creation() {
        let sessions = vec!["session1:::bsky.social".to_string()];
        let client = BlueskyClient::new(sessions);
        assert_eq!(client.get_session_count().blocking_now(), 1);
    }
    
    #[tokio::test]
    async fn test_refresh_sessions() {
        let client = BlueskyClient::new(vec!["old_session".to_string()]);
        assert_eq!(client.get_session_count().await, 1);
        
        client.refresh_sessions(vec![
            "new_session1:::bsky.social".to_string(),
            "new_session2:::bsky.social".to_string(),
        ]).await;
        
        assert_eq!(client.get_session_count().await, 2);
    }
}