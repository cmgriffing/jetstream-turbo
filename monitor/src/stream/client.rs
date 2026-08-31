use chrono::Utc;
use futures::{SinkExt, Stream, StreamExt};
use serde::{Deserialize, Serialize};
use std::time::{Duration, Instant};
use tokio::sync::mpsc;
use tokio_stream::wrappers::UnboundedReceiverStream;
use tokio_tungstenite::{connect_async, tungstenite::Message};
use tracing::{debug, info, warn};

use crate::incidents::{HandshakeFailureReason, TransportLossReason};

use super::transition::{StreamEvent, StreamTransition};

const MAX_REPORTED_CLOCK_SKEW_US: u64 = 24 * 60 * 60 * 1_000_000;
const DEFAULT_IDLE_TIMEOUT: Duration = Duration::from_secs(30);
const DEFAULT_HEARTBEAT_INTERVAL: Duration = Duration::from_secs(20);
const DEFAULT_LIVENESS_DEADLINE: Duration = Duration::from_secs(60);
const DEFAULT_CONNECT_TIMEOUT: Duration = Duration::from_secs(15);
const DEFAULT_BACKOFF_MIN: Duration = Duration::from_secs(1);
const DEFAULT_BACKOFF_MAX: Duration = Duration::from_secs(30);
const RECORD_UPDATE_INTERVAL: Duration = Duration::from_millis(100);
/// Random jitter factor applied to each backoff delay (delay * jitter in [0.8, 1.2]).
const BACKOFF_JITTER: f64 = 0.2;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum StreamId {
    A,
    B,
    Baseline1,
    Baseline2,
}

/// Legacy presentation-level reconnect reason retained for dashboard fields.
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

impl ReconnectReason {
    pub fn as_str(self) -> &'static str {
        match self {
            ReconnectReason::DataIdleTimeout => "data_idle_timeout",
            ReconnectReason::HandshakeFailure => "handshake_failure",
            ReconnectReason::SocketRead => "socket_read",
            ReconnectReason::SocketWrite => "socket_write",
            ReconnectReason::PeerClose => "peer_close",
            ReconnectReason::ConnectTimeout => "connect_timeout",
        }
    }
}

/// Legacy per-connection status retained for dashboard compatibility during migration.
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

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SourceEventObservation {
    pub source_time_us: u64,
    pub observed_at_us: u64,
    pub lag_us: u64,
    pub clock_skew_us: u64,
    pub source_event_id: Option<String>,
}

fn observe_source_event(text: &str, observed_at_us: u64) -> Option<SourceEventObservation> {
    let parsed: serde_json::Value = serde_json::from_str(text).ok()?;
    let source = parsed.get("message").unwrap_or(&parsed);
    let source_time_us = source.get("time_us")?.as_u64()?;
    Some(SourceEventObservation {
        source_time_us,
        observed_at_us,
        lag_us: observed_at_us.saturating_sub(source_time_us),
        clock_skew_us: source_time_us
            .saturating_sub(observed_at_us)
            .min(MAX_REPORTED_CLOCK_SKEW_US),
        source_event_id: portable_source_identity(source),
    })
}

fn portable_source_identity(source: &serde_json::Value) -> Option<String> {
    let did = source.get("did")?.as_str()?;
    let kind = source.get("kind")?.as_str()?;
    let time_us = source.get("time_us")?.as_u64()?.to_string();
    let mut identity = String::from("v1");
    push_identity_component(&mut identity, did);
    push_identity_component(&mut identity, kind);
    push_identity_component(&mut identity, &time_us);
    if kind == "commit" {
        let commit = source.get("commit")?.as_object()?;
        for field in ["rev", "operation", "collection", "rkey", "cid"] {
            push_identity_component(
                &mut identity,
                commit
                    .get(field)
                    .and_then(|value| value.as_str())
                    .unwrap_or_default(),
            );
        }
    }
    Some(identity)
}

fn push_identity_component(identity: &mut String, component: &str) {
    identity.push('|');
    identity.push_str(&component.len().to_string());
    identity.push_str(&component);
}

