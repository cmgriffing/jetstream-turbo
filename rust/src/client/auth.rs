use crate::models::errors::{TurboError, TurboResult};
use reqwest::Client;
use serde::{Deserialize, Serialize};
use std::time::Duration;
use tracing::{debug, error, info, warn};

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct AuthResponse {
    #[serde(rename = "accessJwt")]
    pub access_jwt: String,
    #[serde(rename = "refreshJwt")]
    pub refresh_jwt: String,
    pub handle: String,
    pub did: String,
    #[serde(default)]
    pub email: Option<String>,
    #[serde(default, rename = "emailConfirmed")]
    pub email_confirmed: Option<bool>,
    #[serde(default)]
    pub active: Option<bool>,
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
        Self::with_api_url(handle, app_password, "https://bsky.social/xrpc".to_string())
    }

    pub fn with_api_url(handle: String, app_password: String, api_base_url: String) -> Self {
        Self {
            http_client: Client::builder()
                .timeout(Duration::from_secs(10))
                .user_agent("jetstream-turbo/0.1.0")
                .build()
                .expect("Failed to create HTTP client"),
            handle,
            app_password,
            api_base_url,
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
            let response = self.http_client.post(&url).json(&request_body).send().await;

            match response {
                Ok(resp) => {
                    match resp.status() {
                        reqwest::StatusCode::OK => {
                            let body_text = resp.text().await?;
                            debug!("Auth response body: {}", body_text);

                            let auth_response: AuthResponse = match serde_json::from_str(&body_text)
                            {
                                Ok(r) => r,
                                Err(e) => {
                                    error!(
                                        "Failed to parse auth response: {}. Body: {}",
                                        e, body_text
                                    );
                                    return Err(TurboError::InvalidApiResponse(format!(
                                        "Failed to parse auth response: {}. Response: {}",
                                        e, body_text
                                    )));
                                }
                            };

                            if auth_response.access_jwt.is_empty() {
                                error!(
                                    "Auth response missing access_jwt. Full response: {:?}",
                                    auth_response
                                );
                                return Err(TurboError::InvalidApiResponse(
                                    "Auth response missing access_jwt field".to_string(),
                                ));
                            }

                            info!(
                                "Successfully authenticated with Bluesky as {}",
                                auth_response.handle
                            );

                            // Return the access JWT directly - this is what Bluesky API expects in Authorization header
                            return Ok(auth_response.access_jwt);
                        }
                        reqwest::StatusCode::UNAUTHORIZED => {
                            error!("Authentication failed - invalid handle or app password");
                            return Err(TurboError::PermissionDenied(
                                "Invalid Bluesky handle or app password".to_string(),
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
                                "Status {status}: {error_text}"
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
                debug!(
                    "Retry attempt {} in {}ms",
                    attempt,
                    self.retry_delay.as_millis()
                );
                tokio::time::sleep(self.retry_delay.saturating_mul(attempt)).await;
            }
        }
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
    use wiremock::matchers::{method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    #[tokio::test]
    async fn test_auth_success() {
        let mock_server = MockServer::start().await;

        let auth_response = AuthResponse {
            access_jwt: "test_jwt_token".to_string(),
            refresh_jwt: "test_refresh_token".to_string(),
            handle: "test.bsky.social".to_string(),
            did: "did:plc:test".to_string(),
            email: None,
            email_confirmed: None,
            active: None,
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
        assert_eq!(result, "test_jwt_token");
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
        assert!(matches!(
            result.unwrap_err(),
            TurboError::PermissionDenied(_)
        ));
    }
}
