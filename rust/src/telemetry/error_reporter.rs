use crate::models::errors::TurboError;
use std::collections::HashMap;
use std::time::Duration;
use tokio::sync::mpsc;
use tokio::time::interval;

const DEFAULT_BATCH_SIZE: usize = 50;
const DEFAULT_FLUSH_INTERVAL_SECS: u64 = 60;

#[derive(Debug, Clone)]
pub struct ErrorEvent {
    pub error_type: String,
    pub message: String,
    pub is_retryable: bool,
    pub is_critical: bool,
    pub context: HashMap<String, String>,
}

#[derive(Clone)]
pub struct ErrorReporter {
    tx: mpsc::Sender<ErrorEvent>,
}

fn mask_api_key(key: &str) -> String {
    if key.len() <= 8 {
        return "****".to_string();
    }
    format!("{}...{}", &key[..4], &key[key.len() - 4..])
}

impl ErrorReporter {
    pub async fn new(api_key: Option<String>, host: Option<String>) -> Self {
        let (tx, rx) = mpsc::channel::<ErrorEvent>(512);

        match api_key {
            None => {
                tracing::info!("PostHog error reporting disabled (no POSTHOG_API_KEY configured)");
                Self { tx }
            }
            Some(key) => {
                let host = host.unwrap_or_else(|| "https://us.i.posthog.com".to_string());
                tracing::info!(
                    "Initializing PostHog error reporting (host: {}, api_key: {})",
                    host,
                    mask_api_key(&key)
                );

                let options = posthog_rs::ClientOptions::from((key.as_str(), host.as_str()));
                let client = posthog_rs::client(options).await;

                match Self::validate_connection(&client, &key).await {
                    Ok(_) => {
                        tracing::info!("PostHog connection validated successfully");
                    }
                    Err(e) => {
                        tracing::error!("PostHog connection validation failed: {}", e);
                        tracing::warn!("Error reporting will continue but events may fail to send");
                    }
                }

                tokio::spawn(async move {
                    Self::flush_loop(client, rx).await;
                });

                Self { tx }
            }
        }
    }

    pub fn capture_error(&self, error: &TurboError, context: HashMap<&str, &str>) {
        let event = ErrorEvent {
            error_type: Self::error_type_name(error),
            message: error.to_string(),
            is_retryable: error.is_retryable(),
            is_critical: error.is_critical(),
            context: context
                .into_iter()
                .map(|(k, v)| (k.to_string(), v.to_string()))
                .collect(),
        };

        if let Err(e) = self.tx.try_send(event) {
            tracing::warn!("Error buffer full, dropping error: {}", e);
        }
    }

    fn error_type_name(error: &TurboError) -> String {
        match error {
            TurboError::JetstreamConnection(_) => "JetstreamConnection",
            TurboError::WebSocketConnection(_) => "WebSocketConnection",
            TurboError::HttpRequest(_) => "HttpRequest",
            TurboError::RateLimitExceeded => "RateLimitExceeded",
            TurboError::InvalidApiResponse(_) => "InvalidApiResponse",
            TurboError::Configuration(_) => "Configuration",
            TurboError::MissingEnvVar(_) => "MissingEnvVar",
            TurboError::Database(_) => "Database",
            TurboError::RedisOperation(_) => "RedisOperation",
            TurboError::JsonSerialization(_) => "JsonSerialization",
            TurboError::JsonDeserialization(_) => "JsonDeserialization",
            TurboError::CacheOperation(_) => "CacheOperation",
            TurboError::InvalidMessage(_) => "InvalidMessage",
            TurboError::HydrationFailed(_) => "HydrationFailed",
            TurboError::RotationFailed(_) => "RotationFailed",
            TurboError::Io(_) => "Io",
            TurboError::TaskJoin(_) => "TaskJoin",
            TurboError::Timeout(_) => "Timeout",
            TurboError::Internal(_) => "Internal",
            TurboError::NotFound(_) => "NotFound",
            TurboError::PermissionDenied(_) => "PermissionDenied",
            TurboError::ExpiredToken(_) => "ExpiredToken",
        }
        .to_string()
    }

