use crate::models::errors::TurboError;
use std::collections::HashMap;
use std::time::Duration;
use tokio::sync::{mpsc, oneshot};
use tokio::time::{interval, timeout, Instant};

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
    tx: mpsc::Sender<ReporterMessage>,
}

enum ReporterMessage {
    Event(ErrorEvent),
    Flush(oneshot::Sender<()>),
}

fn mask_api_key(key: &str) -> String {
    if key.len() <= 8 {
        return "****".to_string();
    }
    format!("{}...{}", &key[..4], &key[key.len() - 4..])
}

impl ErrorReporter {
    pub async fn new(api_key: Option<String>, host: Option<String>) -> Self {
        let (tx, rx) = mpsc::channel::<ReporterMessage>(512);

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

        if let Err(e) = self.tx.try_send(ReporterMessage::Event(event)) {
            tracing::warn!("Error buffer full, dropping error: {}", e);
        }
    }

    pub fn capture_unhandled_failure(
        &self,
        failure_type: &str,
        message: &str,
        context: HashMap<&str, &str>,
    ) {
        let event = Self::unhandled_failure_event(failure_type, message, context);

        if let Err(e) = self.tx.try_send(ReporterMessage::Event(event)) {
            tracing::warn!("Error buffer full, dropping unhandled failure: {}", e);
        }
    }

