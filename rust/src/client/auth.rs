use reqwest::Client;
use std::time::Duration;
use tracing::{debug, error, info, warn};
use crate::models::errors::{TurboError, TurboResult};
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct AuthResponse {
    pub access_jwt: String,
    pub refresh_jwt: String,
    pub handle: String,
    pub did: String,
}

pub struct BlueskyAuthClient {
    http_client: Client,
    handle: String,
    app_password: String,
    api_base_url: String,
    max_retries: u32,
    retry_delay: Duration,
}

impl BlueskyAuthClient {
    pub fn new(handle: String, app_password: String) -> Self {
        Self {
            http_client: Client::builder()
                .timeout(Duration::from_secs(10))
                .user_agent("jetstream-turbo/0.1.0")
                .build()
                .expect("Failed to create HTTP client"),
            handle,
            app_password,
            api_base_url: "https://bsky.social/xrpc".to_string(),
            max_retries: 3,
            retry_delay: Duration::from_millis(500),
        }
    }
    
    /// Authenticate with Bluesky and get a session token
    pub async fn authenticate(&self) -> TurboResult<String> {
        let url = format!("{}/com.atproto.server.createSession", self.api_base_url);
        
        let request_body = serde_json::json!({
            "identifier": self.handle,
            "password": self.app_password,
        });
        
        info!("Authenticating with Bluesky as {}", self.handle);
        
        let mut attempt = 0;
        loop {
            let response = self.http_client
                .post(&url)
                .json(&request_body)
                .send()
                .await;
            
            match response {
                Ok(resp) => {
                    match resp.status() {
                        reqwest::StatusCode::OK => {
                            let auth_response: AuthResponse = resp.json().await?;
                            info!("Successfully authenticated with Bluesky as {}", auth_response.handle);
                            
                            // Return session string in the format expected by BlueskyClient
                            // Format: {access_jwt}:::{domain}
                            let domain = self.extract_domain(&auth_response.handle);
                            let session_string = format!("{}::: {}", auth_response.access_jwt, domain);
                            
                            return Ok(session_string);
                        }
                        reqwest::StatusCode::UNAUTHORIZED => {
                            error!("Authentication failed - invalid handle or app password");
                            return Err(TurboError::PermissionDenied(
                                "Invalid Bluesky handle or app password".to_string()
                            ));
                        }
                        reqwest::StatusCode::TOO_MANY_REQUESTS => {
                            warn!("Rate limited during authentication, waiting before retry");
                            tokio::time::sleep(self.retry_delay * 2).await;
                        }
                        status => {
                            let error_text = resp.text().await.unwrap_or_default();
                            error!("Bluesky auth error {}: {}", status, error_text);
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
                debug!("Retry attempt {} in {}ms", attempt, self.retry_delay.as_millis());
                tokio::time::sleep(self.retry_delay.saturating_mul(attempt)).await;
            }
        }
    }
    
    /// Extract domain from handle (e.g., "user.bsky.social" -> "bsky.social")
    fn extract_domain(&self, handle: &str) -> String {
        handle.split('.').skip(1).collect::<Vec<_>>().join(".")
    }
    
    /// Validate that credentials work by attempting authentication
    pub async fn validate_credentials(&self) -> TurboResult<bool> {
        match self.authenticate().await {
            Ok(_) => Ok(true),
            Err(TurboError::PermissionDenied(_)) => Ok(false),
            Err(_) => Ok(false),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use wiremock::{MockServer, Mock, ResponseTemplate};
    use wiremock::matchers::{method, path};
    
    #[tokio::test]
    async fn test_auth_success() {
        let mock_server = MockServer::start().await;
        
        let auth_response = AuthResponse {
            access_jwt: "test_jwt_token".to_string(),
            refresh_jwt: "test_refresh_token".to_string(),
            handle: "test.bsky.social".to_string(),
            did: "did:plc:test".to_string(),
        };
        
        Mock::given(method("POST"))
            .and(path("/com.atproto.server.createSession"))
            .respond_with(ResponseTemplate::new(200).set_body_json(&auth_response))
            .mount(&mock_server)
            .await;
        
        let client = BlueskyAuthClient {
            http_client: Client::new(),
            handle: "test.bsky.social".to_string(),
            app_password: "test-password".to_string(),
            api_base_url: mock_server.uri(),
            max_retries: 3,
            retry_delay: Duration::from_millis(100),
        };
        
        let result = client.authenticate().await.unwrap();
        assert!(result.contains("test_jwt_token"));
        assert!(result.contains("bsky.social"));
    }
    
    #[tokio::test]
    async fn test_auth_unauthorized() {
        let mock_server = MockServer::start().await;
        
        Mock::given(method("POST"))
            .and(path("/com.atproto.server.createSession"))
            .respond_with(ResponseTemplate::new(401))
            .mount(&mock_server)
            .await;
        
        let client = BlueskyAuthClient {
            http_client: Client::new(),
            handle: "test.bsky.social".to_string(),
            app_password: "wrong-password".to_string(),
            api_base_url: mock_server.uri(),
            max_retries: 3,
            retry_delay: Duration::from_millis(100),
        };
        
        let result = client.authenticate().await;
        assert!(result.is_err());
        assert!(matches!(result.unwrap_err(), TurboError::PermissionDenied(_)));
    }
    
    #[test]
    fn test_extract_domain() {
        let client = BlueskyAuthClient::new(
            "user.bsky.social".to_string(),
            "password".to_string(),
        );
        
        assert_eq!(client.extract_domain("user.bsky.social"), "bsky.social");
        assert_eq!(client.extract_domain("test.example.com"), "example.com");
        assert_eq!(client.extract_domain("handle"), "");
    }
}