    async fn validate_connection(client: &posthog_rs::Client, api_key: &str) -> Result<(), String> {
        let mut test_event = posthog_rs::Event::new("$exception", "jetstream-turbo");
        let _ = test_event.insert_prop("$lib", "jetstream-turbo");
        let _ = test_event.insert_prop("test_event", true);

        match client.capture_batch(vec![test_event]).await {
            Ok(_) => Ok(()),
            Err(e) => {
                let error_str = e.to_string().to_lowercase();
                if error_str.contains("401") || error_str.contains("unauthorized") || error_str.contains("invalid") {
                    Err(format!(
                        "Authentication error - check POSTHOG_API_KEY ({})",
                        mask_api_key(api_key)
                    ))
                } else if error_str.contains("403") || error_str.contains("forbidden") {
                    Err("Permission denied - API key lacks required scope".to_string())
                } else if error_str.contains("timeout") || error_str.contains("connection") {
                    Err("Network error - unable to reach host".to_string())
                } else {
                    Err(format!("Connection failed: {}", e))
                }
            }
        }
    }

    async fn flush_loop(client: posthog_rs::Client, mut rx: mpsc::Receiver<ErrorEvent>) {
        let mut flush_interval = interval(Duration::from_secs(DEFAULT_FLUSH_INTERVAL_SECS));
        let mut batch: Vec<ErrorEvent> = Vec::with_capacity(DEFAULT_BATCH_SIZE);

        loop {
            tokio::select! {
                _ = flush_interval.tick() => {
                    if !batch.is_empty() {
                        Self::flush_batch(&client, &batch).await;
                        batch.clear();
                    }
                }
                Some(event) = rx.recv() => {
                    batch.push(event);
                    if batch.len() >= DEFAULT_BATCH_SIZE {
                        Self::flush_batch(&client, &batch).await;
                        batch.clear();
                    }
                }
                else => break,
            }
        }

        if !batch.is_empty() {
            Self::flush_batch(&client, &batch).await;
        }
    }

    async fn flush_batch(client: &posthog_rs::Client, batch: &[ErrorEvent]) {
        let event_count = batch.len();
        tracing::debug!("Sending {} error events to PostHog", event_count);

        let events: Vec<posthog_rs::Event> = batch
            .iter()
            .map(|event| {
                let mut ph_event = posthog_rs::Event::new("$exception", "jetstream-turbo");
                let _ = ph_event.insert_prop("$exception_type", &event.error_type);
                let _ = ph_event.insert_prop("$exception_message", &event.message);
                let _ = ph_event.insert_prop("is_retryable", event.is_retryable);
                let _ = ph_event.insert_prop("is_critical", event.is_critical);
                for (key, value) in &event.context {
                    let _ = ph_event.insert_prop(key, value);
                }
                ph_event
            })
            .collect();

        if let Err(e) = client.capture_batch(events).await {
            let error_str = e.to_string().to_lowercase();

            if error_str.contains("401") || error_str.contains("unauthorized") {
                tracing::error!(
                    "PostHog authentication failed (401): Invalid API key - {} events dropped",
                    event_count
                );
            } else if error_str.contains("403") || error_str.contains("forbidden") {
                tracing::error!(
                    "PostHog permission denied (403): API key lacks required scope - {} events dropped",
                    event_count
                );
            } else if error_str.contains("429") || error_str.contains("rate limit") {
                tracing::warn!(
                    "PostHog rate limited (429): {} events dropped (consider reducing error volume)",
                    event_count
                );
            } else if error_str.contains("timeout") {
                tracing::warn!(
                    "PostHog request timed out - {} events dropped",
                    event_count
                );
            } else if error_str.contains("connection") {
                tracing::warn!(
                    "PostHog network error: {} - {} events dropped",
                    e,
                    event_count
                );
            } else {
                tracing::warn!(
                    "PostHog request failed: {} ({} events dropped)",
                    e,
                    event_count
                );
            }
        } else {
            tracing::debug!("Successfully sent {} error events to PostHog", event_count);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_error_type_name() {
        let error = TurboError::RateLimitExceeded;
        assert_eq!(ErrorReporter::error_type_name(&error), "RateLimitExceeded");

        let error = TurboError::InvalidApiResponse("test error".to_string());
        assert_eq!(ErrorReporter::error_type_name(&error), "InvalidApiResponse");

        let error = TurboError::Internal("test internal".to_string());
        assert_eq!(ErrorReporter::error_type_name(&error), "Internal");
    }
}
