use chrono::Utc;
use futures::{Stream, StreamExt};
use serde::Deserialize;
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
            let mut cumulative_count: u64 = 0;

            loop {
                info!(stream = ?stream_id, "Connecting to {}", url);

                match connect_async(&url).await {
                    Ok((ws_stream, _)) => {
                        info!(stream = ?stream_id, "Connected successfully");
                        let (_, mut read) = ws_stream.split();
                        let mut count: u64 = 0;
                        let mut last_send = Instant::now();
                        let update_interval = Duration::from_millis(100);
                        let mut last_delivery_latency_us: Option<u64> = None;

                        while let Some(msg_result) = read.next().await {
                            match msg_result {
                                Ok(Message::Text(text)) => {
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
                                Err(e) => {
                                    error!(stream = ?stream_id, "WebSocket error: {}", e);
                                    break;
                                }
                                _ => {}
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

                        let (_, mut read) = ws_stream.split();
                        let mut count: u64 = 0;
                        let mut last_send = Instant::now();
                        let update_interval = Duration::from_millis(100);
                        let mut last_delivery_latency_us: Option<u64> = None;

                        while let Some(msg_result) = read.next().await {
                            match msg_result {
                                Ok(Message::Text(text)) => {
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
                                Err(e) => {
                                    error!(stream = ?stream_id, "WebSocket error: {}", e);
                                    break;
                                }
                                _ => {}
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