fn observe_source_event_now(text: &str) -> Option<SourceEventObservation> {
    let now_us = Utc::now().timestamp_micros().max(0) as u64;
    observe_source_event(text, now_us)
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StreamMessage {
    pub stream_id: StreamId,
    pub count: u64,
    pub delivery_latency_us: Option<u64>,
    pub source_event: Option<SourceEventObservation>,
}

/// Bounded exponential backoff schedule with jitter.
#[derive(Debug, Clone, Copy)]
pub struct BackoffPolicy {
    min: Duration,
    max: Duration,
}

impl Default for BackoffPolicy {
    fn default() -> Self {
        Self {
            min: DEFAULT_BACKOFF_MIN,
            max: DEFAULT_BACKOFF_MAX,
        }
    }
}

impl BackoffPolicy {
    pub fn new(min: Duration, max: Duration) -> Self {
        Self {
            min,
            max: max.max(min),
        }
    }

    /// Delay for the given 1-based attempt ordinal with random jitter.
    pub fn delay(&self, ordinal: u64) -> Duration {
        let exponent = ordinal.saturating_sub(1).min(16);
        let base = self.min.saturating_mul(1u32 << exponent);
        let capped = base.min(self.max);
        let jitter = 1.0 - BACKOFF_JITTER + 2.0 * BACKOFF_JITTER * rand::random::<f64>();
        let jittered = capped.as_secs_f64() * jitter.clamp(0.8, 1.2);
        Duration::from_secs_f64(jittered.max(0.0))
    }
}

/// Why an established WebSocket session ended.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum SessionEnd {
    PeerClose,
    SocketRead,
    SocketWrite,
    LivenessDeadline,
}

type WsStream = tokio_tungstenite::WebSocketStream<
    tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>,
>;

fn map_loss_reason(end: SessionEnd) -> TransportLossReason {
    match end {
        SessionEnd::PeerClose => TransportLossReason::PeerClose,
        SessionEnd::SocketRead => TransportLossReason::SocketError,
        SessionEnd::SocketWrite => TransportLossReason::SocketWrite,
        SessionEnd::LivenessDeadline => TransportLossReason::LivenessDeadline,
    }
}

/// Run a single connected session until the transport ends.
///
/// Sends proactive Ping frames on the heartbeat cadence, treats any peer
/// frame as liveness evidence, and emits delivery-idle transitions without
/// closing a heartbeat-responsive socket. Returns the session end reason
/// together with updated counters.
#[allow(clippy::too_many_arguments)]
async fn run_session(
    ws: WsStream,
    stream_id: StreamId,
    idle_timeout: Duration,
    heartbeat_interval: Duration,
    liveness_deadline: Duration,
    tx: &mpsc::UnboundedSender<StreamEvent>,
    cumulative_count: &mut u64,
    last_source_event: &mut Option<SourceEventObservation>,
    delivery_open: &mut bool,
    idle_emitted: &mut bool,
    last_useful_record: &mut Option<Instant>,
) -> SessionEnd {
    use tokio::time::sleep_until;

    let (mut write, mut read) = ws.split();
    let mut last_batch = Instant::now();
    let mut last_peer: Instant = Instant::now();
    let mut ping_at: Instant = Instant::now() + heartbeat_interval;

    let end = loop {
        let now = Instant::now();
        let liveness_at = last_peer + liveness_deadline;
        let idle_at = if *delivery_open && !*idle_emitted {
            last_useful_record.unwrap_or(now) + idle_timeout
        } else {
            now + Duration::from_secs(3600)
        };
        let next_wake = ping_at.min(liveness_at).min(idle_at);

        tokio::select! {
            incoming = read.next() => {
                let Some(msg_result) = incoming else {
                    break SessionEnd::SocketRead;
                };
                match msg_result {
                    Ok(Message::Text(text)) => {
                        last_peer = Instant::now();
                        *last_useful_record = Some(Instant::now());
                        *last_source_event = observe_source_event_now(&text);
                        *cumulative_count = cumulative_count.saturating_add(1);
                        if !*delivery_open {
                            *delivery_open = true;
                            *idle_emitted = false;
                            emit_transition(tx, StreamTransition::DeliveryResumed, stream_id);
                        }
                        if last_batch.elapsed() >= RECORD_UPDATE_INTERVAL {
                            if tx.send(StreamEvent::Record(StreamMessage {
                                stream_id,
                                count: *cumulative_count,
                                delivery_latency_us: last_source_event.as_ref().map(|e| e.lag_us),
                                source_event: last_source_event.clone(),
                            }))
                            .is_err()
                            {
                                debug!(stream = ?stream_id, "receiver dropped");
                                return SessionEnd::SocketRead;
                            }
                            last_batch = Instant::now();
                        }
                    }
                    Ok(Message::Close(_)) => {
                        info!(stream = ?stream_id, "connection closed by server");
                        break SessionEnd::PeerClose;
                    }
                    Ok(Message::Ping(payload)) => {
                        last_peer = Instant::now();
                        if let Err(e) = write.send(Message::Pong(payload)).await {
                            warn!(stream = ?stream_id, "failed to send WebSocket pong: {}", e);
                            break SessionEnd::SocketWrite;
                        }
                    }
                    Ok(Message::Pong(_)) => {
                        last_peer = Instant::now();
                    }
                    Ok(_) => {
                        last_peer = Instant::now();
                    }
                    Err(e) => {
                        warn!(stream = ?stream_id, "WebSocket error: {}", e);
                        break SessionEnd::SocketRead;
                    }
                }
            }
            _ = sleep_until(tokio::time::Instant::from_std(next_wake)) => {
                let now = Instant::now();
                if now >= liveness_at {
                    warn!(stream = ?stream_id, "transport liveness deadline elapsed without peer evidence");
                    break SessionEnd::LivenessDeadline;
                }
                if now >= idle_at {
                    let silence_ms = last_useful_record
                        .map(|t| now.duration_since(t).as_millis() as u64)
                        .unwrap_or(0);
                    *idle_emitted = true;
                    emit_transition(
                        tx,
                        StreamTransition::DeliveryIdle { silence_ms },
                        stream_id,
                    );
                }
                if now >= ping_at {
                    ping_at = now + heartbeat_interval;
                    if let Err(e) = write.send(Message::Ping(vec![])).await {
                        warn!(stream = ?stream_id, "failed to send WebSocket ping: {}", e);
                        break SessionEnd::SocketWrite;
                    }
                }
            }
        }
    };

    // Flush the final cumulative count so consumers see the end-of-session total.
    let _ = tx.send(StreamEvent::Record(StreamMessage {
        stream_id,
        count: *cumulative_count,
        delivery_latency_us: last_source_event.as_ref().map(|e| e.lag_us),
        source_event: last_source_event.clone(),
    }));

    *delivery_open = false;
    end
}

