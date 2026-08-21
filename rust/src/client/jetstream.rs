use crate::models::{
    errors::TurboError, jetstream::JetstreamMessage, recovery::IngestionCheckpoint,
    recovery::ReconnectReason, TurboResult,
};
use crate::storage::SQLiteStore;
use crate::turbocharger::PipelineProgress;
use futures::{Stream, StreamExt};
use std::pin::Pin;
use std::sync::Arc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};
use tokio::sync::mpsc;
use tokio::time::{sleep, Instant};
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

pub struct JetstreamClient {
    endpoints: Vec<String>,
    wanted_collections: String,
    endpoint_backoff_min: Duration,
    endpoint_backoff_max: Duration,
    channel_capacity: usize,
    data_idle_timeout: Option<Duration>,
    connection_timeout: Option<Duration>,
    cursor_overlap: Duration,
    checkpoint_store: Option<Arc<SQLiteStore>>,
    progress: Option<Arc<PipelineProgress>>,
}

impl JetstreamClient {
    pub fn new(endpoints: Vec<String>, wanted_collections: String) -> Self {
        Self {
            endpoints,
            wanted_collections,
            endpoint_backoff_min: Duration::from_secs(1),
            endpoint_backoff_max: Duration::from_secs(30),
            channel_capacity: DEFAULT_CHANNEL_CAPACITY,
            data_idle_timeout: Some(Duration::from_secs(120)),
            connection_timeout: Some(Duration::from_secs(10)),
            cursor_overlap: Duration::from_secs(10),
            checkpoint_store: None,
            progress: None,
        }
    }

    pub fn with_defaults(endpoints: Vec<String>) -> Self {
        Self::new(endpoints, "app.bsky.feed.post".to_string())
    }

    pub fn with_channel_capacity(mut self, capacity: usize) -> Self {
        self.channel_capacity = capacity;
        self
    }

    pub fn with_data_idle_timeout(mut self, timeout: Duration) -> Self {
        self.data_idle_timeout = Some(timeout);
        self
    }

    pub fn with_connection_timeout(mut self, timeout: Duration) -> Self {
        self.connection_timeout = Some(timeout);
        self
    }

    pub fn without_connection_timeout(mut self) -> Self {
        self.connection_timeout = None;
        self
    }

    pub fn with_endpoint_backoff(mut self, minimum: Duration, maximum: Duration) -> Self {
        self.endpoint_backoff_min = minimum;
        self.endpoint_backoff_max = maximum;
        self
    }

    pub fn with_cursor_overlap(mut self, overlap: Duration) -> Self {
        self.cursor_overlap = overlap;
        self
    }

    pub fn with_checkpoint_store(mut self, store: Arc<SQLiteStore>) -> Self {
        self.checkpoint_store = Some(store);
        self
    }

    pub fn without_data_idle_timeout(mut self) -> Self {
        self.data_idle_timeout = None;
        self
    }

    pub fn with_progress_tracker(mut self, progress: Arc<PipelineProgress>) -> Self {
        self.progress = Some(progress);
        self
    }

    pub fn parse_message(&self, text: &str) -> TurboResult<JetstreamMessage> {
        parse_message(text)
    }

    /// Parse a batch of raw messages from owned buffers, reusing simd-json's
    /// internal scratch buffers across messages. Consumes the buffers (mirrors
    /// production: the Jetstream socket hands us owned Strings).
    pub fn parse_message_batch(&self, raws: Vec<String>) -> TurboResult<Vec<JetstreamMessage>> {
        let mut buffers =
            simd_json::Buffers::new(raws.iter().map(String::len).max().unwrap_or(128).max(128));
        let mut out = Vec::with_capacity(raws.len());
        for raw in raws {
            out.push(parse_message_owned_with_buffers(raw, &mut buffers)?);
        }
        Ok(out)
    }
}

