use chrono::Utc;
use futures::{SinkExt, Stream, StreamExt};
use serde::{Deserialize, Serialize};
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::sync::mpsc;
use tokio::time::sleep;
use tokio_stream::wrappers::UnboundedReceiverStream;
use tokio_tungstenite::{connect_async, tungstenite::Message};
use tracing::{debug, error, info, warn};

use crate::diagnostics::{DiagnosticEvent, DiagnosticLogger};

const DEFAULT_IDLE_TIMEOUT: Duration = Duration::from_secs(30);

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum StreamId {
    A,
    B,
    Baseline1,
    Baseline2,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ReconnectReason {
    DataIdleTimeout,
    HandshakeFailure,
    SocketRead,
    SocketWrite,
    PeerClose,
    ConnectTimeout,
}

#[cfg(test)]
mod tests {
    use super::{observe_source_event, ConnectionStatus, ReconnectReason, StreamClient, StreamId};
    use futures::{SinkExt, StreamExt};
    use std::time::{Duration, Instant};
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

    #[test]
    fn parser_observes_raw_source_timestamp() {
        let observation =
            observe_source_event(r#"{"time_us":900}"#, 1_000).expect("raw timestamp should parse");
        assert_eq!(observation.source_time_us, 900);
        assert_eq!(observation.lag_us, 100);
        assert_eq!(observation.clock_skew_us, 0);
    }

    #[test]
    fn parser_observes_nested_enriched_source_timestamp() {
        let observation = observe_source_event(r#"{"message":{"time_us":875}}"#, 1_000)
            .expect("nested timestamp should parse");
        assert_eq!(observation.source_time_us, 875);
    }

    #[test]
    fn parser_keeps_timestamp_less_and_invalid_frames_uncovered() {
        assert_eq!(observe_source_event(r#"{"kind":"commit"}"#, 1_000), None);
        assert_eq!(observe_source_event("not-json", 1_000), None);
    }

    #[test]
    fn parser_clamps_future_event_lag_and_bounds_clock_skew() {
        let observation = observe_source_event(r#"{"time_us":999999999999}"#, 1_000)
            .expect("future timestamp should parse");
        assert_eq!(observation.lag_us, 0);
        assert_eq!(observation.clock_skew_us, super::MAX_REPORTED_CLOCK_SKEW_US);
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

        let delivering = next_status(&mut statuses).await;
        assert!(delivering.connected);
        assert!(delivering.delivery_available);

        let disconnected = next_status(&mut statuses).await;
        assert_eq!(disconnected.stream_id, StreamId::A);
        assert!(!disconnected.connected);
        assert_eq!(
            disconnected.reconnect_reason,
            Some(ReconnectReason::DataIdleTimeout)
        );
        assert!(disconnected.client_recovery);
    }

    #[tokio::test]
    async fn hanging_connection_times_out_within_configured_duration() {
        // Bind a TCP listener that accepts connections but never responds —
        // simulating a server where the TLS/WebSocket handshake stalls.
        let listener = TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind test listener");
        let addr = listener.local_addr().expect("read listener address");

        tokio::spawn(async move {
            // Accept the TCP connection but never send any data, so the
            // WebSocket handshake hangs indefinitely. Hold the stream open
            // so the client doesn't get a connection reset.
            let (_stream, _) = listener.accept().await.expect("accept");
            tokio::time::sleep(Duration::from_secs(10)).await;
        });

        let client = StreamClient::new(format!("ws://{}", addr), StreamId::A)
            .with_connect_timeout(Duration::from_millis(200));
        let (_messages, mut statuses) = client.stream_with_status();

        let start = Instant::now();
        let status = next_status(&mut statuses).await;
        let elapsed = start.elapsed();

        assert!(!status.connected);
        assert_eq!(
            status.reconnect_reason,
            Some(ReconnectReason::ConnectTimeout)
        );
        // Should time out well within 2 seconds (configured at 200ms).
        assert!(
            elapsed < Duration::from_secs(2),
            "took {:?} to time out — expected ~200ms",
            elapsed
        );
    }

    #[tokio::test]
    async fn replay_aware_fixture_counts_raw_nested_and_cursorless_frames() {
        let listener = TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind replay-aware websocket fixture");
        let addr = listener.local_addr().expect("read listener address");
        tokio::spawn(async move {
            let (stream, _) = listener.accept().await.expect("accept fixture client");
            let mut websocket = accept_async(stream).await.expect("accept websocket");
            tokio::time::sleep(Duration::from_millis(120)).await;
            websocket
                .send(Message::Text(r#"{"time_us":100}"#.into()))
                .await
                .expect("send raw frame");
            tokio::time::sleep(Duration::from_millis(120)).await;
            websocket
                .send(Message::Text(r#"{"message":{"time_us":50}}"#.into()))
                .await
                .expect("send enriched replay frame");
            tokio::time::sleep(Duration::from_millis(120)).await;
            websocket
                .send(Message::Text(r#"{"kind":"commit"}"#.into()))
                .await
                .expect("send cursorless frame");
            tokio::time::sleep(Duration::from_millis(120)).await;
        });

        let client = StreamClient::new(format!("ws://{addr}"), StreamId::A)
            .with_idle_timeout(Duration::from_secs(1));
        let mut messages = Box::pin(client.stream_counts());
        let raw = next_message(&mut messages).await;
        let nested = next_message(&mut messages).await;
        let cursorless = next_message(&mut messages).await;

        assert_eq!(raw.count, 1);
        assert_eq!(
            raw.source_event.map(|event| event.source_time_us),
            Some(100)
        );
        assert_eq!(nested.count, 2);
        assert_eq!(
            nested.source_event.map(|event| event.source_time_us),
            Some(50)
        );
        assert_eq!(cursorless.count, 3);
        assert_eq!(cursorless.source_event, None);
    }

    async fn next_message(
        messages: &mut (impl futures::Stream<Item = super::StreamMessage> + Unpin),
    ) -> super::StreamMessage {
        tokio::time::timeout(Duration::from_secs(2), messages.next())
            .await
            .expect("timed out waiting for fixture message")
            .expect("message stream ended")
    }
}

const MAX_REPORTED_CLOCK_SKEW_US: u64 = 24 * 60 * 60 * 1_000_000;

#[derive(Deserialize)]
struct TimestampEnvelope {
    time_us: Option<u64>,
    message: Option<NestedTimestamp>,
}

#[derive(Deserialize)]
struct NestedTimestamp {
    time_us: Option<u64>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct SourceEventObservation {
    pub source_time_us: u64,
    pub observed_at_us: u64,
    pub lag_us: u64,
    pub clock_skew_us: u64,
}

fn observe_source_event(text: &str, observed_at_us: u64) -> Option<SourceEventObservation> {
    let parsed: TimestampEnvelope = serde_json::from_str(text).ok()?;
    let source_time_us = parsed
        .time_us
        .or_else(|| parsed.message.and_then(|message| message.time_us))?;
    Some(SourceEventObservation {
        source_time_us,
        observed_at_us,
        lag_us: observed_at_us.saturating_sub(source_time_us),
        clock_skew_us: source_time_us
            .saturating_sub(observed_at_us)
            .min(MAX_REPORTED_CLOCK_SKEW_US),
    })
}

fn observe_source_event_now(text: &str) -> Option<SourceEventObservation> {
    let now_us = Utc::now().timestamp_micros().max(0) as u64;
    observe_source_event(text, now_us)
}

#[derive(Debug, Clone)]
pub struct StreamMessage {
    pub stream_id: StreamId,
    pub count: u64,
    pub delivery_latency_us: Option<u64>,
    pub source_event: Option<SourceEventObservation>,
}

#[derive(Debug, Clone)]
pub struct ConnectionStatus {
    pub stream_id: StreamId,
    pub connected: bool,
    pub connected_at: Option<Instant>,
    pub connect_time_ms: Option<u64>,
    pub delivery_available: bool,
    pub reconnect_reason: Option<ReconnectReason>,
    pub client_recovery: bool,
}

pub struct StreamClient {
    url: String,
    stream_id: StreamId,
    reconnect_delay: Duration,
    idle_timeout: Duration,
    connect_timeout: Duration,
    diagnostics: Option<Arc<DiagnosticLogger>>,
}

impl StreamClient {
    pub fn new(url: String, stream_id: StreamId) -> Self {
        Self {
            url,
            stream_id,
            reconnect_delay: Duration::from_secs(5),
            idle_timeout: DEFAULT_IDLE_TIMEOUT,
            connect_timeout: Duration::from_secs(15),
            diagnostics: None,
        }
    }

    pub fn with_idle_timeout(mut self, idle_timeout: Duration) -> Self {
        self.idle_timeout = idle_timeout;
        self
    }

    pub fn with_connect_timeout(mut self, connect_timeout: Duration) -> Self {
        self.connect_timeout = connect_timeout;
        self
    }

    pub fn with_diagnostics(mut self, diagnostics: Arc<DiagnosticLogger>) -> Self {
        self.diagnostics = Some(diagnostics);
        self
    }

    pub fn stream_counts(&self) -> impl Stream<Item = StreamMessage> {
        let (tx, rx) = mpsc::unbounded_channel();
        let url = self.url.clone();
        let stream_id = self.stream_id;
        let reconnect_delay = self.reconnect_delay;
        let idle_timeout = self.idle_timeout;
        let connect_timeout = self.connect_timeout;
        let diagnostics = self.diagnostics.clone();

        tokio::spawn(async move {
            let mut cumulative_count: u64 = 0;
            let mut attempt_number: u64 = 0;
            let mut disconnect_start: Option<Instant> = None;

            loop {
                info!(stream = ?stream_id, "Connecting to {}", url);
                let connect_start = Instant::now();

                match tokio::time::timeout(connect_timeout, connect_async(&url)).await {
                    Ok(Ok((ws_stream, _))) => {
                        let connect_time_ms = connect_start.elapsed().as_millis() as u64;
                        info!(stream = ?stream_id, "Connected successfully in {}ms", connect_time_ms);

                        if attempt_number > 0 {
                            let downtime =
                                disconnect_start.map(|s| s.elapsed().as_secs()).unwrap_or(0);
                            if let Some(ref diag) = diagnostics {
                                diag.log(&DiagnosticEvent::Recovered {
                                    stream_id,
                                    url: url.clone(),
                                    timestamp: Utc::now(),
                                    downtime_seconds: downtime,
                                    attempt_count: attempt_number,
                                });
                            }
                            attempt_number = 0;
                            disconnect_start = None;
                        }

                        let (mut write, mut read) = ws_stream.split();
                        let mut count: u64 = 0;
                        let mut last_send = Instant::now();
                        let mut last_message = Instant::now();
                        let update_interval = Duration::from_millis(100);
                        let mut last_source_event: Option<SourceEventObservation> = None;

                        while let Ok(Some(msg_result)) =
                            tokio::time::timeout(idle_timeout, read.next()).await
                        {
                            match msg_result {
                                Ok(Message::Text(text)) => {
                                    last_message = Instant::now();
                                    count += 1;
                                    last_source_event = observe_source_event_now(&text);
                                    if last_send.elapsed() >= update_interval {
                                        if tx
                                            .send(StreamMessage {
                                                stream_id,
                                                count: cumulative_count.saturating_add(count),
                                                delivery_latency_us: last_source_event
                                                    .map(|event| event.lag_us),
                                                source_event: last_source_event,
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
                                    if disconnect_start.is_none() {
                                        disconnect_start = Some(Instant::now());
                                    }
                                    if let Some(ref diag) = diagnostics {
                                        diag.log(&DiagnosticEvent::Disconnected {
                                            stream_id,
                                            url: url.clone(),
                                            timestamp: Utc::now(),
                                            reconnect_reason: "peer_close".to_string(),
                                        });
                                    }
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
                                    if disconnect_start.is_none() {
                                        disconnect_start = Some(Instant::now());
                                    }
                                    if let Some(ref diag) = diagnostics {
                                        diag.log(&DiagnosticEvent::Disconnected {
                                            stream_id,
                                            url: url.clone(),
                                            timestamp: Utc::now(),
                                            reconnect_reason: "socket_error".to_string(),
                                        });
                                    }
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
                                delivery_latency_us: last_source_event.map(|event| event.lag_us),
                                source_event: last_source_event,
                            })
                            .is_err()
                        {
                            return;
                        }
                    }
                    Ok(Err(e)) => {
                        error!(stream = ?stream_id, "Connection failed: {}", e);
                        attempt_number += 1;
                        if disconnect_start.is_none() {
                            disconnect_start = Some(Instant::now());
                        }
                        if let Some(ref diag) = diagnostics {
                            diag.log(&DiagnosticEvent::ConnectionAttemptFailed {
                                stream_id,
                                url: url.clone(),
                                timestamp: Utc::now(),
                                error_type: "connect_error".to_string(),
                                error_message: e.to_string(),
                                elapsed_ms: connect_start.elapsed().as_millis() as u64,
                                timeout_seconds: connect_timeout.as_secs(),
                                attempt_number,
                            });
                        }
                    }
                    Err(_elapsed) => {
                        error!(
                            stream = ?stream_id,
                            "Connection timed out after {:?}",
                            connect_timeout
                        );
                        attempt_number += 1;
                        if disconnect_start.is_none() {
                            disconnect_start = Some(Instant::now());
                        }
                        if let Some(ref diag) = diagnostics {
                            diag.log(&DiagnosticEvent::ConnectionAttemptFailed {
                                stream_id,
                                url: url.clone(),
                                timestamp: Utc::now(),
                                error_type: "timeout".to_string(),
                                error_message: format!(
                                    "connection timed out after {}s",
                                    connect_timeout.as_secs()
                                ),
                                elapsed_ms: connect_timeout.as_millis() as u64,
                                timeout_seconds: connect_timeout.as_secs(),
                                attempt_number,
                            });
                        }
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
        let connect_timeout = self.connect_timeout;
        let diagnostics = self.diagnostics.clone();

        tokio::spawn(async move {
            let mut cumulative_count: u64 = 0;
            let mut attempt_number: u64 = 0;
            let mut disconnect_start: Option<Instant> = None;

            loop {
                info!(stream = ?stream_id, "Connecting to {}", url);
                let connect_start = Instant::now();

                match tokio::time::timeout(connect_timeout, connect_async(&url)).await {
                    Ok(Ok((ws_stream, _))) => {
                        let connect_time_ms = connect_start.elapsed().as_millis() as u64;
                        info!(stream = ?stream_id, "Connected successfully in {}ms", connect_time_ms);

                        if attempt_number > 0 {
                            let downtime =
                                disconnect_start.map(|s| s.elapsed().as_secs()).unwrap_or(0);
                            if let Some(ref diag) = diagnostics {
                                diag.log(&DiagnosticEvent::Recovered {
                                    stream_id,
                                    url: url.clone(),
                                    timestamp: Utc::now(),
                                    downtime_seconds: downtime,
                                    attempt_count: attempt_number,
                                });
                            }
                            attempt_number = 0;
                            disconnect_start = None;
                        }

                        let _ = tx_status.send(ConnectionStatus {
                            stream_id,
                            connected: true,
                            connected_at: Some(connect_start),
                            connect_time_ms: Some(connect_time_ms),
                            delivery_available: false,
                            reconnect_reason: None,
                            client_recovery: false,
                        });

                        let (mut write, mut read) = ws_stream.split();
                        let mut count: u64 = 0;
                        let mut last_send = Instant::now();
                        let mut last_message = Instant::now();
                        let update_interval = Duration::from_millis(100);
                        let mut last_source_event: Option<SourceEventObservation> = None;
                        let mut delivery_available = false;
                        let mut reconnect_reason = ReconnectReason::DataIdleTimeout;

                        while let Ok(Some(msg_result)) =
                            tokio::time::timeout(idle_timeout, read.next()).await
                        {
                            match msg_result {
                                Ok(Message::Text(text)) => {
                                    last_message = Instant::now();
                                    count += 1;
                                    last_source_event = observe_source_event_now(&text);
                                    if !delivery_available {
                                        delivery_available = true;
                                        let _ = tx_status.send(ConnectionStatus {
                                            stream_id,
                                            connected: true,
                                            connected_at: Some(connect_start),
                                            connect_time_ms: None,
                                            delivery_available: true,
                                            reconnect_reason: None,
                                            client_recovery: false,
                                        });
                                    }
                                    if last_send.elapsed() >= update_interval {
                                        if tx_msg
                                            .send(StreamMessage {
                                                stream_id,
                                                count: cumulative_count.saturating_add(count),
                                                delivery_latency_us: last_source_event
                                                    .map(|event| event.lag_us),
                                                source_event: last_source_event,
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
                                    reconnect_reason = ReconnectReason::PeerClose;
                                    info!(stream = ?stream_id, "Connection closed by server");
                                    if disconnect_start.is_none() {
                                        disconnect_start = Some(Instant::now());
                                    }
                                    if let Some(ref diag) = diagnostics {
                                        diag.log(&DiagnosticEvent::Disconnected {
                                            stream_id,
                                            url: url.clone(),
                                            timestamp: Utc::now(),
                                            reconnect_reason: "peer_close".to_string(),
                                        });
                                    }
                                    break;
                                }
                                Ok(Message::Ping(payload)) => {
                                    if let Err(e) = write.send(Message::Pong(payload)).await {
                                        reconnect_reason = ReconnectReason::SocketWrite;
                                        error!(stream = ?stream_id, "Failed to send WebSocket pong: {}", e);
                                        if disconnect_start.is_none() {
                                            disconnect_start = Some(Instant::now());
                                        }
                                        if let Some(ref diag) = diagnostics {
                                            diag.log(&DiagnosticEvent::Disconnected {
                                                stream_id,
                                                url: url.clone(),
                                                timestamp: Utc::now(),
                                                reconnect_reason: "socket_write".to_string(),
                                            });
                                        }
                                        break;
                                    }
                                    if last_message.elapsed() >= idle_timeout {
                                        warn!(stream = ?stream_id, "No data messages received for {:?}; reconnecting", idle_timeout);
                                        if disconnect_start.is_none() {
                                            disconnect_start = Some(Instant::now());
                                        }
                                        if let Some(ref diag) = diagnostics {
                                            diag.log(&DiagnosticEvent::Disconnected {
                                                stream_id,
                                                url: url.clone(),
                                                timestamp: Utc::now(),
                                                reconnect_reason: "data_idle_timeout".to_string(),
                                            });
                                        }
                                        break;
                                    }
                                }
                                Err(e) => {
                                    reconnect_reason = ReconnectReason::SocketRead;
                                    error!(stream = ?stream_id, "WebSocket error: {}", e);
                                    if disconnect_start.is_none() {
                                        disconnect_start = Some(Instant::now());
                                    }
                                    if let Some(ref diag) = diagnostics {
                                        diag.log(&DiagnosticEvent::Disconnected {
                                            stream_id,
                                            url: url.clone(),
                                            timestamp: Utc::now(),
                                            reconnect_reason: "socket_error".to_string(),
                                        });
                                    }
                                    break;
                                }
                                _ => {
                                    if last_message.elapsed() >= idle_timeout {
                                        warn!(stream = ?stream_id, "No data messages received for {:?}; reconnecting", idle_timeout);
                                        if disconnect_start.is_none() {
                                            disconnect_start = Some(Instant::now());
                                        }
                                        if let Some(ref diag) = diagnostics {
                                            diag.log(&DiagnosticEvent::Disconnected {
                                                stream_id,
                                                url: url.clone(),
                                                timestamp: Utc::now(),
                                                reconnect_reason: "data_idle_timeout".to_string(),
                                            });
                                        }
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
                                delivery_latency_us: last_source_event.map(|event| event.lag_us),
                                source_event: last_source_event,
                            })
                            .is_err()
                        {
                            return;
                        }

                        let _ = tx_status.send(ConnectionStatus {
                            stream_id,
                            connected: false,
                            connected_at: None,
                            connect_time_ms: None,
                            delivery_available: false,
                            reconnect_reason: Some(reconnect_reason),
                            client_recovery: reconnect_reason == ReconnectReason::DataIdleTimeout,
                        });
                    }
                    Ok(Err(e)) => {
                        error!(stream = ?stream_id, "Connection failed: {}", e);
                        attempt_number += 1;
                        if disconnect_start.is_none() {
                            disconnect_start = Some(Instant::now());
                        }
                        if let Some(ref diag) = diagnostics {
                            diag.log(&DiagnosticEvent::ConnectionAttemptFailed {
                                stream_id,
                                url: url.clone(),
                                timestamp: Utc::now(),
                                error_type: "connect_error".to_string(),
                                error_message: e.to_string(),
                                elapsed_ms: connect_start.elapsed().as_millis() as u64,
                                timeout_seconds: connect_timeout.as_secs(),
                                attempt_number,
                            });
                        }
                        let _ = tx_status.send(ConnectionStatus {
                            stream_id,
                            connected: false,
                            connected_at: None,
                            connect_time_ms: None,
                            delivery_available: false,
                            reconnect_reason: Some(ReconnectReason::HandshakeFailure),
                            client_recovery: false,
                        });
                    }
                    Err(_elapsed) => {
                        error!(
                            stream = ?stream_id,
                            "Connection timed out after {:?}",
                            connect_timeout
                        );
                        attempt_number += 1;
                        if disconnect_start.is_none() {
                            disconnect_start = Some(Instant::now());
                        }
                        if let Some(ref diag) = diagnostics {
                            diag.log(&DiagnosticEvent::ConnectionAttemptFailed {
                                stream_id,
                                url: url.clone(),
                                timestamp: Utc::now(),
                                error_type: "timeout".to_string(),
                                error_message: format!(
                                    "connection timed out after {}s",
                                    connect_timeout.as_secs()
                                ),
                                elapsed_ms: connect_timeout.as_millis() as u64,
                                timeout_seconds: connect_timeout.as_secs(),
                                attempt_number,
                            });
                        }
                        let _ = tx_status.send(ConnectionStatus {
                            stream_id,
                            connected: false,
                            connected_at: None,
                            connect_time_ms: None,
                            delivery_available: false,
                            reconnect_reason: Some(ReconnectReason::ConnectTimeout),
                            client_recovery: false,
                        });
                    }
                }

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