fn emit_transition(
    tx: &mpsc::UnboundedSender<StreamEvent>,
    transition: StreamTransition,
    stream_id: StreamId,
) {
    if tx.send(StreamEvent::Transition(transition)).is_err() {
        debug!(stream = ?stream_id, "receiver dropped for transition");
    }
}

/// WebSocket stream client that separates transport liveness from useful delivery.
///
/// It emits one ordered `StreamEvent` sequence per stream: low-rate state
/// transitions share the channel with record batches, so every consumer
/// observes exactly one ordering authority.
pub struct StreamClient {
    url: String,
    stream_id: StreamId,
    idle_timeout: Duration,
    heartbeat_interval: Duration,
    liveness_deadline: Duration,
    connect_timeout: Duration,
    backoff: BackoffPolicy,
}

impl StreamClient {
    pub fn new(url: String, stream_id: StreamId) -> Self {
        Self {
            url,
            stream_id,
            idle_timeout: DEFAULT_IDLE_TIMEOUT,
            heartbeat_interval: DEFAULT_HEARTBEAT_INTERVAL,
            liveness_deadline: DEFAULT_LIVENESS_DEADLINE,
            connect_timeout: DEFAULT_CONNECT_TIMEOUT,
            backoff: BackoffPolicy::default(),
        }
    }

    pub fn with_idle_timeout(mut self, idle_timeout: Duration) -> Self {
        self.idle_timeout = idle_timeout;
        self
    }

    pub fn with_heartbeat_interval(mut self, heartbeat_interval: Duration) -> Self {
        self.heartbeat_interval = heartbeat_interval;
        self
    }

    pub fn with_liveness_deadline(mut self, liveness_deadline: Duration) -> Self {
        self.liveness_deadline = liveness_deadline;
        self
    }

    pub fn with_connect_timeout(mut self, connect_timeout: Duration) -> Self {
        self.connect_timeout = connect_timeout;
        self
    }

    pub fn with_backoff_policy(mut self, backoff: BackoffPolicy) -> Self {
        self.backoff = backoff;
        self
    }

