use chrono::Utc;
use futures::{SinkExt, Stream, StreamExt};
use serde::Deserialize;
use std::time::{Duration, Instant};
use tokio::sync::mpsc;
use tokio::time::sleep;
use tokio_stream::wrappers::UnboundedReceiverStream;
use tokio_tungstenite::{connect_async, tungstenite::Message};
use tracing::{debug, error, info, warn};

const DEFAULT_IDLE_TIMEOUT: Duration = Duration::from_secs(30);

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StreamId {
    A,
    B,
    Baseline1,
    Baseline2,
}

#[cfg(test)]
mod tests {
    use super::{ConnectionStatus, StreamClient, StreamId};
    use futures::{SinkExt, StreamExt};
    use std::time::Duration;
    use tokio::net::TcpListener;
    use tokio_tungstenite::{accept_async, tungstenite::Message};

    async fn next_status(
        statuses: &mut (impl futures::Stream<Item = ConnectionStatus> + Unpin),
    ) -> ConnectionStatus {
        tokio::time::timeout(Duration::from_secs(2), statuses.next())
            .await
            .expect("timed out waiting for status")
            .expect("status stream ended")
    }

    #[tokio::test]
    async fn stale_open_connection_is_marked_disconnected_after_idle_timeout() {
        let listener = TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind test websocket listener");
        let addr = listener.local_addr().expect("read listener address");

        tokio::spawn(async move {
            let (stream, _) = listener.accept().await.expect("accept test client");
            let mut websocket = accept_async(stream)
                .await
                .expect("accept websocket handshake");
            websocket
                .send(Message::Text(r#"{"time_us": 1}"#.into()))
                .await
                .expect("send first message");
            tokio::time::sleep(Duration::from_secs(5)).await;
        });

        let client = StreamClient::new(format!("ws://{}", addr), StreamId::A)
            .with_idle_timeout(Duration::from_millis(50));
        let (_messages, mut statuses) = client.stream_with_status();

        let connected = next_status(&mut statuses).await;
        assert_eq!(connected.stream_id, StreamId::A);
        assert!(connected.connected);

        let disconnected = next_status(&mut statuses).await;
        assert_eq!(disconnected.stream_id, StreamId::A);
        assert!(!disconnected.connected);
    }
}

#[derive(Deserialize)]
struct TimeOnly {
    time_us: Option<u64>,
}

fn extract_delivery_latency_us(text: &str) -> Option<u64> {
    let parsed: TimeOnly = serde_json::from_str(text).ok()?;
    let time_us = parsed.time_us?;
    let now_us = Utc::now().timestamp_micros() as u64;
    now_us.checked_sub(time_us)
}

#[derive(Debug, Clone)]
pub struct StreamMessage {
    pub stream_id: StreamId,
    pub count: u64,
    pub delivery_latency_us: Option<u64>,
}

#[derive(Debug, Clone)]
pub struct ConnectionStatus {
    pub stream_id: StreamId,
    pub connected: bool,
    pub connected_at: Option<Instant>,
    pub connect_time_ms: Option<u64>,
}

pub struct StreamClient {
    url: String,
    stream_id: StreamId,
    reconnect_delay: Duration,
    idle_timeout: Duration,
}

impl StreamClient {
    pub fn new(url: String, stream_id: StreamId) -> Self {
        Self {
            url,
            stream_id,
            reconnect_delay: Duration::from_secs(5),
            idle_timeout: DEFAULT_IDLE_TIMEOUT,
        }
    }

    pub fn with_idle_timeout(mut self, idle_timeout: Duration) -> Self {
        self.idle_timeout = idle_timeout;
        self
    }

    pub fn stream_counts(&self) -> impl Stream<Item = StreamMessage> {
        let (tx, rx) = mpsc::unbounded_channel();
        let url = self.url.clone();
        let stream_id = self.stream_id;
        let reconnect_delay = self.reconnect_delay;
        let idle_timeout = self.idle_timeout;

        tokio::spawn(async move {
            let mut cumulative_count: u64 = 0;

            loop {
                info!(stream = ?stream_id, "Connecting to {}", url);

                match connect_async(&url).await {
                    Ok((ws_stream, _)) => {
                        info!(stream = ?stream_id, "Connected successfully");
                        let (mut write, mut read) = ws_stream.split();
                        let mut count: u64 = 0;
                        let mut last_send = Instant::now();
                        let mut last_message = Instant::now();
                        let update_interval = Duration::from_millis(100);
                        let mut last_delivery_latency_us: Option<u64> = None;

                        while let Ok(Some(msg_result)) =
                            tokio::time::timeout(idle_timeout, read.next()).await
                        {
                            match msg_result {
                                Ok(Message::Text(text)) => {
                                    last_message = Instant::now();
                                    count += 1;
                                    last_delivery_latency_us = extract_delivery_latency_us(&text);
                                    if last_send.elapsed() >= update_interval {
                                        if tx
                                            .send(StreamMessage {
                                                stream_id,
                                                count: cumulative_count.saturating_add(count),
                                                delivery_latency_us: last_delivery_latency_us,
                                            })
                                            .is_err()
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
                                Ok(Message::Ping(payload)) => {
                                    if let Err(e) = write.send(Message::Pong(payload)).await {
                                        error!(stream = ?stream_id, "Failed to send WebSocket pong: {}", e);
                                        break;
                                    }
                                    if last_message.elapsed() >= idle_timeout {
                                        warn!(stream = ?stream_id, "No data messages received for {:?}; reconnecting", idle_timeout);
                                        break;
                                    }
                                }
                                Err(e) => {
                                    error!(stream = ?stream_id, "WebSocket error: {}", e);
                                    break;
                                }
                                _ => {
                                    if last_message.elapsed() >= idle_timeout {
                                        warn!(stream = ?stream_id, "No data messages received for {:?}; reconnecting", idle_timeout);
                                        break;
                                    }
                                }
                            }
                        }

                        cumulative_count = cumulative_count.saturating_add(count);

                        if tx
                            .send(StreamMessage {
                                stream_id,
                                count: cumulative_count,
                                delivery_latency_us: last_delivery_latency_us,
                            })
                            .is_err()
                        {
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
        let idle_timeout = self.idle_timeout;

        tokio::spawn(async move {
            let mut cumulative_count: u64 = 0;

            loop {
                info!(stream = ?stream_id, "Connecting to {}", url);
                let connect_start = Instant::now();

                match connect_async(&url).await {
                    Ok((ws_stream, _)) => {
                        let connect_time_ms = connect_start.elapsed().as_millis() as u64;
                        info!(stream = ?stream_id, "Connected successfully in {}ms", connect_time_ms);

                        let _ = tx_status.send(ConnectionStatus {
                            stream_id,
                            connected: true,
                            connected_at: Some(connect_start),
                            connect_time_ms: Some(connect_time_ms),
                        });

                        let (mut write, mut read) = ws_stream.split();
                        let mut count: u64 = 0;
                        let mut last_send = Instant::now();
                        let mut last_message = Instant::now();
                        let update_interval = Duration::from_millis(100);
                        let mut last_delivery_latency_us: Option<u64> = None;

                        while let Ok(Some(msg_result)) =
                            tokio::time::timeout(idle_timeout, read.next()).await
                        {
                            match msg_result {
                                Ok(Message::Text(text)) => {
                                    last_message = Instant::now();
                                    count += 1;
                                    last_delivery_latency_us = extract_delivery_latency_us(&text);
                                    if last_send.elapsed() >= update_interval {
                                        if tx_msg
                                            .send(StreamMessage {
                                                stream_id,
                                                count: cumulative_count.saturating_add(count),
                                                delivery_latency_us: last_delivery_latency_us,
                                            })
                                            .is_err()
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
                                Ok(Message::Ping(payload)) => {
                                    if let Err(e) = write.send(Message::Pong(payload)).await {
                                        error!(stream = ?stream_id, "Failed to send WebSocket pong: {}", e);
                                        break;
                                    }
                                    if last_message.elapsed() >= idle_timeout {
                                        warn!(stream = ?stream_id, "No data messages received for {:?}; reconnecting", idle_timeout);
                                        break;
                                    }
                                }
                                Err(e) => {
                                    error!(stream = ?stream_id, "WebSocket error: {}", e);
                                    break;
                                }
                                _ => {
                                    if last_message.elapsed() >= idle_timeout {
                                        warn!(stream = ?stream_id, "No data messages received for {:?}; reconnecting", idle_timeout);
                                        break;
                                    }
                                }
                            }
                        }

                        cumulative_count = cumulative_count.saturating_add(count);

                        if tx_msg
                            .send(StreamMessage {
                                stream_id,
                                count: cumulative_count,
                                delivery_latency_us: last_delivery_latency_us,
                            })
                            .is_err()
                        {
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
                    connect_time_ms: None,
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
