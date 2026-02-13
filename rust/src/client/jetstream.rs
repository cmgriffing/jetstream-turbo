use futures::{Stream, StreamExt};
use std::time::Duration;
use tokio::time::sleep;
use tokio_tungstenite::{connect_async, tungstenite::Message};
use tokio_stream::wrappers::UnboundedReceiverStream;
use tokio::sync::mpsc;
use tracing::{debug, error, info, warn};
use crate::models::{jetstream::JetstreamMessage, errors::TurboError, TurboResult};

pub struct JetstreamClient {
    endpoints: Vec<String>,
    wanted_collections: String,
    max_reconnect_attempts: u32,
    reconnect_delay: Duration,
}

impl JetstreamClient {
    pub fn new(endpoints: Vec<String>, wanted_collections: String) -> Self {
        Self {
            endpoints,
            wanted_collections,
            max_reconnect_attempts: 10,
            reconnect_delay: Duration::from_secs(5),
        }
    }

    pub fn with_defaults(endpoints: Vec<String>) -> Self {
        Self::new(endpoints, "app.bsky.feed.post".to_string())
    }

    pub async fn stream_messages(&self) -> TurboResult<impl Stream<Item = TurboResult<JetstreamMessage>>> {
        let (tx, rx) = mpsc::unbounded_channel();

        // Start the connection loop
        let endpoints = self.endpoints.clone();
        let wanted_collections = self.wanted_collections.clone();
        let max_reconnect_attempts = self.max_reconnect_attempts;
        let reconnect_delay = self.reconnect_delay;

        tokio::spawn(async move {
            let mut current_endpoint = 0;
            let mut reconnect_attempts = 0;

            loop {
                let endpoint = &endpoints[current_endpoint];
                let url = format!(
                    "wss://{endpoint}/subscribe?wantedCollections={wanted_collections}"
                );

                info!("Connecting to Jetstream endpoint: {}", endpoint);

                match connect_async(&url).await {
                    Ok((ws_stream, _)) => {
                        info!("Successfully connected to {}", endpoint);
                        reconnect_attempts = 0; // Reset on successful connection

                            let (_, mut read) = ws_stream.split();

                        // Process messages
                        while let Some(msg_result) = read.next().await {
                            match msg_result {
                                Ok(Message::Text(text)) => {
                                    debug!("Received message: {}", text);
                                    match parse_message(&text) {
                                        Ok(message) => {
                                            if tx.send(Ok(message)).is_err() {
                                                info!("Receiver dropped, stopping stream");
                                                return;
                                            }
                                        }
                                        Err(e) => {
                                            warn!("Failed to parse message: {:?}. Raw: {}", e, &text[..text.len().min(200)]);
                                            // Continue processing other messages
                                        }
                                    }
                                }
                                Ok(Message::Binary(_)) => {
                                    debug!("Received binary message (ignoring)");
                                }
                                Ok(Message::Ping(_)) => {
                                    debug!("Received ping");
                                }
                                Ok(Message::Pong(_)) => {
                                    debug!("Received pong");
                                }
                                Ok(Message::Close(_)) => {
                                    info!("WebSocket connection closed by server");
                                    break;
                                }
                                Ok(Message::Frame(_)) => {
                                    // Ignore raw frames
                                    debug!("Received raw frame (ignoring)");
                                }
                                Err(e) => {
                                    error!("WebSocket error: {}", e);
                                    break;
                                }
                            }
                        }
                    }
                    Err(e) => {
                        error!("Failed to connect to {}: {}", endpoint, e);

                        reconnect_attempts += 1;
                        if reconnect_attempts >= max_reconnect_attempts {
                            error!("Max reconnection attempts reached");
                            if tx.send(Err(TurboError::WebSocketConnection(format!(
                                "Failed to connect after {max_reconnect_attempts} attempts"
                            )))).is_err() {
                                return;
                            }
                            break;
                        }
                    }
                }

                // Try next endpoint or wait before retry
                current_endpoint = (current_endpoint + 1) % endpoints.len();
                if endpoints.len() == 1 {
                    info!("Waiting {} seconds before reconnection attempt", reconnect_delay.as_secs());
                    sleep(reconnect_delay).await;
                } else {
                    sleep(Duration::from_secs(1)).await;
                }
            }
        });

        Ok(UnboundedReceiverStream::new(rx))
    }

    pub fn parse_message(&self, text: &str) -> TurboResult<JetstreamMessage> {
        parse_message(text)
    }
}

fn parse_message(text: &str) -> TurboResult<JetstreamMessage> {
    let message: JetstreamMessage = serde_json::from_str(text)
        .map_err(TurboError::JsonSerialization)?;

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
            "commit": {
                "seq": 12345,
                "rebase": false,
                "time_us": 1640995200000000,
                "operation": {
                    "type": "create",
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
        }
        "#;
        
        let result = client.parse_message(valid_json);
        assert!(result.is_ok());
        
        let message = result.unwrap();
        assert_eq!(message.did, "did:plc:test");
        assert_eq!(message.seq, 12345);
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
            "commit": {
                "seq": 12345,
                "rebase": false,
                "time_us": 1640995200000000,
                "operation": {}
            }
        }
        "#;
        let result = client.parse_message(empty_did);
        assert!(result.is_err());
        assert!(matches!(result.unwrap_err(), TurboError::InvalidMessage(_)));
    }
}