    /// Run the ordered event stream for this client.
    pub fn stream(&self) -> impl Stream<Item = StreamEvent> {
        let (tx, rx) = mpsc::unbounded_channel();
        let url = self.url.clone();
        let stream_id = self.stream_id;
        let idle_timeout = self.idle_timeout;
        let heartbeat_interval = self.heartbeat_interval;
        let liveness_deadline = self.liveness_deadline;
        let connect_timeout = self.connect_timeout;
        let backoff = self.backoff;

        tokio::spawn(async move {
            let send = |event: StreamEvent| {
                if tx.send(event).is_err() {
                    debug!(stream = ?stream_id, "receiver dropped");
                }
            };

            let mut cumulative_count: u64 = 0;
            let mut last_source_event: Option<SourceEventObservation> = None;
            // 1-based ordinal of the connection attempt the loop is about to
            // perform; counts failed attempts during an outage or start-up.
            let mut attempt_number: u64 = 0;
            // Original outage boundary preserved across reconnect attempts.
            let mut outage_started: Option<Instant> = None;
            let mut ever_connected = false;
            #[allow(unused_assignments)]
            let mut delivery_open = false;
            #[allow(unused_assignments)]
            let mut idle_emitted = false;
            let mut last_useful_record: Option<Instant> = None;
            let mut pending_delay: Option<Duration> = None;

            loop {
                if let Some(delay) = pending_delay.take() {
                    tokio::time::sleep(delay).await;
                }
                info!(stream = ?stream_id, "connecting to stream endpoint");
                let connect_start = Instant::now();

                let handshake =
                    tokio::time::timeout(connect_timeout, connect_async(&url)).await;

                match handshake {
                    Ok(Ok((ws_stream, _))) => {
                        let connect_time_ms = connect_start.elapsed().as_millis() as u64;
                        ever_connected = true;
                        // Backoff state resets after successful transport recovery.
                        #[allow(unused_assignments)]
                        {
                            attempt_number = 0;
                            pending_delay = None;
                        }
                        outage_started = None;
                        send(StreamEvent::Transition(StreamTransition::HandshakeSucceeded {
                            connect_time_ms,
                        }));

                        let end = run_session(
                            ws_stream,
                            stream_id,
                            idle_timeout,
                            heartbeat_interval,
                            liveness_deadline,
                            &tx,
                            &mut cumulative_count,
                            &mut last_source_event,
                            &mut delivery_open,
                            &mut idle_emitted,
                            &mut last_useful_record,
                        )
                        .await;

                        // True transport loss: preserve the original outage boundary.
                        outage_started.get_or_insert(Instant::now());
                        let outage_elapsed_ms = outage_started
                            .map(|s| s.elapsed().as_millis() as u64)
                            .unwrap_or(0);
                        send(StreamEvent::Transition(StreamTransition::TransportLost {
                            reason: map_loss_reason(end),
                            outage_elapsed_ms: Some(outage_elapsed_ms),
                        }));
                        attempt_number = 1;
                        pending_delay = Some(backoff.delay(1));
                    }
                    outcome @ (Ok(Err(_)) | Err(_)) => {
                        let reason = if outcome.is_err() {
                            warn!(
                                stream = ?stream_id,
                                "connection timed out after {:?}",
                                connect_timeout
                            );
                            HandshakeFailureReason::ConnectTimeout
                        } else {
                            HandshakeFailureReason::ConnectError
                        };
                        attempt_number = attempt_number.saturating_add(1);
                        // A first startup failure does not open a transport
                        // outage: coverage remains unknown until the first
                        // successful handshake.
                        if ever_connected {
                            outage_started.get_or_insert(Instant::now());
                        }
                        let ordinal = attempt_number;
                        let scheduled_delay_ms = backoff.delay(ordinal).as_millis() as u64;
                        send(StreamEvent::Transition(
                            StreamTransition::ReconnectAttemptFailed {
                                ordinal,
                                reason,
                                scheduled_delay_ms,
                            },
                        ));
                        pending_delay = Some(Duration::from_millis(scheduled_delay_ms));
                    }
                }
            }
        });

        UnboundedReceiverStream::new(rx)
    }
}
#[cfg(test)]
mod fixtures {
    use super::{StreamClient, StreamId, StreamEvent, StreamTransition};
    use futures::SinkExt;
    use std::time::Duration;
    use tokio::net::TcpListener;
    use tokio_tungstenite::tungstenite::Message;