    pub async fn flush_with_timeout(&self, timeout_duration: Duration) -> bool {
        let deadline = Instant::now() + timeout_duration;
        let (done_tx, done_rx) = oneshot::channel();
        let send_timeout = deadline.saturating_duration_since(Instant::now());

        if send_timeout.is_zero() {
            tracing::warn!("Timed out before requesting telemetry flush");
            return false;
        }

        match timeout(send_timeout, self.tx.send(ReporterMessage::Flush(done_tx))).await {
            Ok(Ok(())) => {}
            Ok(Err(e)) => {
                tracing::warn!("Unable to request telemetry flush: {}", e);
                return false;
            }
            Err(_) => {
                tracing::warn!("Timed out requesting telemetry flush");
                return false;
            }
        }

        let wait_timeout = deadline.saturating_duration_since(Instant::now());
        if wait_timeout.is_zero() {
            tracing::warn!("Timed out waiting for telemetry flush");
            return false;
        }

        match timeout(wait_timeout, done_rx).await {
            Ok(Ok(())) => true,
            Ok(Err(e)) => {
                tracing::warn!("Telemetry flush confirmation failed: {}", e);
                false
            }
            Err(_) => {
                tracing::warn!("Timed out waiting for telemetry flush confirmation");
                false
            }
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

    fn unhandled_failure_event(
        failure_type: &str,
        message: &str,
        context: HashMap<&str, &str>,
    ) -> ErrorEvent {
        ErrorEvent {
            error_type: failure_type.to_string(),
            message: message.to_string(),
            is_retryable: false,
            is_critical: true,
            context: context
                .into_iter()
                .map(|(k, v)| (k.to_string(), v.to_string()))
                .collect(),
        }
    }

    async fn validate_connection(client: &posthog_rs::Client, api_key: &str) -> Result<(), String> {
        let mut test_event = posthog_rs::Event::new("$exception", "jetstream-turbo");
        let _ = test_event.insert_prop("$lib", "jetstream-turbo");
        let _ = test_event.insert_prop("test_event", true);

        match client.capture_batch(vec![test_event], false).await {
            Ok(_) => Ok(()),
            Err(e) => {
                let error_str = e.to_string().to_lowercase();
                if error_str.contains("401")
                    || error_str.contains("unauthorized")
                    || error_str.contains("invalid")
                {
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

    async fn flush_loop(client: posthog_rs::Client, mut rx: mpsc::Receiver<ReporterMessage>) {
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
                Some(message) = rx.recv() => {
                    match message {
                        ReporterMessage::Event(event) => {
                            batch.push(event);
                            if batch.len() >= DEFAULT_BATCH_SIZE {
                                Self::flush_batch(&client, &batch).await;
                                batch.clear();
                            }
                        }
                        ReporterMessage::Flush(done_tx) => {
                            if !batch.is_empty() {
                                Self::flush_batch(&client, &batch).await;
                                batch.clear();
                            }
                            let _ = done_tx.send(());
                        }
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

        if let Err(e) = client.capture_batch(events, false).await {
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
                tracing::warn!("PostHog request timed out - {} events dropped", event_count);
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
    use serde_json::Value;
    use wiremock::matchers::{method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    #[test]
    fn test_error_type_name() {
        let error = TurboError::RateLimitExceeded;
        assert_eq!(ErrorReporter::error_type_name(&error), "RateLimitExceeded");

        let error = TurboError::InvalidApiResponse("test error".to_string());
        assert_eq!(ErrorReporter::error_type_name(&error), "InvalidApiResponse");

        let error = TurboError::Internal("test internal".to_string());
        assert_eq!(ErrorReporter::error_type_name(&error), "Internal");
    }

    #[test]
    fn test_unhandled_failure_event() {
        let mut context = HashMap::new();
        context.insert("component", "main");
        context.insert("operation", "panic_hook");

        let event = ErrorReporter::unhandled_failure_event("panic", "boom", context);

        assert_eq!(event.error_type, "panic");
        assert_eq!(event.message, "boom");
        assert!(!event.is_retryable);
        assert!(event.is_critical);
        assert_eq!(event.context.get("component"), Some(&"main".to_string()));
        assert_eq!(
            event.context.get("operation"),
            Some(&"panic_hook".to_string())
        );
    }

    #[tokio::test]
    async fn disabled_configuration_drops_events_and_cannot_flush() {
        let reporter = ErrorReporter::new(None, None).await;

        let mut context = HashMap::new();
        context.insert("component", "test");
        reporter.capture_error(&TurboError::Internal("ignored".to_string()), context);

        let mut crash_context = HashMap::new();
        crash_context.insert("component", "runtime");
        reporter.capture_unhandled_failure("Panic", "boom", crash_context);

        assert!(!reporter.flush_with_timeout(Duration::from_millis(50)).await);
    }

    #[tokio::test]
    async fn startup_validation_and_flush_emit_expected_payloads() {
        let mock_server = MockServer::start().await;

        Mock::given(method("POST"))
            .and(path("/i/v0/e/"))
            .respond_with(ResponseTemplate::new(200))
            .mount(&mock_server)
            .await;

        let reporter = ErrorReporter::new(
            Some("phc_test_project_key".to_string()),
            Some(mock_server.uri()),
        )
        .await;

        let mut handled_context = HashMap::new();
        handled_context.insert("component", "main");
        handled_context.insert("operation", "server_run");
        reporter.capture_error(
            &TurboError::Internal("server failed".to_string()),
            handled_context,
        );

        let mut crash_context = HashMap::new();
        crash_context.insert("component", "runtime");
        crash_context.insert("operation", "panic_hook");
        crash_context.insert("panic_location", "src/main.rs:10:5");
        reporter.capture_unhandled_failure("Panic", "simulated panic", crash_context);

        assert!(reporter.flush_with_timeout(Duration::from_secs(1)).await);

        let requests = mock_server
            .received_requests()
            .await
            .expect("requests should be captured");

        assert_eq!(requests.len(), 2, "expected validation and flush requests");

        let validation_payload: Value =
            serde_json::from_slice(&requests[0].body).expect("validation payload should be json");
        let validation_events = validation_payload
            .as_array()
            .expect("validation payload should be a batch");
        assert_eq!(validation_events.len(), 1);
        assert_eq!(validation_events[0]["event"], "$exception");
        assert_eq!(validation_events[0]["api_key"], "phc_test_project_key");
        assert_eq!(validation_events[0]["$distinct_id"], "jetstream-turbo");
        assert_eq!(validation_events[0]["properties"]["test_event"], true);

        let flush_payload: Value =
            serde_json::from_slice(&requests[1].body).expect("flush payload should be json");
        let flushed_events = flush_payload
            .as_array()
            .expect("flush payload should be a batch");
        assert_eq!(flushed_events.len(), 2);

        assert_exception_event(
            &flushed_events[0],
            "Internal",
            "Internal error: server failed",
            false,
            false,
            &[("component", "main"), ("operation", "server_run")],
        );
        assert_exception_event(
            &flushed_events[1],
            "Panic",
            "simulated panic",
            false,
            true,
            &[
                ("component", "runtime"),
                ("operation", "panic_hook"),
                ("panic_location", "src/main.rs:10:5"),
            ],
        );
    }

    fn assert_exception_event(
        event: &Value,
        error_type: &str,
        message: &str,
        is_retryable: bool,
        is_critical: bool,
        context_pairs: &[(&str, &str)],
    ) {
        assert_eq!(event["event"], "$exception");
        assert_eq!(event["$distinct_id"], "jetstream-turbo");
        assert_eq!(event["properties"]["$exception_type"], error_type);
        assert_eq!(event["properties"]["$exception_message"], message);
        assert_eq!(event["properties"]["is_retryable"], is_retryable);
        assert_eq!(event["properties"]["is_critical"], is_critical);

        for (key, value) in context_pairs {
            assert_eq!(event["properties"][*key], *value);
        }
    }
}