impl MessageSource for JetstreamClient {
    async fn stream_messages(
        &self,
    ) -> TurboResult<Pin<Box<dyn Stream<Item = TurboResult<JetstreamMessage>> + Send>>> {
        let (tx, rx) = mpsc::channel(self.channel_capacity);
        if self.endpoints.is_empty() {
            return Err(TurboError::WebSocketConnection(
                "at least one Jetstream endpoint is required".to_string(),
            ));
        }

        // Start the connection loop
        let endpoints = self.endpoints.clone();
        let wanted_collections = self.wanted_collections.clone();
        let endpoint_backoff_min = self.endpoint_backoff_min;
        let endpoint_backoff_max = self.endpoint_backoff_max;
        let data_idle_timeout = self.data_idle_timeout;
        let connection_timeout = self.connection_timeout;
        let cursor_overlap = self.cursor_overlap;
        let checkpoint_store = self.checkpoint_store.clone();
        let progress = self.progress.clone();

        tokio::spawn(async move {
            let mut current_endpoint = 0;
            let mut attempts_in_sweep = 0usize;
            let mut sweep_number = 0u32;
            let mut endpoint_failures = vec![0u32; endpoints.len()];
            let mut endpoint_eligible_at = vec![Instant::now(); endpoints.len()];
            loop {
                let endpoint = &endpoints[current_endpoint];
                if let Some(progress) = &progress {
                    progress.connecting();
                }
                let checkpoint = if let Some(store) = &checkpoint_store {
                    match store.load_ingestion_checkpoint().await {
                        Ok(checkpoint) => checkpoint,
                        Err(error) => {
                            error!(endpoint, %error, "Failed to load durable ingestion checkpoint");
                            let _ = tx.send(Err(error)).await;
                            return;
                        }
                    }
                } else {
                    None
                };
                let url = match endpoint_url(
                    endpoint,
                    &wanted_collections,
                    checkpoint.as_ref(),
                    cursor_overlap,
                ) {
                    Ok(url) => url,
                    Err(error) => {
                        error!(endpoint, %error, "Invalid Jetstream endpoint URL");
                        let _ = tx.send(Err(error)).await;
                        return;
                    }
                };

                info!("Connecting to Jetstream endpoint: {}", endpoint);
                metrics::counter!(
                    "jetstream_endpoint_attempts_total",
                    "endpoint" => endpoint.clone()
                )
                .increment(1);
                if let Some(progress) = &progress {
                    progress.endpoint_attempted(endpoint);
                }

                let connection_result = if let Some(connection_timeout) = connection_timeout {
                    tokio::time::timeout(connection_timeout, connect_async(&url)).await
                } else {
                    Ok(connect_async(&url).await)
                };
                match connection_result {
                    Ok(Ok((ws_stream, response))) => {
                        info!("Successfully connected to {}", endpoint);
                        if let Some(progress) = &progress {
                            progress.connection_established_with_replay(
                                endpoint.clone(),
                                checkpoint.is_some(),
                            );
                            if headers_indicate_clamped(response.headers()) {
                                progress.mark_unrecoverable_gap(
                                    "Jetstream reported that the requested replay cursor was clamped",
                                );
                                metrics::counter!(
                                    "jetstream_reconnects_total",
                                    "endpoint" => endpoint.clone(),
                                    "reason" => ReconnectReason::ReplayClamped.as_str()
                                )
                                .increment(1);
                            } else if checkpoint.is_some() {
                                progress.clear_unrecoverable_gap();
                            }
                        }
                        metrics::gauge!("jetstream_connected", "endpoint" => endpoint.clone())
                            .set(1.0);
                        endpoint_failures[current_endpoint] = 0;

                        let (_, mut read) = ws_stream.split();
                        // Reuse simd-json's internal scratch buffers across all
                        // messages on this connection (avoids per-message
                        // allocation of the parse scratch buffers).
                        let mut parse_buffers = simd_json::Buffers::new(8192);
                        let useful_data_deadline = tokio::time::sleep_until(
                            data_idle_timeout
                                .map(|timeout| Instant::now() + timeout)
                                .unwrap_or_else(|| {
                                    Instant::now() + Duration::from_secs(100 * 365 * 24 * 60 * 60)
                                }),
                        );
                        tokio::pin!(useful_data_deadline);

                        // Process messages
                        loop {
                            tokio::select! {
                                _ = &mut useful_data_deadline => {
                                    warn!(endpoint, idle_seconds = data_idle_timeout.map(|timeout| timeout.as_secs()).unwrap_or(0), reconnect_reason = "data_idle_timeout", "Jetstream connection delivered no useful data before deadline; rotating endpoint");
                                    report_reconnect(&progress, endpoint, ReconnectReason::DataIdleTimeout);
                                    break;
                                }
                                msg_result = read.next() => {
                                    let Some(msg_result) = msg_result else {
                                        report_reconnect(&progress, endpoint, ReconnectReason::PeerClose);
                                        break;
                                    };

                                    match msg_result {
                                Ok(Message::Text(text)) => {
                                    trace!("Received message: {}", text);
                                    // Parse the owned buffer directly (no input copy).
                                    match parse_message_owned_with_buffers(
                                        text,
                                        &mut parse_buffers,
                                    ) {
                                        Ok(message) => {
                                            if is_in_scope(&message, &wanted_collections) {
                                                if let Some(event_time_us) = message.time_us {
                                                    if let Some(timeout) = data_idle_timeout {
                                                        useful_data_deadline.as_mut().reset(Instant::now() + timeout);
                                                    }
                                                    if let Some(progress) = &progress {
                                                        if let Some(recovery_ms) = progress.valid_ingress_event(
                                                            event_time_us
                                                        ) {
                                                            info!(endpoint, recovery_ms, "Jetstream useful-data delivery recovered");
                                                            metrics::histogram!("jetstream_recovery_duration_seconds", "endpoint" => endpoint.clone()).record(recovery_ms as f64 / 1000.0);
                                                        }
                                                    }
                                                    let now_us = SystemTime::now()
                                                        .duration_since(UNIX_EPOCH)
                                                        .unwrap_or_default()
                                                        .as_micros()
                                                        .min(u64::MAX as u128) as u64;
                                                    metrics::gauge!("jetstream_last_received_event_time_us")
                                                        .set(event_time_us as f64);
                                                    metrics::gauge!("jetstream_received_lag_seconds")
                                                        .set(now_us.saturating_sub(event_time_us) as f64 / 1_000_000.0);
                                                }
                                                if checkpoint.is_some() {
                                                    metrics::counter!("jetstream_replayed_events_total").increment(1);
                                                    if let Some(progress) = &progress {
                                                        progress.replayed_event();
                                                    }
                                                }
                                                metrics::counter!("jetstream_valid_messages_total", "endpoint" => endpoint.clone()).increment(1);
                                                let occupancy_before = tx.max_capacity() - tx.capacity();
                                                let blocked = tx.capacity() == 0;
                                                metrics::gauge!("jetstream_input_queue_occupancy")
                                                    .set(occupancy_before as f64);
                                                if blocked {
                                                    metrics::gauge!("jetstream_input_backpressured").set(1.0);
                                                    if let Some(progress) = &progress {
                                                        progress.input_blocked(occupancy_before);
                                                    }
                                                }
                                                let blocked_at = Instant::now();
                                                if tx.send(Ok(message)).await.is_err() {
                                                    info!("Receiver dropped, stopping stream");
                                                    return;
                                                }
                                                let blocked_duration = blocked_at.elapsed();
                                                metrics::histogram!(
                                                    "jetstream_input_blocked_send_seconds",
                                                    "endpoint" => endpoint.clone()
                                                )
                                                .record(blocked_duration.as_secs_f64());
                                                let occupancy_after = tx.max_capacity() - tx.capacity();
                                                metrics::gauge!("jetstream_input_queue_occupancy")
                                                    .set(occupancy_after as f64);
                                                if blocked {
                                                    metrics::gauge!("jetstream_input_backpressured").set(0.0);
                                                    if let Some(progress) = &progress {
                                                        progress.input_recovered(occupancy_after);
                                                    }
                                                }
                                                if let Some(progress) = &progress {
                                                    progress.input_send_completed(
                                                        occupancy_after,
                                                        blocked_duration,
                                                    );
                                                }
                                            } else {
                                                trace!(endpoint, "Ignoring out-of-scope Jetstream data event");
                                        }
                                        },
                                        Err(e) => {
                                            warn!(
                                                "Failed to parse message: {:?} (raw text not retained by owned-parse path)",
                                                e
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
                                    report_reconnect(&progress, endpoint, ReconnectReason::PeerClose);
                                    break;
                                }
                                Ok(Message::Frame(_)) => {
                                    // Ignore raw frames
                                    trace!("Received raw frame (ignoring)");
                                }
                                Err(e) => {
                                    error!("WebSocket error: {}", e);
                                    report_reconnect(&progress, endpoint, ReconnectReason::SocketRead);
                                    break;
                                }
                            }
                                }
                            }
                        }
                    }
                    Ok(Err(e)) => {
                        error!("Failed to connect to {}: {}", endpoint, e);
                        let reason = handshake_reconnect_reason(&e, checkpoint.is_some());
                        if reason == ReconnectReason::ReplayRejected {
                            if let Some(progress) = &progress {
                                progress.mark_unrecoverable_gap(format!(
                                    "Jetstream endpoint {endpoint} rejected the durable replay cursor"
                                ));
                            }
                        }
                        report_reconnect(&progress, endpoint, reason);
                    }
                    Err(_) => {
                        error!(
                            endpoint,
                            timeout_seconds = connection_timeout
                                .map(|timeout| timeout.as_secs_f64())
                                .unwrap_or_default(),
                            "Jetstream WebSocket handshake timed out"
                        );
                        report_reconnect(&progress, endpoint, ReconnectReason::ConnectTimeout);
                    }
                }

                endpoint_failures[current_endpoint] =
                    endpoint_failures[current_endpoint].saturating_add(1);
                let endpoint_penalty = exponential_penalty(
                    endpoint_backoff_min,
                    endpoint_backoff_max,
                    endpoint_failures[current_endpoint],
                );
                endpoint_eligible_at[current_endpoint] = Instant::now() + endpoint_penalty;
                metrics::counter!(
                    "jetstream_endpoint_failures_total",
                    "endpoint" => endpoint.clone()
                )
                .increment(1);
                if let Some(progress) = &progress {
                    progress.endpoint_failed(endpoint);
                }

                attempts_in_sweep += 1;
                current_endpoint = (current_endpoint + 1) % endpoints.len();
                if attempts_in_sweep == endpoints.len() {
                    sweep_number = sweep_number.saturating_add(1);
                    let entropy = SystemTime::now()
                        .duration_since(UNIX_EPOCH)
                        .unwrap_or_default()
                        .subsec_nanos() as u64;
                    let global_backoff = jittered_backoff(
                        endpoint_backoff_min,
                        endpoint_backoff_max,
                        sweep_number,
                        entropy,
                    );
                    let earliest_endpoint = endpoint_eligible_at
                        .iter()
                        .enumerate()
                        .min_by_key(|(_, eligible_at)| **eligible_at)
                        .map(|(index, _)| index)
                        .unwrap_or(0);
                    current_endpoint = earliest_endpoint;
                    let endpoint_wait = endpoint_eligible_at[earliest_endpoint]
                        .saturating_duration_since(Instant::now());
                    let delay = global_backoff.max(endpoint_wait);
                    info!(
                        sweep_number,
                        delay_seconds = delay.as_secs_f64(),
                        "All Jetstream endpoints failed; backing off before next sweep"
                    );
                    metrics::counter!("jetstream_endpoint_sweeps_exhausted_total").increment(1);
                    sleep(delay).await;
                    attempts_in_sweep = 0;
                }
            }
        });

        Ok(Box::pin(ReceiverStream::new(rx)))
    }
}

fn exponential_penalty(minimum: Duration, maximum: Duration, failures: u32) -> Duration {
    let exponent = failures.saturating_sub(1).min(31);
    minimum.saturating_mul(1u32 << exponent).min(maximum)
}

fn jittered_backoff(
    minimum: Duration,
    maximum: Duration,
    sweep_number: u32,
    entropy: u64,
) -> Duration {
    let ceiling = exponential_penalty(minimum, maximum, sweep_number);
    let minimum_ms = minimum.as_millis().min(u64::MAX as u128) as u64;
    let ceiling_ms = ceiling.as_millis().min(u64::MAX as u128) as u64;
    if ceiling_ms <= minimum_ms {
        return minimum;
    }
    let width = ceiling_ms - minimum_ms;
    Duration::from_millis(minimum_ms + entropy % (width + 1))
}

fn report_reconnect(
    progress: &Option<Arc<PipelineProgress>>,
    endpoint: &str,
    reason: ReconnectReason,
) {
    if let Some(progress) = progress {
        progress.disconnected(reason.as_str());
    }
    metrics::counter!(
        "jetstream_reconnects_total",
        "endpoint" => endpoint.to_string(),
        "reason" => reason.as_str()
    )
    .increment(1);
    metrics::gauge!("jetstream_connected", "endpoint" => endpoint.to_string()).set(0.0);
}

fn handshake_reconnect_reason(
    error: &tokio_tungstenite::tungstenite::Error,
    replay_requested: bool,
) -> ReconnectReason {
    if replay_requested
        && matches!(
            error,
            tokio_tungstenite::tungstenite::Error::Http(response)
                if response.status().is_client_error()
        )
    {
        ReconnectReason::ReplayRejected
    } else {
        ReconnectReason::HandshakeFailure
    }
}

fn headers_indicate_clamped(headers: &tokio_tungstenite::tungstenite::http::HeaderMap) -> bool {
    ["x-jetstream-cursor-clamped", "x-cursor-clamped"]
        .into_iter()
        .filter_map(|name| headers.get(name))
        .filter_map(|value| value.to_str().ok())
        .any(|value| value.eq_ignore_ascii_case("true") || value == "1")
}

fn endpoint_url(
    endpoint: &str,
    wanted_collections: &str,
    checkpoint: Option<&IngestionCheckpoint>,
    cursor_overlap: Duration,
) -> TurboResult<String> {
    let base = if endpoint.starts_with("ws://") || endpoint.starts_with("wss://") {
        endpoint.trim_end_matches('/').to_string()
    } else {
        format!("wss://{endpoint}")
    };
    let mut url = url::Url::parse(&base).map_err(|error| {
        TurboError::WebSocketConnection(format!("invalid endpoint URL {endpoint}: {error}"))
    })?;
    url.set_path("/subscribe");

    let mut query = url.query_pairs_mut();
    query.append_pair("wantedCollections", wanted_collections);
    if let Some(checkpoint) = checkpoint {
        let overlap_us = cursor_overlap.as_micros().min(u64::MAX as u128) as u64;
        let cursor = checkpoint.cursor.time_us.saturating_sub(overlap_us);
        query.append_pair("cursor", &cursor.to_string());
    }
    drop(query);

    Ok(url.into())
}

fn parse_message(text: &str) -> TurboResult<JetstreamMessage> {
    parse_message_with_buffers(text, &mut simd_json::Buffers::new(text.len()))
}

fn parse_message_with_buffers(
    text: &str,
    buffers: &mut simd_json::Buffers,
) -> TurboResult<JetstreamMessage> {
    parse_message_owned_with_buffers(text.to_string(), buffers)
}

/// Parse a message from an already-owned buffer, avoiding the input copy that
/// the `&str` path must make (simd-json requires mutable input). The Jetstream
/// socket hands us an owned `String` per message, so production uses this.
fn parse_message_owned_with_buffers(
    mut text: String,
    buffers: &mut simd_json::Buffers,
) -> TurboResult<JetstreamMessage> {
    // Use simd-json for faster parsing (2-4x faster than serde_json)
    // simd-json requires mutable input and uses unsafe SIMD operations internally
    // The library handles safety internally through careful validation
    let message: JetstreamMessage =
        unsafe { simd_json::serde::from_str_with_buffers(&mut text, buffers)? };

    // Validate required fields
    if message.did.is_empty() {
        return Err(TurboError::InvalidMessage("DID is empty".to_string()));
    }

    Ok(message)
}

fn is_in_scope(message: &JetstreamMessage, wanted_collections: &str) -> bool {
    match message.kind {
        crate::models::jetstream::MessageKind::Commit => message
            .commit
            .as_ref()
            .and_then(|commit| commit.collection.as_deref())
            .is_some_and(|collection| {
                wanted_collections
                    .split(',')
                    .any(|wanted| wanted.trim() == collection)
            }),
        crate::models::jetstream::MessageKind::Identity
        | crate::models::jetstream::MessageKind::Account => true,
        crate::models::jetstream::MessageKind::Unknown => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::turbocharger::{PipelineReadinessState, ProgressThresholds};
    use futures::SinkExt;
    use tokio::net::TcpListener;
    use tokio_tungstenite::{accept_async, accept_hdr_async};

    fn checkpoint(time_us: u64, source_seq: Option<u64>) -> IngestionCheckpoint {
        let message = JetstreamMessage {
            did: "did:plc:cursor".to_string().into(),
            time_us: Some(time_us),
            seq: source_seq,
            kind: crate::models::jetstream::MessageKind::Account,
            commit: None,
        };
        IngestionCheckpoint {
            ingress_ordinal: 42,
            cursor: crate::models::recovery::SourceCursor::from_message(&message).unwrap(),
            updated_at: chrono::Utc::now(),
        }
    }

    async fn local_fixture(frames: Vec<Message>, hold_open: Duration) -> String {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        tokio::spawn(async move {
            let (socket, _) = listener.accept().await.unwrap();
            let mut websocket = accept_async(socket).await.unwrap();
            for frame in frames {
                websocket.send(frame).await.unwrap();
            }
            sleep(hold_open).await;
        });
        format!("ws://{address}")
    }

    async fn hanging_handshake_fixture(hold_open: Duration) -> String {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        tokio::spawn(async move {
            let (_socket, _) = listener.accept().await.unwrap();
            sleep(hold_open).await;
        });
        format!("ws://{address}")
    }

    async fn capturing_fixture(
        frames: Vec<Message>,
        requests: tokio::sync::mpsc::UnboundedSender<String>,
    ) -> String {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        tokio::spawn(async move {
            let (socket, _) = listener.accept().await.unwrap();
            let mut websocket = accept_hdr_async(
                socket,
                move |request: &tokio_tungstenite::tungstenite::handshake::server::Request,
                      response| {
                    let _ = requests.send(request.uri().to_string());
                    Ok(response)
                },
            )
            .await
            .unwrap();
            for frame in frames {
                websocket.send(frame).await.unwrap();
            }
            sleep(Duration::from_millis(100)).await;
        });
        format!("ws://{address}")
    }

    fn valid_message_json() -> String {
        r#"{
            "did":"did:plc:recovered","seq":12345,"time_us":1640995200000000,
            "kind":"commit","commit":{"rev":"test-rev","operation":"create",
            "collection":"app.bsky.feed.post","rkey":"test","record":null}
        }"#
        .to_string()
    }

    fn out_of_scope_message_json() -> String {
        valid_message_json().replace("app.bsky.feed.post", "app.bsky.feed.like")
    }

    #[test]
    fn test_jetstream_client_creation() {
        let endpoints = vec![
            "jetstream.us-east.bsky.network".to_string(),
            "jetstream.us-west.bsky.network".to_string(),
        ];

        let client = JetstreamClient::new(endpoints.clone(), "app.bsky.feed.post".to_string());
        assert_eq!(client.endpoints, endpoints);
        assert_eq!(client.wanted_collections, "app.bsky.feed.post");
    }

    #[test]
    fn test_jetstream_client_with_defaults() {
        let endpoints = vec!["jetstream.us-east.bsky.network".to_string()];
        let client = JetstreamClient::with_defaults(endpoints);
        assert_eq!(client.wanted_collections, "app.bsky.feed.post");
    }

    #[test]
    fn endpoint_url_without_checkpoint_starts_at_live_tip() {
        let url = endpoint_url(
            "jetstream.example",
            "app.bsky.feed.post,app.bsky.feed.like",
            None,
            Duration::from_secs(10),
        )
        .unwrap();

        assert_eq!(
            url,
            "wss://jetstream.example/subscribe?wantedCollections=app.bsky.feed.post%2Capp.bsky.feed.like"
        );
    }

    #[test]
    fn endpoint_url_rewinds_checkpoint_and_preserves_existing_query() {
        let url = endpoint_url(
            "wss://jetstream.example/old?compression=true",
            "app.bsky.feed.post",
            Some(&checkpoint(15_000_000, Some(99))),
            Duration::from_secs(10),
        )
        .unwrap();

        assert_eq!(
            url,
            "wss://jetstream.example/subscribe?compression=true&wantedCollections=app.bsky.feed.post&cursor=5000000"
        );
    }

    #[test]
    fn endpoint_url_clamps_rewound_cursor_at_zero() {
        let url = endpoint_url(
            "jetstream.example",
            "app.bsky.feed.post",
            Some(&checkpoint(5_000_000, None)),
            Duration::from_secs(10),
        )
        .unwrap();

        assert!(url.ends_with("&cursor=0"), "unexpected URL: {url}");
    }

    #[test]
    fn endpoint_url_does_not_use_endpoint_local_sequence() {
        let first = endpoint_url(
            "jetstream.us-west.example",
            "app.bsky.feed.post",
            Some(&checkpoint(15_000_000, Some(1))),
            Duration::from_secs(10),
        )
        .unwrap();
        let second = endpoint_url(
            "jetstream.us-east.example",
            "app.bsky.feed.post",
            Some(&checkpoint(15_000_000, Some(9_999))),
            Duration::from_secs(10),
        )
        .unwrap();

        assert_eq!(
            url::Url::parse(&first).unwrap().query(),
            url::Url::parse(&second).unwrap().query()
        );
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
        assert_eq!(message.did.as_ref(), "did:plc:test");
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

    #[tokio::test]
    async fn useful_data_idle_rotates_past_control_and_malformed_frames_then_recovers() {
        let stale = local_fixture(
            vec![
                Message::Ping(Vec::new()),
                Message::Binary(vec![1, 2, 3]),
                Message::Text("not-json".into()),
                Message::Text(out_of_scope_message_json()),
            ],
            Duration::from_secs(2),
        )
        .await;
        let recovered = local_fixture(
            vec![Message::Text(valid_message_json())],
            Duration::from_millis(100),
        )
        .await;
        let progress = Arc::new(PipelineProgress::new(2, 10));
        let client = JetstreamClient::new(
            vec![stale, recovered.clone()],
            "app.bsky.feed.post".to_string(),
        )
        .with_data_idle_timeout(Duration::from_millis(50))
        .with_progress_tracker(Arc::clone(&progress));

        let mut stream = client.stream_messages().await.unwrap();
        let message = tokio::time::timeout(Duration::from_millis(500), stream.next())
            .await
            .expect("fixture should recover before timeout")
            .expect("stream should produce a result")
            .expect("recovered message should be valid");

        assert_eq!(message.did.as_ref(), "did:plc:recovered");
        let snapshot = progress.snapshot(ProgressThresholds {
            startup_grace: Duration::from_secs(10),
            ingress_idle: Duration::from_secs(10),
            batch_execution: Duration::from_secs(10),
            recovery_successes: 1,
        });
        assert_eq!(snapshot.readiness_state, PipelineReadinessState::Healthy);
        assert_eq!(snapshot.ingress_messages, 1);
        assert_eq!(snapshot.reconnect_count, 1);
        assert_eq!(
            snapshot.last_reconnect_reason.as_deref(),
            Some("data_idle_timeout")
        );
        assert_eq!(
            snapshot.connected_endpoint.as_deref(),
            Some(recovered.as_str())
        );
        assert!(snapshot.recovery_duration_ms.is_some());
    }

    #[tokio::test]
    async fn connection_timeout_rotates_past_hanging_handshake() {
        let hanging = hanging_handshake_fixture(Duration::from_secs(2)).await;
        let recovered = local_fixture(
            vec![Message::Text(valid_message_json())],
            Duration::from_millis(100),
        )
        .await;
        let client =
            JetstreamClient::new(vec![hanging, recovered], "app.bsky.feed.post".to_string())
                .with_connection_timeout(Duration::from_millis(30))
                .with_data_idle_timeout(Duration::from_secs(1));

        let mut stream = client.stream_messages().await.unwrap();
        let message = tokio::time::timeout(Duration::from_secs(3), stream.next())
            .await
            .expect("alternate endpoint should be attempted after bounded handshake")
            .unwrap()
            .unwrap();

        assert_eq!(message.did.as_ref(), "did:plc:recovered");
    }

    #[tokio::test]
    async fn saturated_input_channel_waits_without_dropping() {
        let endpoint = local_fixture(
            vec![
                Message::Text(valid_message_json()),
                Message::Text(valid_message_json().replace("12345", "12346")),
            ],
            Duration::from_secs(1),
        )
        .await;
        let progress = Arc::new(PipelineProgress::new(1, 1));
        let client = JetstreamClient::new(vec![endpoint], "app.bsky.feed.post".to_string())
            .with_channel_capacity(1)
            .with_data_idle_timeout(Duration::from_secs(1))
            .with_progress_tracker(Arc::clone(&progress));
        let mut stream = client.stream_messages().await.unwrap();
        sleep(Duration::from_millis(50)).await;

        let first = stream.next().await.unwrap().unwrap();
        let second = stream.next().await.unwrap().unwrap();
        let snapshot = progress.snapshot(ProgressThresholds {
            startup_grace: Duration::from_secs(1),
            ingress_idle: Duration::from_secs(1),
            batch_execution: Duration::from_secs(1),
            recovery_successes: 1,
        });

        assert_eq!(first.seq, Some(12_345));
        assert_eq!(second.seq, Some(12_346));
        assert_eq!(snapshot.input_drops, 0);
    }

    #[tokio::test]
    async fn backpressure_disconnect_reconnects_from_durable_checkpoint() {
        let temp_dir = tempfile::tempdir().unwrap();
        let store = Arc::new(
            SQLiteStore::new(
                temp_dir.path().join("recovery.db"),
                crate::storage::SQLitePragmaConfig {
                    cache_size_kib: 1024,
                    mmap_size_mb: 1,
                    journal_size_limit_mb: 1,
                },
            )
            .await
            .unwrap(),
        );
        store
            .advance_ingestion_checkpoint(&IngestionCheckpoint {
                ingress_ordinal: 7,
                cursor: crate::models::recovery::SourceCursor {
                    time_us: 15_000_000,
                    source_seq: Some(70),
                    source_event_id: crate::models::recovery::SourceEventId::from(
                        "checkpoint-event".to_string(),
                    ),
                },
                updated_at: chrono::Utc::now(),
            })
            .await
            .unwrap();
        let (request_tx, mut request_rx) = tokio::sync::mpsc::unbounded_channel();
        let first = capturing_fixture(
            vec![
                Message::Text(valid_message_json()),
                Message::Text(valid_message_json().replace("12345", "12346")),
                Message::Close(None),
            ],
            request_tx.clone(),
        )
        .await;
        let second = capturing_fixture(
            vec![Message::Text(
                valid_message_json().replace("12345", "12347"),
            )],
            request_tx,
        )
        .await;
        let progress = Arc::new(PipelineProgress::new(1, 1));
        let client = JetstreamClient::new(vec![first, second], "app.bsky.feed.post".to_string())
            .with_channel_capacity(1)
            .with_cursor_overlap(Duration::from_secs(10))
            .with_checkpoint_store(store)
            .with_progress_tracker(Arc::clone(&progress));
        let mut stream = client.stream_messages().await.unwrap();
        sleep(Duration::from_millis(50)).await;

        for _ in 0..3 {
            tokio::time::timeout(Duration::from_secs(1), stream.next())
                .await
                .unwrap()
                .unwrap()
                .unwrap();
        }
        let first_request = request_rx.recv().await.unwrap();
        let second_request = request_rx.recv().await.unwrap();
        let snapshot = progress.snapshot(ProgressThresholds {
            startup_grace: Duration::from_secs(1),
            ingress_idle: Duration::from_secs(1),
            batch_execution: Duration::from_secs(1),
            recovery_successes: 1,
        });

        assert!(first_request.contains("cursor=5000000"));
        assert!(second_request.contains("cursor=5000000"));
        assert_eq!(snapshot.input_drops, 0);
    }

    #[tokio::test]
    async fn exhausted_endpoint_sweep_backs_off_then_retries() {
        let first_listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let first = format!("ws://{}", first_listener.local_addr().unwrap());
        let second_listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let second = format!("ws://{}", second_listener.local_addr().unwrap());
        drop(first_listener);
        drop(second_listener);
        let progress = Arc::new(PipelineProgress::new(1, 10));
        let client = JetstreamClient::new(
            vec![first.clone(), second.clone()],
            "app.bsky.feed.post".to_string(),
        )
        .with_endpoint_backoff(Duration::from_millis(10), Duration::from_millis(20))
        .with_connection_timeout(Duration::from_millis(20))
        .with_progress_tracker(Arc::clone(&progress));
        let _stream = client.stream_messages().await.unwrap();

        sleep(Duration::from_millis(80)).await;
        let snapshot = progress.snapshot(ProgressThresholds {
            startup_grace: Duration::from_secs(1),
            ingress_idle: Duration::from_secs(1),
            batch_execution: Duration::from_secs(1),
            recovery_successes: 1,
        });

        assert!(snapshot.endpoint_attempts.get(&first).copied().unwrap_or(0) >= 2);
        assert!(
            snapshot
                .endpoint_attempts
                .get(&second)
                .copied()
                .unwrap_or(0)
                >= 2
        );
        assert!(snapshot.endpoint_failures.get(&first).copied().unwrap_or(0) >= 2);
        assert!(
            snapshot
                .endpoint_failures
                .get(&second)
                .copied()
                .unwrap_or(0)
                >= 2
        );
    }

    #[test]
    fn endpoint_penalty_is_exponential_and_bounded() {
        let minimum = Duration::from_secs(1);
        let maximum = Duration::from_secs(5);

        assert_eq!(exponential_penalty(minimum, maximum, 1), minimum);
        assert_eq!(
            exponential_penalty(minimum, maximum, 3),
            Duration::from_secs(4)
        );
        assert_eq!(exponential_penalty(minimum, maximum, 20), maximum);
    }

    #[test]
    fn global_backoff_jitter_stays_within_configured_bounds() {
        let minimum = Duration::from_secs(1);
        let maximum = Duration::from_secs(10);

        let delay = jittered_backoff(minimum, maximum, 4, 7_123);

        assert!(delay >= minimum && delay <= maximum, "delay was {delay:?}");
    }

    #[test]
    fn rejected_replay_handshake_is_classified_as_unrecoverable_gap() {
        let response = tokio_tungstenite::tungstenite::http::Response::builder()
            .status(400)
            .body(None)
            .unwrap();
        let error = tokio_tungstenite::tungstenite::Error::Http(response);

        assert_eq!(
            handshake_reconnect_reason(&error, true),
            ReconnectReason::ReplayRejected
        );
        assert_eq!(
            handshake_reconnect_reason(&error, false),
            ReconnectReason::HandshakeFailure
        );
    }

    #[test]
    fn clamped_replay_response_header_is_detected() {
        let mut headers = tokio_tungstenite::tungstenite::http::HeaderMap::new();
        headers.insert("x-jetstream-cursor-clamped", "true".parse().unwrap());

        assert!(headers_indicate_clamped(&headers));
    }
}
