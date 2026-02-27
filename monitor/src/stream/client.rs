use futures::{Stream, StreamExt};
use std::time::{Duration, Instant};
use tokio::sync::mpsc;
use tokio::time::sleep;
use tokio_stream::wrappers::UnboundedReceiverStream;
use tokio_tungstenite::{connect_async, tungstenite::Message};
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

#[derive(Debug, Clone)]
pub struct ConnectionStatus {
    pub stream_id: StreamId,
    pub connected: bool,
    pub connected_at: Option<Instant>,
    pub latency_ms: Option<u64>,
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
                        let mut last_send = Instant::now();
                        let update_interval = Duration::from_millis(100);

                        while let Some(msg_result) = read.next().await {
                            match msg_result {
                                Ok(Message::Text(_)) => {
                                    count += 1;
                                    if last_send.elapsed() >= update_interval {
                                        if tx.send(StreamMessage { stream_id, count }).is_err() {
                                            debug!(stream = ?stream_id, "Receiver dropped");
                                            return;
                                        }
                                        last_send = Instant::now();
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

    pub fn stream_with_status(
        &self,
    ) -> (
        impl Stream<Item = StreamMessage>,
        impl Stream<Item = ConnectionStatus>,
    ) {
        let (tx_msg, rx_msg) = mpsc::unbounded_channel();
        let (tx_status, rx_status) = mpsc::unbounded_channel();
        let url = self.url.clone();
        let stream_id = self.stream_id;
        let reconnect_delay = self.reconnect_delay;

        tokio::spawn(async move {
            loop {
                info!(stream = ?stream_id, "Connecting to {}", url);
                let connect_start = Instant::now();

                match connect_async(&url).await {
                    Ok((ws_stream, _)) => {
                        let latency_ms = connect_start.elapsed().as_millis() as u64;
                        info!(stream = ?stream_id, "Connected successfully in {}ms", latency_ms);

                        let _ = tx_status.send(ConnectionStatus {
                            stream_id,
                            connected: true,
                            connected_at: Some(connect_start),
                            latency_ms: Some(latency_ms),
                        });

                        let (_, mut read) = ws_stream.split();
                        let mut count: u64 = 0;
                        let mut last_send = Instant::now();
                        let update_interval = Duration::from_millis(100);

                        while let Some(msg_result) = read.next().await {
                            match msg_result {
                                Ok(Message::Text(_)) => {
                                    count += 1;
                                    if last_send.elapsed() >= update_interval {
                                        if tx_msg.send(StreamMessage { stream_id, count }).is_err()
                                        {
                                            debug!(stream = ?stream_id, "Receiver dropped");
                                            return;
                                        }
                                        last_send = Instant::now();
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

                        if tx_msg.send(StreamMessage { stream_id, count }).is_err() {
                            return;
                        }
                    }
                    Err(e) => {
                        error!(stream = ?stream_id, "Connection failed: {}", e);
                    }
                }

                let _ = tx_status.send(ConnectionStatus {
                    stream_id,
                    connected: false,
                    connected_at: None,
                    latency_ms: None,
                });

                warn!(stream = ?stream_id, "Reconnecting in {:?}...", reconnect_delay);
                sleep(reconnect_delay).await;
            }
        });

        (
            UnboundedReceiverStream::new(rx_msg),
            UnboundedReceiverStream::new(rx_status),
        )
    }
}
