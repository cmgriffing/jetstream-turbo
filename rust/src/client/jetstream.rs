use crate::models::{errors::TurboError, jetstream::JetstreamMessage, TurboResult};
use futures::{Stream, StreamExt};
use std::pin::Pin;
use std::time::Duration;
use tokio::sync::mpsc;
use tokio::time::sleep;
use tokio_stream::wrappers::ReceiverStream;
use tokio_tungstenite::{connect_async, tungstenite::Message};
use tracing::{error, info, trace, warn};

pub trait MessageSource {
    fn stream_messages(
        &self,
    ) -> impl std::future::Future<
        Output = TurboResult<Pin<Box<dyn Stream<Item = TurboResult<JetstreamMessage>> + Send>>>,
    > + Send;
}

const DEFAULT_CHANNEL_CAPACITY: usize = 10_000;
const DROP_LOG_INTERVAL: Duration = Duration::from_secs(30);

#[derive(Debug)]
struct DropLogState {
    dropped_since_last_log: u64,
    dropped_total: u64,
    in_backpressure: bool,
}

impl DropLogState {
    fn new() -> Self {
        Self {
            dropped_since_last_log: 0,
            dropped_total: 0,
            in_backpressure: false,
        }
    }

    fn record_drop(&mut self) {
        self.dropped_since_last_log += 1;
        self.dropped_total += 1;
        self.in_backpressure = true;
    }

    fn take_snapshot(&mut self) -> Option<(u64, u64)> {
        if self.dropped_since_last_log == 0 {
            return None;
        }

        let dropped_since_last_log = self.dropped_since_last_log;
        let dropped_total = self.dropped_total;
        self.dropped_since_last_log = 0;

        Some((dropped_since_last_log, dropped_total))
    }

    fn mark_recovered(&mut self) -> Option<u64> {
        if !self.in_backpressure {
            return None;
        }

        self.in_backpressure = false;
        Some(self.dropped_total)
    }
}

pub struct JetstreamClient {
    endpoints: Vec<String>,
    wanted_collections: String,
    max_reconnect_attempts: u32,
    reconnect_delay: Duration,
    channel_capacity: usize,
}

impl JetstreamClient {
    pub fn new(endpoints: Vec<String>, wanted_collections: String) -> Self {
        Self {
            endpoints,
            wanted_collections,
            max_reconnect_attempts: 10,
            reconnect_delay: Duration::from_secs(5),
            channel_capacity: DEFAULT_CHANNEL_CAPACITY,
        }
    }

    pub fn with_defaults(endpoints: Vec<String>) -> Self {
        Self::new(endpoints, "app.bsky.feed.post".to_string())
    }

    pub fn with_channel_capacity(mut self, capacity: usize) -> Self {
        self.channel_capacity = capacity;
        self
    }

    pub fn parse_message(&self, text: &str) -> TurboResult<JetstreamMessage> {
        parse_message(text)
    }
}

impl MessageSource for JetstreamClient {
    async fn stream_messages(
        &self,
    ) -> TurboResult<Pin<Box<dyn Stream<Item = TurboResult<JetstreamMessage>> + Send>>> {
        let (tx, rx) = mpsc::channel(self.channel_capacity);

        // Start the connection loop
        let endpoints = self.endpoints.clone();
        let wanted_collections = self.wanted_collections.clone();
        let max_reconnect_attempts = self.max_reconnect_attempts;
        let reconnect_delay = self.reconnect_delay;

        tokio::spawn(async move {
            let mut current_endpoint = 0;
            let mut reconnect_attempts = 0;
            let mut drop_log_state = DropLogState::new();
            let mut drop_log_interval = tokio::time::interval(DROP_LOG_INTERVAL);

            drop_log_interval.tick().await;

            loop {
                let endpoint = &endpoints[current_endpoint];
                let url =
                    format!("wss://{endpoint}/subscribe?wantedCollections={wanted_collections}");

                info!("Connecting to Jetstream endpoint: {}", endpoint);

                match connect_async(&url).await {
                    Ok((ws_stream, _)) => {
                        info!("Successfully connected to {}", endpoint);
                        reconnect_attempts = 0; // Reset on successful connection

                        let (_, mut read) = ws_stream.split();

                        // Process messages
                        loop {
                            tokio::select! {
                                _ = drop_log_interval.tick() => {
                                    if let Some((dropped_since_last_log, dropped_total)) =
                                        drop_log_state.take_snapshot()
                                    {
                                        warn!(
                                            dropped_since_last_log,
                                            dropped_total,
                                            channel_capacity = tx.max_capacity(),
                                            endpoint,
                                            "Jetstream input channel saturated; dropping messages"
                                        );
                                    }
                                }
                                msg_result = read.next() => {
                                    let Some(msg_result) = msg_result else {
                                        break;
                                    };

                                    match msg_result {
                                Ok(Message::Text(text)) => {
                                    trace!("Received message: {}", text);
                                    match parse_message(&text) {
                                        Ok(message) => match tx.try_send(Ok(message)) {
                                            Ok(()) => {
                                                if let Some(dropped_total) =
                                                    drop_log_state.mark_recovered()
                                                {
                                                    info!(
                                                        dropped_total,
                                                        endpoint,
                                                        "Jetstream input channel recovered"
                                                    );
                                                }
                                            }
                                            Err(mpsc::error::TrySendError::Full(_)) => {
                                                drop_log_state.record_drop();
                                            }
                                            Err(mpsc::error::TrySendError::Closed(_)) => {
                                                info!("Receiver dropped, stopping stream");
                                                return;
                                            }
                                        },
                                        Err(e) => {
                                            warn!(
                                                "Failed to parse message: {:?}. Raw: {}",
                                                e,
                                                &text[..text.len().min(200)]
                                            );
                                            // Continue processing other messages
                                        }
                                    }
                                }
                                Ok(Message::Binary(_)) => {
                                    trace!("Received binary message (ignoring)");
                                }
                                Ok(Message::Ping(_)) => {
                                    trace!("Received ping");
                                }
                                Ok(Message::Pong(_)) => {
                                    trace!("Received pong");
                                }
                                Ok(Message::Close(_)) => {
                                    info!("WebSocket connection closed by server");
                                    break;
                                }
                                Ok(Message::Frame(_)) => {
                                    // Ignore raw frames
                                    trace!("Received raw frame (ignoring)");
                                }
                                Err(e) => {
                                    error!("WebSocket error: {}", e);
                                    break;
                                }
                            }
                                }
                            }
                        }
                    }
                    Err(e) => {
                        error!("Failed to connect to {}: {}", endpoint, e);

                        reconnect_attempts += 1;
                        if reconnect_attempts >= max_reconnect_attempts {
                            error!("Max reconnection attempts reached");
                            let err = Err(TurboError::WebSocketConnection(format!(
                                "Failed to connect after {max_reconnect_attempts} attempts"
                            )));
                            match tx.try_send(err) {
                                Ok(()) | Err(mpsc::error::TrySendError::Full(_)) => {}
                                Err(mpsc::error::TrySendError::Closed(_)) => return,
                            }
                            break;
                        }
                    }
                }

                // Try next endpoint or wait before retry
                current_endpoint = (current_endpoint + 1) % endpoints.len();
                if endpoints.len() == 1 {
                    info!(
                        "Waiting {} seconds before reconnection attempt",
                        reconnect_delay.as_secs()
                    );
                    sleep(reconnect_delay).await;
                } else {
                    sleep(Duration::from_secs(1)).await;
                }
            }
        });

        Ok(Box::pin(ReceiverStream::new(rx)))
    }
}

