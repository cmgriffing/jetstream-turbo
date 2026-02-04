use reqwest::Client;
use std::time::Duration;
use tracing::{debug, error, info};
use crate::models::errors::{TurboError, TurboResult};
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct Credential {
    pub session_string: String,
    pub created_at: String,
    pub expires_at: Option<String>,
    pub domain: String,
}

pub struct GrazeClient {
    http_client: Client,
    base_url: String,
    credential_secret: String,
    max_retries: u32,
    retry_delay: Duration,
}

impl GrazeClient {
    pub fn new(base_url: String, credential_secret: String) -> Self {
        Self {
            http_client: Client::builder()
                .timeout(Duration::from_secs(10))
                .user_agent("jetstream-turbo/0.1.0")
                .build()
                .expect("Failed to create HTTP client"),
            base_url,
            credential_secret,
            max_retries: 3,
            retry_delay: Duration::from_millis(500),
        }
    }
    
    pub async fn fetch_session_strings(&self) -> TurboResult<Vec<String>> {
        let url = format!(
            "{}/app/api/v1/turbo-tokens/credentials?credential_secret={}",
            self.base_url.trim_end_matches('/'),
            self.credential_secret
        );
        
        info!("Fetching session strings from Graze API");
        
        let mut attempt = 0;
        loop {
            let response = self.http_client
                .get(&url)
                .send()
                .await;
            
            match response {
                Ok(resp) => {
                    match resp.status() {
                        reqwest::StatusCode::OK => {
                            let credentials: Vec<Credential> = resp.json().await?;
                            let session_strings: Vec<String> = credentials
                                .into_iter()
                                .map(|cred| cred.session_string)
                                .collect();
                            
                            info!("Successfully fetched {} session strings", session_strings.len());
                            return Ok(session_strings);
                        }
                        reqwest::StatusCode::UNAUTHORIZED => {
                            error!("Unauthorized - check credential_secret");
                            return Err(TurboError::PermissionDenied(
                                "Invalid credential_secret".to_string()
                            ));
                        }
                        reqwest::StatusCode::NOT_FOUND => {
                            error!("Graze API endpoint not found: {}", url);
                            return Err(TurboError::NotFound(
                                "API endpoint not found".to_string()
                            ));
                        }
                        status => {
                            let error_text = resp.text().await.unwrap_or_default();
                            error!("Graze API error {}: {}", status, error_text);
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
                tokio::time::sleep(self.retry_delay * (attempt as u64)).await;
            }
        }
    }
    
    pub async fn validate_connection(&self) -> TurboResult<bool> {
        let url = format!("{}/health", self.base_url.trim_end_matches('/'));
        
        match self.http_client.get(&url).send().await {
            Ok(resp) => {
                Ok(resp.status().is_success())
            }
            Err(_) => Ok(false)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use wiremock::{MockServer, Mock, ResponseTemplate};
    use wiremock::matchers::{method, path, query_param};
    
    #[tokio::test]
    async fn test_graze_client_success() {
        let mock_server = MockServer::start().await;
        
        let expected_credentials = vec![
            Credential {
                session_string: "session1:::bsky.social".to_string(),
                created_at: "2023-01-01T00:00:00Z".to_string(),
                expires_at: None,
                domain: "bsky.social".to_string(),
            },
            Credential {
                session_string: "session2:::bsky.social".to_string(),
                created_at: "2023-01-01T00:00:00Z".to_string(),
                expires_at: None,
                domain: "bsky.social".to_string(),
            },
        ];
        
        Mock::given(method("GET"))
            .and(path("/app/api/v1/turbo-tokens/credentials"))
            .and(query_param("credential_secret", "test_secret"))
            .respond_with(ResponseTemplate::new(200).set_body_json(&expected_credentials))
            .mount(&mock_server)
            .await;
        
        let client = GrazeClient::new(
            mock_server.uri(),
            "test_secret".to_string(),
        );
        
        let result = client.fetch_session_strings().await.unwrap();
        assert_eq!(result.len(), 2);
        assert_eq!(result[0], "session1:::bsky.social");
        assert_eq!(result[1], "session2:::bsky.social");
    }
    
    #[tokio::test]
    async fn test_graze_client_unauthorized() {
        let mock_server = MockServer::start().await;
        
        Mock::given(method("GET"))
            .and(path("/app/api/v1/turbo-tokens/credentials"))
            .and(query_param("credential_secret", "invalid_secret"))
            .respond_with(ResponseTemplate::new(401))
            .mount(&mock_server)
            .await;
        
        let client = GrazeClient::new(
            mock_server.uri(),
            "invalid_secret".to_string(),
        );
        
        let result = client.fetch_session_strings().await;
        assert!(result.is_err());
        assert!(matches!(result.unwrap_err(), TurboError::PermissionDenied(_)));
    }
    
    #[tokio::test]
    async fn test_graze_client_not_found() {
        let mock_server = MockServer::start().await;
        
        Mock::given(method("GET"))
            .and(path("/app/api/v1/turbo-tokens/credentials"))
            .and(query_param("credential_secret", "test_secret"))
            .respond_with(ResponseTemplate::new(404))
            .mount(&mock_server)
            .await;
        
        let client = GrazeClient::new(
            mock_server.uri(),
            "test_secret".to_string(),
        );
        
        let result = client.fetch_session_strings().await;
        assert!(result.is_err());
        assert!(matches!(result.unwrap_err(), TurboError::NotFound(_)));
    }
}