    async fn next_transition(
        stream: &mut (impl futures::Stream<Item = StreamEvent> + Unpin),
    ) -> StreamTransition {
        let deadline = std::time::Duration::from_secs(3);
        loop {
            let event = tokio::time::timeout(deadline, futures::StreamExt::next(stream))
                .await
                .expect("timeout waiting for stream event")
                .expect("stream ended");
            if let StreamEvent::Transition(transition) = event {
                return transition;
            }
        }
    }

    /// Assert no transport-lost transition arrives within `quiet` millis.
    async fn expect_quiet_transport(stream: &mut (impl futures::Stream<Item = StreamEvent> + Unpin), quiet: u64) {
        let deadline = std::time::Instant::now() + std::time::Duration::from_millis(quiet);
        let mut saw_handshake = false;
        while std::time::Instant::now() < deadline {
            match tokio::time::timeout(deadline - std::time::Instant::now(), futures::StreamExt::next(stream)).await {
                Err(_) => return,
                Ok(Some(StreamEvent::Transition(StreamTransition::HandshakeSucceeded { .. }))) if !saw_handshake => {
                    saw_handshake = true;
                }
                Ok(Some(StreamEvent::Transition(
                    StreamTransition::DeliveryResumed,
                ))) => {}
                Ok(Some(StreamEvent::Transition(StreamTransition::DeliveryIdle { .. }))) => {}
                Ok(Some(other)) => panic!("unexpected event during quiet window: {other:?}"),
                Ok(None) => panic!("stream ended during quiet window"),
            }
        }
        assert!(saw_handshake, "expected a handshake before the quiet window");
    }

    async fn bind_listener() -> (TcpListener, String) {
        let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind fixture");
        let addr = listener.local_addr().expect("addr").to_string();
        (listener, format!("ws://{addr}"))
    }

