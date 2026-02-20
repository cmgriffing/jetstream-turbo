use futures::{Stream, StreamExt};
use std::time::Duration;
use tokio::sync::mpsc;
use tokio::time::sleep;
use tokio_tungstenite::{connect_async, tungstenite::Message};
use tokio_stream::wrappers::UnboundedReceiverStream;
use tracing::{debug, error, info, warn};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StreamId {
    A,
    B,
}

#[derive(Debug, Clone)]
pub struct StreamMessage {
    pub stream_id: StreamId,
    pub count: u64,
}

pub struct StreamClient {
    url: String,
    stream_id: StreamId,
    reconnect_delay: Duration,
}

impl StreamClient {
    pub fn new(url: String, stream_id: StreamId) -> Self {
        Self {
            url,
            stream_id,
            reconnect_delay: Duration::from_secs(5),
        }
    }

    pub fn stream_counts(&self) -> impl Stream<Item = StreamMessage> {
        let (tx, rx) = mpsc::unbounded_channel();
        let url = self.url.clone();
        let stream_id = self.stream_id;
        let reconnect_delay = self.reconnect_delay;

        tokio::spawn(async move {
            loop {
                info!(stream = ?stream_id, "Connecting to {}", url);
                
                match connect_async(&url).await {
                    Ok((ws_stream, _)) => {
                        info!(stream = ?stream_id, "Connected successfully");
                        let (_, mut read) = ws_stream.split();
                        let mut count: u64 = 0;

                        while let Some(msg_result) = read.next().await {
                            match msg_result {
                                Ok(Message::Text(_)) => {
                                    count += 1;
                                    if count % 1000 == 0 {
                                        if tx.send(StreamMessage { stream_id, count }).is_err() {
                                            debug!(stream = ?stream_id, "Receiver dropped");
                                            return;
                                        }
                                    }
                                }
                                Ok(Message::Close(_)) => {
                                    info!(stream = ?stream_id, "Connection closed by server");
                                    break;
                                }
                                Err(e) => {
                                    error!(stream = ?stream_id, "WebSocket error: {}", e);
                                    break;
                                }
                                _ => {}
                            }
                        }
                        
                        if tx.send(StreamMessage { stream_id, count }).is_err() {
                            return;
                        }
                    }
                    Err(e) => {
                        error!(stream = ?stream_id, "Connection failed: {}", e);
                    }
                }

                warn!(stream = ?stream_id, "Reconnecting in {:?}...", reconnect_delay);
                sleep(reconnect_delay).await;
            }
        });

        UnboundedReceiverStream::new(rx)
    }
}