fn parse_message(text: &str) -> TurboResult<JetstreamMessage> {
    // Use simd-json for faster parsing (2-4x faster than serde_json)
    // simd-json requires mutable input and uses unsafe SIMD operations internally
    // The library handles safety internally through careful validation
    let mut text = text.to_string();
    let message: JetstreamMessage =
        unsafe { simd_json::from_str(&mut text).map_err(TurboError::JsonDeserialization)? };

    // Validate required fields
    if message.did.is_empty() {
        return Err(TurboError::InvalidMessage("DID is empty".to_string()));
    }

    Ok(message)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_jetstream_client_creation() {
        let endpoints = vec![
            "jetstream1.us-east.bsky.network".to_string(),
            "jetstream2.us-east.bsky.network".to_string(),
        ];

        let client = JetstreamClient::new(endpoints.clone(), "app.bsky.feed.post".to_string());
        assert_eq!(client.endpoints, endpoints);
        assert_eq!(client.wanted_collections, "app.bsky.feed.post");
    }

    #[test]
    fn test_jetstream_client_with_defaults() {
        let endpoints = vec!["jetstream1.us-east.bsky.network".to_string()];
        let client = JetstreamClient::with_defaults(endpoints);
        assert_eq!(client.wanted_collections, "app.bsky.feed.post");
    }

    #[test]
    fn test_message_parsing() {
        let client = JetstreamClient::with_defaults(vec!["test.bsky.network".to_string()]);

        let valid_json = r#"
        {
            "did": "did:plc:test",
            "seq": 12345,
            "time_us": 1640995200000000,
            "kind": "commit",
            "commit": {
                "rev": "test-rev",
                "operation": "create",
                "collection": "app.bsky.feed.post",
                "rkey": "test",
                "record": {
                    "uri": "at://did:plc:test/app.bsky.feed.post/test",
                    "cid": "bafyrei",
                    "author": "did:plc:test",
                    "type": "app.bsky.feed.post",
                    "createdAt": "2022-01-01T00:00:00.000Z",
                    "fields": {}
                }
            }
        }
        "#;

        let result = client.parse_message(valid_json);
        assert!(result.is_ok());

        let message = result.unwrap();
        assert_eq!(message.did, "did:plc:test");
        assert_eq!(message.seq, Some(12345));
    }

    #[test]
    fn test_invalid_message_parsing() {
        let client = JetstreamClient::with_defaults(vec!["test.bsky.network".to_string()]);

        let invalid_json = r#"{ "invalid": "json" }"#;
        let result = client.parse_message(invalid_json);
        assert!(result.is_err());

        let empty_did = r#"
        {
            "did": "",
            "seq": 12345,
            "time_us": 1640995200000000,
            "kind": "commit",
            "commit": {
                "operation": "create",
                "collection": "app.bsky.feed.post",
                "rkey": "test"
            }
        }
        "#;
        let result = client.parse_message(empty_did);
        assert!(result.is_err());
        assert!(matches!(result.unwrap_err(), TurboError::InvalidMessage(_)));
    }

    #[test]
    fn test_drop_log_state_tracks_drops_and_recovery() {
        let mut state = DropLogState::new();

        assert_eq!(state.take_snapshot(), None);
        assert_eq!(state.mark_recovered(), None);

        state.record_drop();
        state.record_drop();
        assert_eq!(state.take_snapshot(), Some((2, 2)));
        assert_eq!(state.take_snapshot(), None);
        assert_eq!(state.mark_recovered(), Some(2));
        assert_eq!(state.mark_recovered(), None);

        state.record_drop();
        assert_eq!(state.take_snapshot(), Some((1, 3)));
        assert_eq!(state.mark_recovered(), Some(3));
    }
}