    #[tokio::test]
    async fn heartbeat_responsive_socket_surges_through_data_idle_without_reconnect() {
        let (listener, url) = bind_listener().await;
        tokio::spawn(async move {
            let (stream, _) = listener.accept().await.expect("accept");
            let mut ws: tokio_tungstenite::WebSocketStream<tokio::net::TcpStream> =
                tokio_tungstenite::accept_async(stream).await.expect("handshake");
            // Deliver one useful record then go quiet; pings are auto-ponged
            // by tungstenite when reading.
            ws.send(Message::Text(r#"{"time_us":100}"#.into())).await.ok();
            // Keep responding to pings for the remainder of the fixture window.
            while let Ok(Some(_)) = tokio::time::timeout(
                std::time::Duration::from_secs(10),
                futures::StreamExt::next(&mut ws),
            )
            .await
            {}
        });

        let client = StreamClient::new(url, StreamId::A)
            .with_idle_timeout(std::time::Duration::from_millis(80))
            .with_heartbeat_interval(std::time::Duration::from_millis(50))
            .with_liveness_deadline(std::time::Duration::from_millis(400));
        let mut events = Box::pin(client.stream());

        let resumed = next_transition(&mut events).await;
        assert!(matches!(
            resumed,
            StreamTransition::DeliveryResumed
                | StreamTransition::HandshakeSucceeded { .. }
        ));
        // Drain the handshake/idle sequence; the socket must remain connected:
        // an idle transition may arrive, but no transport loss, and no second
        // handshake (which would indicate a reconnect).
        expect_quiet_transport(&mut events, 250).await;
    }

    #[tokio::test]
    async fn ignored_heartbeats_declare_transport_loss_after_liveness_deadline() {
        let (listener, url) = bind_listener().await;
        tokio::spawn(async move {
            let (stream, _) = listener.accept().await.expect("accept fixture");
            // Accept the handshake but never read or respond to pings.
            let _ws: tokio_tungstenite::WebSocketStream<tokio::net::TcpStream> =
                tokio_tungstenite::accept_async(stream)
                    .await
                    .expect("handshake");
            // Hold open without reading so ping frames are never answered.
            tokio::time::sleep(std::time::Duration::from_secs(10)).await;
        });

        let client = StreamClient::new(url, StreamId::A)
            .with_idle_timeout(std::time::Duration::from_secs(60))
            .with_heartbeat_interval(std::time::Duration::from_millis(50))
            .with_liveness_deadline(std::time::Duration::from_millis(150));
        let mut events = Box::pin(client.stream());

        let mut saw_handshake = false;
        loop {
            match next_transition(&mut events).await {
                StreamTransition::HandshakeSucceeded { .. } => {
                    saw_handshake = true;
                }
                StreamTransition::TransportLost { reason, .. } => {
                    assert!(saw_handshake);
                    assert_eq!(
                        reason.as_str(),
                        "liveness_deadline",
                        "missed liveness deadline must report the bounded reason"
                    );
                    return;
                }
                other => panic!("unexpected event {other:?}"),
            }
        }
    }

    #[tokio::test]
    async fn delayed_heartbeats_inside_liveness_bound_keep_transport_alive() {
        let (listener, url) = bind_listener().await;
        tokio::spawn(async move {
            let (stream, _) = listener.accept().await.expect("accept fixture");
            let mut ws: tokio_tungstenite::WebSocketStream<tokio::net::TcpStream> =
                tokio_tungstenite::accept_async(stream).await.expect("handshake");
            while let Some(Ok(msg)) = futures::StreamExt::next(&mut ws).await {
                if matches!(msg, Message::Ping(_)) {
                    tokio::time::sleep(std::time::Duration::from_millis(60)).await;
                    ws.send(Message::Pong(vec![])).await.ok();
                }
            }
            tokio::time::sleep(std::time::Duration::from_secs(5)).await;
        });

        let client = StreamClient::new(url, StreamId::A)
            .with_idle_timeout(std::time::Duration::from_secs(60))
            .with_heartbeat_interval(std::time::Duration::from_millis(30))
            .with_liveness_deadline(std::time::Duration::from_millis(150));
        let mut events = Box::pin(client.stream());

        assert!(matches!(
            next_transition(&mut events).await,
            StreamTransition::HandshakeSucceeded { .. }
        ));
        expect_quiet_transport(&mut events, 200).await;
    }

    #[tokio::test]
    async fn peer_close_is_true_transport_loss() {
        let (listener, url) = bind_listener().await;
        tokio::spawn(async move {
            let (stream, _) = listener.accept().await.expect("accept fixture");
            let mut ws: tokio_tungstenite::WebSocketStream<tokio::net::TcpStream> =
                tokio_tungstenite::accept_async(stream).await.expect("handshake");
            ws.send(Message::Text(r#"{"time_us":1}"#.into())).await.ok();
            tokio::time::sleep(std::time::Duration::from_millis(120)).await;
            let _ = ws.send(Message::Close(None)).await;
            tokio::time::sleep(std::time::Duration::from_secs(5)).await;
        });

        let client = StreamClient::new(url, StreamId::A)
            .with_idle_timeout(std::time::Duration::from_secs(60))
            .with_heartbeat_interval(std::time::Duration::from_millis(50))
            .with_liveness_deadline(std::time::Duration::from_millis(500))
            .with_backoff_policy(super::BackoffPolicy::new(
                std::time::Duration::from_millis(50),
                std::time::Duration::from_millis(100),
            ));
        let mut events = Box::pin(client.stream());

        loop {
            match next_transition(&mut events).await {
                StreamTransition::HandshakeSucceeded { .. } => {}
                StreamTransition::DeliveryResumed => {}
                StreamTransition::TransportLost { reason, .. } => {
                    assert_eq!(reason.as_str(), "peer_close");
                    return;
                }
                other => panic!("unexpected event {other:?}"),
            }
        }
    }

    #[tokio::test]
    async fn socket_read_failure_is_true_transport_loss() {
        let (listener, url) = bind_listener().await;
        tokio::spawn(async move {
            let (stream, _) = listener.accept().await.expect("accept fixture");
            let ws: tokio_tungstenite::WebSocketStream<tokio::net::TcpStream> =
                tokio_tungstenite::accept_async(stream).await.expect("handshake");
            // Abruptly close the TCP connection to force a socket read failure.
            tokio::time::sleep(std::time::Duration::from_millis(120)).await;
            drop(ws);
            tokio::time::sleep(std::time::Duration::from_secs(5)).await;
        });

        let client = StreamClient::new(url, StreamId::A)
            .with_idle_timeout(std::time::Duration::from_secs(60))
            .with_heartbeat_interval(std::time::Duration::from_millis(50))
            .with_liveness_deadline(std::time::Duration::from_millis(500))
            .with_backoff_policy(super::BackoffPolicy::new(
                std::time::Duration::from_millis(50),
                std::time::Duration::from_millis(100),
            ));
        let mut events = Box::pin(client.stream());

        loop {
            match next_transition(&mut events).await {
                StreamTransition::HandshakeSucceeded { .. } => {}
                StreamTransition::DeliveryResumed => {}
                StreamTransition::TransportLost { reason, .. } => {
                    assert_eq!(reason.as_str(), "socket_error");
                    return;
                }
                other => panic!("unexpected event {other:?}"),
            }
        }
    }

    #[tokio::test]
    async fn repeated_handshake_failures_record_ordinals_and_bounded_delays() {
        let (listener, url) = bind_listener().await;
        tokio::spawn(async move {
            // Never accept the WebSocket handshake; hold the TCP connection.
            let (_stream, _) = listener.accept().await.expect("accept fixture");
            tokio::time::sleep(std::time::Duration::from_secs(20)).await;
        });

        let client = StreamClient::new(url, StreamId::A)
            .with_connect_timeout(std::time::Duration::from_millis(80))
            .with_backoff_policy(super::BackoffPolicy::new(
                std::time::Duration::from_millis(30),
                std::time::Duration::from_millis(60),
            ));
        let mut events = Box::pin(client.stream());

        let mut ordinals = Vec::new();
        let mut delays = Vec::new();
        for _ in 0..4 {
            match next_transition(&mut events).await {
                StreamTransition::ReconnectAttemptFailed {
                    ordinal,
                    reason,
                    scheduled_delay_ms,
                } => {
                    assert_eq!(reason.as_str(), "connect_timeout");
                    ordinals.push(ordinal);
                    delays.push(scheduled_delay_ms);
                }
                other => panic!("unexpected event {other:?}"),
            }
        }
        assert_eq!(ordinals, vec![1, 2, 3, 4], "attempt ordinals must increase");
        for delay in &delays {
            assert!(
                (20..=80).contains(delay),
                "delay {delay}ms must stay within the configured backoff window"
            );
        }
    }

    #[tokio::test]
    async fn same_socket_delivery_recovery_emits_resume_without_reconnect() {
        let (listener, url) = bind_listener().await;
        tokio::spawn(async move {
            let (stream, _) = listener.accept().await.expect("accept fixture");
            let mut ws: tokio_tungstenite::WebSocketStream<tokio::net::TcpStream> =
                tokio_tungstenite::accept_async(stream).await.expect("handshake");
            ws.send(Message::Text(r#"{"time_us":100}"#.into())).await.ok();
            // Stay silent past the idle deadline, then resume on the same socket.
            tokio::time::sleep(std::time::Duration::from_millis(250)).await;
            ws.send(Message::Text(r#"{"time_us":340}"#.into())).await.ok();
            // Keep responding to pings for the remainder of the fixture window.
            while let Ok(Some(_)) = tokio::time::timeout(
                std::time::Duration::from_secs(5),
                futures::StreamExt::next(&mut ws),
            )
            .await
            {}
        });

        let client = StreamClient::new(url, StreamId::A)
            .with_idle_timeout(std::time::Duration::from_millis(100))
            .with_heartbeat_interval(std::time::Duration::from_millis(50))
            .with_liveness_deadline(std::time::Duration::from_millis(400));
        let mut events = Box::pin(client.stream());

        let mut handshakes = 0;
        let mut saw_resume_after_idle = false;
        let mut saw_loss = false;
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(3);
        while std::time::Instant::now() < deadline {
            match tokio::time::timeout_at(
                tokio::time::Instant::from_std(deadline),
                futures::StreamExt::next(&mut events),
            )
            .await
            {
                Ok(Some(StreamEvent::Transition(StreamTransition::HandshakeSucceeded { .. }))) => {
                    handshakes += 1;
                }
                Ok(Some(StreamEvent::Transition(StreamTransition::DeliveryResumed))) => {
                    saw_resume_after_idle = true;
                }
                Ok(Some(StreamEvent::Transition(StreamTransition::DeliveryIdle { .. }))) => {}
                Ok(Some(StreamEvent::Record(_))) => {}
                Ok(Some(StreamEvent::Transition(StreamTransition::TransportLost { .. }))) => {
                    saw_loss = true;
                }
                Ok(Some(StreamEvent::Transition(
                    StreamTransition::ReconnectAttemptFailed { .. },
                ))) => {}
                Ok(None) => panic!("stream ended early"),
                Err(_) => break,
            }
        }
        assert_eq!(handshakes, 1, "socket must not reconnect during delivery idle");
        assert!(!saw_loss, "no transport loss on a responsive idle socket");
        assert!(saw_resume_after_idle, "delivery must resume on the same socket");
    }
}
