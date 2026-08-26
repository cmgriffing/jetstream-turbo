use crate::models::errors::{TurboError, TurboResult};
use crate::turbocharger::{HealthDiagnostics, HealthStatus, ProductionTurboCharger, TurboStats};
use axum::{
    extract::{
        ws::{Message, WebSocket, WebSocketUpgrade},
        Query, State,
    },
    http::StatusCode,
    response::Json,
    routing::{get, Router},
};
use futures::{SinkExt, StreamExt};
use serde::{Deserialize, Serialize};
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::{broadcast, mpsc};
use tokio::time::{interval, Instant, MissedTickBehavior};
use tracing::{debug, info, warn};

const MONITOR_WS_HEARTBEAT_INTERVAL: Duration = Duration::from_secs(20);
const MONITOR_WS_PEER_TIMEOUT: Duration = Duration::from_secs(75);
const MONITOR_WS_LAG_LOG_INTERVAL: Duration = Duration::from_secs(30);
const MONITOR_WS_OUTGOING_CHANNEL_CAPACITY: usize = 16;

#[derive(Deserialize)]
pub struct StatsQuery {
    pub detailed: Option<bool>,
}

#[derive(Serialize)]
pub struct StatsResponse {
    pub status: String,
    pub data: TurboStats,
}

#[derive(Serialize)]
pub struct HealthResponse {
    pub status: String,
    pub data: HealthStatus,
}

#[derive(Serialize)]
pub struct ErrorResponse {
    pub status: String,
    pub error: String,
}

pub fn create_router(turbocharger: Arc<ProductionTurboCharger>) -> Router {
    Router::new()
        .route("/health", get(health_check))
        .route("/stats", get(get_stats))
        .route("/metrics", get(get_metrics))
        .route("/ws", get(ws_handler))
        .with_state(turbocharger)
}

async fn health_check(
    State(turbocharger): State<Arc<ProductionTurboCharger>>,
) -> Result<(StatusCode, Json<HealthResponse>), StatusCode> {
    match turbocharger.health_check().await {
        Ok(status) => {
            let (status_code, response) = health_http_response(status);
            Ok((status_code, Json(response)))
        }
        Err(_) => Err(StatusCode::INTERNAL_SERVER_ERROR),
    }
}

async fn get_stats(
    State(turbocharger): State<Arc<ProductionTurboCharger>>,
    Query(_query): Query<StatsQuery>,
) -> Result<Json<StatsResponse>, StatusCode> {
    match turbocharger.get_stats().await {
        Ok(stats) => Ok(Json(StatsResponse {
            status: "success".to_string(),
            data: stats,
        })),
        Err(_) => Err(StatusCode::INTERNAL_SERVER_ERROR),
    }
}

async fn get_metrics(State(turbocharger): State<Arc<ProductionTurboCharger>>) -> String {
    let diagnostics = turbocharger.get_runtime_diagnostics().await;
    prometheus_metrics_from_diagnostics(&diagnostics)
}

async fn ws_handler(
    State(turbocharger): State<Arc<ProductionTurboCharger>>,
    ws: WebSocketUpgrade,
) -> axum::response::Response {
    ws.on_upgrade(move |socket| handle_websocket(socket, turbocharger.subscribe()))
}

async fn handle_websocket(
    socket: WebSocket,
    broadcast_rx: broadcast::Receiver<crate::models::enriched::EnrichedRecord>,
) {
    let (sender, socket_rx) = socket.split();
    let connection_id = uuid::Uuid::new_v4();
    let (outgoing_tx, outgoing_rx) = mpsc::channel(MONITOR_WS_OUTGOING_CHANNEL_CAPACITY);

    // The receive task checks peer timeout on the same 20s cadence the old
    // single-task loop used (its heartbeat tick). The heartbeat itself now
    // lives in the send task; only the timeout *check* happens here.
    let mut peer_timeout_interval = interval(MONITOR_WS_HEARTBEAT_INTERVAL);
    let mut lag_log_interval = interval(MONITOR_WS_LAG_LOG_INTERVAL);
    let mut heartbeat_interval = interval(MONITOR_WS_HEARTBEAT_INTERVAL);
    peer_timeout_interval.set_missed_tick_behavior(MissedTickBehavior::Delay);
    lag_log_interval.set_missed_tick_behavior(MissedTickBehavior::Delay);
    heartbeat_interval.set_missed_tick_behavior(MissedTickBehavior::Delay);
    peer_timeout_interval.tick().await;
    lag_log_interval.tick().await;
    heartbeat_interval.tick().await;

    info!(%connection_id, "Monitor WebSocket connected");

    let receive_task = tokio::spawn(monitor_receive_loop(
        connection_id,
        socket_rx,
        broadcast_rx,
        outgoing_tx,
        peer_timeout_interval,
        lag_log_interval,
    ));
    let send_task = tokio::spawn(monitor_send_loop(
        connection_id,
        sender,
        outgoing_rx,
        heartbeat_interval,
    ));

    let (receive_result, send_result) = tokio::join!(receive_task, send_task);

    let (lagged_total, dropped_total) = match receive_result {
        Ok(stats) => stats,
        Err(error) => {
            warn!(%connection_id, %error, "Monitor WebSocket receive task panicked");
            (0, 0)
        }
    };
    let sent_total = match send_result {
        Ok(stats) => stats,
        Err(error) => {
            warn!(%connection_id, %error, "Monitor WebSocket send task panicked");
            0
        }
    };

    debug!(
        %connection_id,
        sent_total,
        lagged_total,
        dropped_total,
        "Monitor WebSocket handler stopped"
    );
}

/// Drains the broadcast channel and forwards owned `EnrichedRecord` values into
/// the outgoing channel without ever blocking on the WebSocket sender. Exits on
/// peer timeout, socket error, Close frame, peer disconnect, or broadcast close.
async fn monitor_receive_loop<Stream, StreamError>(
    connection_id: uuid::Uuid,
    mut socket_rx: Stream,
    mut broadcast_rx: broadcast::Receiver<crate::models::enriched::EnrichedRecord>,
    outgoing_tx: mpsc::Sender<crate::models::enriched::EnrichedRecord>,
    mut peer_timeout_interval: tokio::time::Interval,
    mut lag_log_interval: tokio::time::Interval,
) -> (u64, u64)
where
    Stream: futures::Stream<Item = Result<Message, StreamError>> + Unpin,
    StreamError: std::fmt::Display,
{
    let mut last_peer_message = Instant::now();
    let mut lagged_since_last_log: u64 = 0;
    let mut lagged_total: u64 = 0;
    let mut dropped_total: u64 = 0;

    loop {
        tokio::select! {
            msg = broadcast_rx.recv() => {
                match msg {
                    Ok(record) => {
                        match outgoing_tx.try_send(record) {
                            Ok(()) => {}
                            Err(mpsc::error::TrySendError::Full(_)) => {
                                metrics::counter!("monitor_ws_outgoing_dropped").increment(1);
                                dropped_total += 1;
                            }
                            Err(mpsc::error::TrySendError::Closed(_)) => {
                                // Send task exited; connection teardown is in progress.
                                break;
                            }
                        }
                    }
                    Err(broadcast::error::RecvError::Lagged(skipped)) => {
                        lagged_since_last_log += skipped;
                        lagged_total += skipped;
                        continue;
                    }
                    Err(broadcast::error::RecvError::Closed) => {
                        warn!(%connection_id, lagged_total, dropped_total, "Monitor WebSocket broadcast channel closed");
                        break;
                    }
                }
            }
            msg = socket_rx.next() => {
                match msg {
                    Some(Ok(Message::Close(frame))) => {
                        info!(%connection_id, ?frame, lagged_total, dropped_total, "Monitor WebSocket closed by peer");
                        break;
                    }
                    Some(Ok(Message::Ping(_))) | Some(Ok(Message::Pong(_))) => {
                        last_peer_message = Instant::now();
                    }
                    Some(Ok(Message::Text(_))) | Some(Ok(Message::Binary(_))) => {
                        last_peer_message = Instant::now();
                    }
                    Some(Err(error)) => {
                        warn!(%connection_id, %error, lagged_total, dropped_total, "Monitor WebSocket receive failed");
                        break;
                    }
                    None => {
                        info!(%connection_id, lagged_total, dropped_total, "Monitor WebSocket peer disconnected");
                        break;
                    }
                }
            }
            _ = peer_timeout_interval.tick() => {
                let idle_for = last_peer_message.elapsed();
                if idle_for >= MONITOR_WS_PEER_TIMEOUT {
                    warn!(
                        %connection_id,
                        lagged_total,
                        dropped_total,
                        idle_for_ms = idle_for.as_millis() as u64,
                        "Monitor WebSocket peer timed out"
                    );
                    break;
                }
            }
            _ = lag_log_interval.tick() => {
                if lagged_since_last_log > 0 {
                    warn!(
                        %connection_id,
                        lagged_since_last_log,
                        lagged_total,
                        dropped_total,
                        "Monitor WebSocket receiver lagged behind broadcast ring"
                    );
                    lagged_since_last_log = 0;
                }
            }
        }
    }

    (lagged_total, dropped_total)
}

/// Owns the WebSocket sender, serializes `EnrichedRecord` values, and sends
/// heartbeats with biased priority. There is no send timeout: sends block
/// naturally while the monitor is slow, and the bounded outgoing channel
/// absorbs short stalls. Exits when the outgoing channel closes (receive task
/// gone) or a send fails.
async fn monitor_send_loop<Sink, SinkError>(
    connection_id: uuid::Uuid,
    mut sender: Sink,
    mut outgoing_rx: mpsc::Receiver<crate::models::enriched::EnrichedRecord>,
    mut heartbeat_interval: tokio::time::Interval,
) -> u64
where
    Sink: futures::Sink<Message, Error = SinkError> + Unpin,
    SinkError: std::fmt::Display,
{
    let mut sent_total: u64 = 0;

    loop {
        tokio::select! {
            biased;
            _ = heartbeat_interval.tick() => {
                if sender
                    .send(Message::Ping(b"jetstream-turbo".to_vec()))
                    .await
                    .is_err()
                {
                    warn!(%connection_id, sent_total, "Monitor WebSocket heartbeat send failed");
                    break;
                }
            }
            record = outgoing_rx.recv() => {
                match record {
                    Some(record) => {
                        let json = match serde_json::to_string(&record) {
                            Ok(json) => json,
                            Err(error) => {
                                warn!(%connection_id, %error, "Monitor WebSocket record serialization failed");
                                continue;
                            }
                        };
                        if sender.send(Message::Text(json)).await.is_err() {
                            warn!(%connection_id, sent_total, "Monitor WebSocket send failed");
                            break;
                        }
                        sent_total += 1;
                    }
                    None => {
                        info!(%connection_id, sent_total, "Monitor WebSocket outgoing channel closed");
                        break;
                    }
                }
            }
        }
    }

    sent_total
}

pub async fn create_server(
    port: u16,
    turbocharger: Arc<ProductionTurboCharger>,
) -> TurboResult<()> {
    let readiness_turbocharger = Arc::clone(&turbocharger);
    let app = Router::new()
        .nest("/api/v1", create_router(turbocharger))
        .route("/", get(|| async { "jetstream-turbo API server" }))
        .route(
            "/ready",
            get(move || {
                let turbocharger = Arc::clone(&readiness_turbocharger);
                async move {
                    match turbocharger.health_check().await {
                        Ok(status) => readiness_http_status(&status),
                        Err(_) => StatusCode::INTERNAL_SERVER_ERROR,
                    }
                }
            }),
        );

    let listener = tokio::net::TcpListener::bind(format!("0.0.0.0:{port}"))
        .await
        .map_err(|e| TurboError::Io(Box::new(e)))?;

    info!("Starting HTTP server on port {}", port);

    axum::serve(listener, app)
        .await
        .map_err(|e| TurboError::Io(Box::new(std::io::Error::other(e))))?;

    Ok(())
}

fn readiness_http_status(status: &HealthStatus) -> StatusCode {
    if status.healthy {
        StatusCode::OK
    } else {
        StatusCode::SERVICE_UNAVAILABLE
    }
}

fn health_http_response(status: HealthStatus) -> (StatusCode, HealthResponse) {
    let status_code = StatusCode::OK;
    let response_status = if status.healthy {
        "healthy"
    } else {
        "unhealthy"
    };

    (
        status_code,
        HealthResponse {
            status: response_status.to_string(),
            data: status,
        },
    )
}

fn prometheus_metrics_from_diagnostics(diagnostics: &HealthDiagnostics) -> String {
    let mut output = String::new();

    append_gauge_metric(
        &mut output,
        "jetstream_turbo_process_memory_rss_bytes",
        "Current process resident memory in bytes.",
        optional_u64_metric_value(diagnostics.process_memory.rss_bytes),
    );
    append_gauge_metric(
        &mut output,
        "jetstream_turbo_process_memory_virtual_bytes",
        "Current process virtual memory in bytes.",
        optional_u64_metric_value(diagnostics.process_memory.virtual_memory_bytes),
    );
    append_gauge_metric(
        &mut output,
        "jetstream_turbo_process_memory_peak_window_seconds",
        "Rolling memory-peak window size in seconds.",
        diagnostics
            .process_memory
            .peaks_24h
            .window_seconds
            .to_string(),
    );
    append_gauge_metric(
        &mut output,
        "jetstream_turbo_process_memory_samples_24h",
        "Number of in-process memory samples retained in the 24h peak window.",
        diagnostics
            .process_memory
            .peaks_24h
            .samples_collected
            .to_string(),
    );
    append_gauge_metric(
        &mut output,
        "jetstream_turbo_process_memory_latest_sample_age_seconds",
        "Age in seconds of the most recent in-process memory sample.",
        optional_u64_metric_value(
            diagnostics
                .process_memory
                .peaks_24h
                .latest_sample_age_seconds,
        ),
    );
    append_gauge_metric(
        &mut output,
        "jetstream_turbo_process_memory_rss_peak_24h_bytes",
        "Highest resident memory sample seen in the rolling 24h window.",
        optional_u64_metric_value(diagnostics.process_memory.peaks_24h.rss_peak_bytes),
    );
    append_gauge_metric(
        &mut output,
        "jetstream_turbo_process_memory_rss_peak_24h_unix_seconds",
        "Unix timestamp for when the rolling 24h RSS peak was observed.",
        optional_u64_metric_value(diagnostics.process_memory.peaks_24h.rss_peak_unix_seconds),
    );
    append_gauge_metric(
        &mut output,
        "jetstream_turbo_process_memory_virtual_peak_24h_bytes",
        "Highest virtual memory sample seen in the rolling 24h window.",
        optional_u64_metric_value(
            diagnostics
                .process_memory
                .peaks_24h
                .virtual_memory_peak_bytes,
        ),
    );
    append_gauge_metric(
        &mut output,
        "jetstream_turbo_process_memory_virtual_peak_24h_unix_seconds",
        "Unix timestamp for when the rolling 24h virtual-memory peak was observed.",
        optional_u64_metric_value(
            diagnostics
                .process_memory
                .peaks_24h
                .virtual_memory_peak_unix_seconds,
        ),
    );
    append_gauge_metric(
        &mut output,
        "jetstream_turbo_runtime_memory_samples_retained",
        "Number of fixed-cadence runtime-memory samples retained in process.",
        diagnostics.runtime_memory.samples_retained.to_string(),
    );
    append_gauge_metric(
        &mut output,
        "jetstream_turbo_runtime_memory_sample_capacity",
        "Hard maximum number of runtime-memory samples retained in process.",
        diagnostics.runtime_memory.sample_capacity.to_string(),
    );
    append_gauge_metric(
        &mut output,
        "jetstream_turbo_runtime_memory_sample_interval_seconds",
        "Configured independent runtime-memory sampling interval.",
        diagnostics
            .runtime_memory
            .sample_interval_seconds
            .to_string(),
    );
    append_gauge_metric(
        &mut output,
        "jetstream_turbo_runtime_memory_latest_sample_age_seconds",
        "Age of the latest independent runtime-memory sample.",
        optional_u64_metric_value(diagnostics.runtime_memory.latest_sample_age_seconds),
    );
    output.push_str("# HELP jetstream_turbo_memory_pressure_state_info Current bounded memory-pressure state.\n");
    output.push_str("# TYPE jetstream_turbo_memory_pressure_state_info gauge\n");
    output.push_str(&format!(
        "jetstream_turbo_memory_pressure_state_info{{state=\"{}\"}} 1\n",
        diagnostics.runtime_memory.pressure_state.as_str()
    ));
    if let Some(envelope) = diagnostics.runtime_memory.envelope {
        output.push_str("# HELP jetstream_turbo_memory_threshold_bytes Configured runtime-memory envelope threshold.\n");
        output.push_str("# TYPE jetstream_turbo_memory_threshold_bytes gauge\n");
        for (threshold, value) in [
            ("recovery", envelope.recovery_bytes),
            ("soft_pressure", envelope.soft_pressure_bytes),
            ("emergency", envelope.emergency_bytes),
            ("external_hard_limit", envelope.external_hard_limit_bytes),
        ] {
            output.push_str(&format!(
                "jetstream_turbo_memory_threshold_bytes{{threshold=\"{threshold}\"}} {value}\n"
            ));
        }
    }
    if let Some(sample) = diagnostics.runtime_memory.latest_sample.as_ref() {
        output.push_str(
            "# HELP jetstream_turbo_memory_workload_phase_info Current bounded workload phase.\n",
        );
        output.push_str("# TYPE jetstream_turbo_memory_workload_phase_info gauge\n");
        output.push_str(&format!(
            "jetstream_turbo_memory_workload_phase_info{{phase=\"{}\"}} 1\n",
            sample.phase.as_str()
        ));
        for (name, help, value) in [
            (
                "jetstream_turbo_process_memory_anonymous_rss_bytes",
                "Current anonymous process RSS in bytes.",
                optional_u64_metric_value(sample.process.anonymous_rss_bytes),
            ),
            (
                "jetstream_turbo_process_memory_file_rss_bytes",
                "Current file-backed process RSS in bytes.",
                optional_u64_metric_value(sample.process.file_rss_bytes),
            ),
            (
                "jetstream_turbo_process_memory_shared_rss_bytes",
                "Current shared process RSS in bytes.",
                optional_u64_metric_value(sample.process.shared_rss_bytes),
            ),
            (
                "jetstream_turbo_process_memory_swap_bytes",
                "Current process swap usage in bytes.",
                optional_u64_metric_value(sample.process.swap_bytes),
            ),
            (
                "jetstream_turbo_cgroup_memory_current_bytes",
                "Current cgroup memory usage in bytes.",
                optional_u64_metric_value(sample.cgroup.current_bytes),
            ),
            (
                "jetstream_turbo_cgroup_memory_high_bytes",
                "Configured cgroup memory.high value in bytes.",
                optional_u64_metric_value(sample.cgroup.high_bytes),
            ),
            (
                "jetstream_turbo_cgroup_memory_max_bytes",
                "Configured cgroup memory.max value in bytes.",
                optional_u64_metric_value(sample.cgroup.max_bytes),
            ),
            (
                "jetstream_turbo_cgroup_oom_events_total",
                "Cgroup OOM events observed by the kernel.",
                optional_u64_metric_value(sample.cgroup.events.oom),
            ),
            (
                "jetstream_turbo_cgroup_oom_kills_total",
                "Cgroup OOM kills observed by the kernel.",
                optional_u64_metric_value(sample.cgroup.events.oom_kill),
            ),
        ] {
            append_gauge_metric(&mut output, name, help, value);
        }
        append_gauge_metric(
            &mut output,
            "jetstream_turbo_cgroup_memory_pressure_some_avg10",
            "Cgroup memory PSI some-stall average over ten seconds.",
            optional_f64_metric_value(sample.cgroup.pressure_some_avg10),
        );
        append_gauge_metric(
            &mut output,
            "jetstream_turbo_cgroup_memory_pressure_full_avg10",
            "Cgroup memory PSI full-stall average over ten seconds.",
            optional_f64_metric_value(sample.cgroup.pressure_full_avg10),
        );
        output.push_str("# HELP jetstream_turbo_memory_component_bytes Estimated retained bytes by bounded component.\n");
        output.push_str("# TYPE jetstream_turbo_memory_component_bytes gauge\n");
        output.push_str("# HELP jetstream_turbo_memory_component_limit_bytes Configured byte limit by bounded component.\n");
        output.push_str("# TYPE jetstream_turbo_memory_component_limit_bytes gauge\n");
        for (component, current, limit) in [
            (
                "user_cache",
                sample.components.user_cache_bytes,
                sample.components.user_cache_limit_bytes,
            ),
            (
                "post_cache",
                sample.components.post_cache_bytes,
                sample.components.post_cache_limit_bytes,
            ),
            (
                "negative_cache",
                sample.components.negative_cache_bytes,
                sample.components.negative_cache_limit_bytes,
            ),
            (
                "input_channel",
                sample.components.input_channel_bytes,
                sample.components.input_channel_limit_bytes,
            ),
            (
                "in_flight_payload",
                sample.components.in_flight_payload_bytes,
                sample.components.in_flight_payload_limit_bytes,
            ),
            (
                "monitor_broadcast",
                sample.components.monitor_broadcast_bytes,
                sample.components.monitor_broadcast_limit_bytes,
            ),
        ] {
            output.push_str(&format!(
                "jetstream_turbo_memory_component_bytes{{component=\"{component}\"}} {current}\n"
            ));
            output.push_str(&format!(
                "jetstream_turbo_memory_component_limit_bytes{{component=\"{component}\"}} {limit}\n"
            ));
        }
        append_gauge_metric(
            &mut output,
            "jetstream_turbo_memory_coordination_bytes",
            "Conservative current byte estimate for active hydration coordination.",
            sample.components.coordination_bytes.to_string(),
        );
        append_gauge_metric(
            &mut output,
            "jetstream_turbo_sqlx_pool_connections",
            "Current SQLx SQLite pool size.",
            sample.components.sqlx_connections.to_string(),
        );
        append_gauge_metric(
            &mut output,
            "jetstream_turbo_sqlx_pool_idle_connections",
            "Current idle SQLx SQLite connections.",
            sample.components.sqlx_idle_connections.to_string(),
        );
        append_gauge_metric(
            &mut output,
            "jetstream_turbo_sqlx_pool_max_connections",
            "Configured maximum SQLx SQLite connections.",
            sample.components.sqlx_max_connections.to_string(),
        );
        append_gauge_metric(
            &mut output,
            "jetstream_turbo_sqlite_cache_bytes_per_connection",
            "Configured SQLite page-cache byte ceiling per connection.",
            sample
                .components
                .sqlite_cache_bytes_per_connection
                .to_string(),
        );
        append_gauge_metric(
            &mut output,
            "jetstream_turbo_sqlite_mmap_limit_bytes",
            "Configured SQLite mmap byte ceiling.",
            sample.components.sqlite_mmap_bytes.to_string(),
        );
        output.push_str("# HELP jetstream_turbo_sqlite_temp_store_info Current bounded SQLite temp-store mode.\n");
        output.push_str("# TYPE jetstream_turbo_sqlite_temp_store_info gauge\n");
        output.push_str(&format!(
            "jetstream_turbo_sqlite_temp_store_info{{mode=\"{}\"}} 1\n",
            escape_prometheus_label(&sample.components.sqlite_temp_store)
        ));
    }
    append_gauge_metric(
        &mut output,
        "jetstream_turbo_cache_user_entries",
        "Current number of user profile entries in cache.",
        diagnostics.cache_state.user_entries.to_string(),
    );
    append_gauge_metric(
        &mut output,
        "jetstream_turbo_cache_post_entries",
        "Current number of post entries in cache.",
        diagnostics.cache_state.post_entries.to_string(),
    );
    append_gauge_metric(
        &mut output,
        "jetstream_turbo_cache_user_capacity",
        "Configured maximum number of user profile cache entries.",
        diagnostics.cache_state.user_capacity.to_string(),
    );
    append_gauge_metric(
        &mut output,
        "jetstream_turbo_cache_post_capacity",
        "Configured maximum number of post cache entries.",
        diagnostics.cache_state.post_capacity.to_string(),
    );
    append_gauge_metric(
        &mut output,
        "jetstream_turbo_optional_hydration_negative_cache_entries",
        "Active temporarily unavailable referenced-post cache entries.",
        diagnostics.cache_state.negative_post_entries.to_string(),
    );
    append_gauge_metric(
        &mut output,
        "jetstream_turbo_optional_hydration_negative_cache_capacity",
        "Configured capacity for temporarily unavailable referenced posts.",
        diagnostics.cache_state.negative_post_capacity.to_string(),
    );
    append_gauge_metric(
        &mut output,
        "jetstream_turbo_optional_hydration_partial_records",
        "Durably stored records with partial optional hydration.",
        optional_i64_metric_value(diagnostics.sqlite_state.partial_records),
    );
    output.push_str("# HELP jetstream_turbo_optional_hydration_post_outcomes_total Referenced-post fetch outcomes by bounded result kind.\n");
    output.push_str("# TYPE jetstream_turbo_optional_hydration_post_outcomes_total counter\n");
    for (outcome, value) in [
        ("found", diagnostics.cache_state.post_found),
        ("missing", diagnostics.cache_state.post_missing),
        (
            "temporarily_unavailable",
            diagnostics.cache_state.post_unavailable,
        ),
    ] {
        output.push_str(&format!(
            "jetstream_turbo_optional_hydration_post_outcomes_total{{outcome=\"{outcome}\"}} {value}\n"
        ));
    }
    append_counter_metric(
        &mut output,
        "jetstream_turbo_optional_hydration_partial_records_total",
        "Records produced with partial optional hydration.",
        diagnostics.cache_state.partial_records_total,
    );
    append_counter_metric(
        &mut output,
        "jetstream_turbo_optional_hydration_negative_cache_hits_total",
        "Referenced-post requests suppressed by the active negative cache.",
        diagnostics.cache_state.negative_post_hits,
    );
    append_counter_metric(
        &mut output,
        "jetstream_turbo_optional_hydration_negative_cache_evictions_total",
        "Negative referenced-post entries evicted at capacity.",
        diagnostics.cache_state.negative_post_evictions,
    );
    append_counter_metric(
        &mut output,
        "jetstream_turbo_optional_hydration_recoveries_total",
        "Referenced posts that resolved after temporary unavailability.",
        diagnostics.cache_state.post_recoveries,
    );
    output.push_str("# HELP jetstream_turbo_optional_hydration_isolation_outcomes_total Bounded post-isolation classifications.\n");
    output.push_str("# TYPE jetstream_turbo_optional_hydration_isolation_outcomes_total counter\n");
    for (outcome, value) in [
        (
            "broad_outage",
            diagnostics.cache_state.isolation_broad_outage,
        ),
        (
            "singleton_poison",
            diagnostics.cache_state.isolation_singleton_poison,
        ),
        (
            "budget_exhausted",
            diagnostics.cache_state.isolation_budget_exhausted,
        ),
    ] {
        output.push_str(&format!(
            "jetstream_turbo_optional_hydration_isolation_outcomes_total{{outcome=\"{outcome}\"}} {value}\n"
        ));
    }
    append_gauge_metric(
        &mut output,
        "jetstream_turbo_sqlite_available",
        "Whether SQLite is currently available (1 = yes, 0 = no).",
        bool_metric_value(diagnostics.sqlite_state.available),
    );
    append_gauge_metric(
        &mut output,
        "jetstream_turbo_sqlite_db_size_bytes",
        "Current SQLite database file size in bytes.",
        optional_i64_metric_value(diagnostics.sqlite_state.db_size_bytes),
    );
    append_gauge_metric(
        &mut output,
        "jetstream_turbo_db_size_bytes",
        "Current SQLite database file size in bytes.",
        optional_i64_metric_value(diagnostics.sqlite_state.db_size_bytes),
    );
    append_gauge_metric(
        &mut output,
        "jetstream_turbo_db_freelist_ratio",
        "Ratio of freelist pages to total pages in the SQLite database.",
        optional_f64_metric_value(diagnostics.sqlite_state.freelist_ratio),
    );
    append_gauge_metric(
        &mut output,
        "jetstream_turbo_vacuum_pending",
        "Whether a VACUUM is scheduled and waiting (1 = yes, 0 = no).",
        optional_bool_metric_value(diagnostics.sqlite_state.vacuum_pending),
    );
    append_gauge_metric(
        &mut output,
        "jetstream_turbo_vacuum_last_duration_ms",
        "Duration of the most recent completed VACUUM in milliseconds.",
        optional_u64_metric_value(diagnostics.sqlite_state.vacuum_last_run_duration_ms),
    );
    append_gauge_metric(
        &mut output,
        "jetstream_turbo_db_over_budget",
        "Whether the database file exceeds the configured maximum size (1 = yes, 0 = no).",
        optional_bool_metric_value(diagnostics.sqlite_state.over_budget),
    );
    append_gauge_metric(
        &mut output,
        "jetstream_turbo_sqlite_wal_size_bytes",
        "Current SQLite WAL file size in bytes.",
        optional_i64_metric_value(diagnostics.sqlite_state.wal_size_bytes),
    );
    append_gauge_metric(
        &mut output,
        "jetstream_turbo_not_redis_connected",
        "Whether not_redis is currently reachable (1 = yes, 0 = no).",
        bool_metric_value(diagnostics.not_redis_state.connected),
    );
    append_gauge_metric(
        &mut output,
        "jetstream_turbo_not_redis_stream_length",
        "Current not_redis stream length.",
        optional_usize_metric_value(diagnostics.not_redis_state.stream_length),
    );
    append_gauge_metric(
        &mut output,
        "jetstream_turbo_not_redis_configured_max_length",
        "Configured not_redis stream trim max length.",
        optional_usize_metric_value(diagnostics.not_redis_state.configured_max_length),
    );
    append_gauge_metric(
        &mut output,
        "jetstream_turbo_pipeline_ingress_age_seconds",
        "Age of the most recent valid Jetstream message.",
        optional_u64_metric_value(diagnostics.pipeline_progress.ingress_age_seconds),
    );
    append_gauge_metric(
        &mut output,
        "jetstream_turbo_pipeline_completion_age_seconds",
        "Age of the most recent successful batch completion.",
        optional_u64_metric_value(diagnostics.pipeline_progress.completion_age_seconds),
    );
    append_gauge_metric(
        &mut output,
        "jetstream_turbo_pipeline_active_batches",
        "Current active batch count.",
        diagnostics.pipeline_progress.active_permits.to_string(),
    );
    append_gauge_metric(
        &mut output,
        "jetstream_turbo_pipeline_maximum_batches",
        "Configured maximum concurrent batch count.",
        diagnostics.pipeline_progress.maximum_permits.to_string(),
    );
    append_gauge_metric(
        &mut output,
        "jetstream_turbo_pipeline_oldest_batch_age_seconds",
        "Age of the oldest active batch.",
        optional_u64_metric_value(
            diagnostics
                .pipeline_progress
                .oldest_active_batch_age_seconds,
        ),
    );
    append_gauge_metric(
        &mut output,
        "jetstream_turbo_pipeline_ingress_messages_total",
        "Valid ingress messages observed.",
        diagnostics.pipeline_progress.ingress_messages.to_string(),
    );
    append_gauge_metric(
        &mut output,
        "jetstream_turbo_pipeline_rejected_ingress_total",
        "Cursorless in-scope ingress events rejected before pipeline processing.",
        diagnostics.pipeline_progress.rejected_ingress.to_string(),
    );
    append_gauge_metric(
        &mut output,
        "jetstream_turbo_pipeline_completed_records_total",
        "Records in successfully completed batches.",
        diagnostics.pipeline_progress.completed_records.to_string(),
    );
    append_gauge_metric(
        &mut output,
        "jetstream_turbo_pipeline_batch_timeouts_total",
        "Batches stopped by their execution deadline.",
        diagnostics.pipeline_progress.timed_out_batches.to_string(),
    );
    append_gauge_metric(
        &mut output,
        "jetstream_turbo_pipeline_batch_failures_total",
        "Batches finalized after a processing failure.",
        diagnostics.pipeline_progress.failed_batches.to_string(),
    );
    append_gauge_metric(
        &mut output,
        "jetstream_turbo_pipeline_batch_aborts_total",
        "Batches finalized by cancellation or task abandonment.",
        diagnostics.pipeline_progress.aborted_batches.to_string(),
    );
    append_gauge_metric(
        &mut output,
        "jetstream_turbo_pipeline_input_drops_total",
        "Messages dropped at the saturated input boundary.",
        diagnostics.pipeline_progress.input_drops.to_string(),
    );
    append_gauge_metric(
        &mut output,
        "jetstream_turbo_pipeline_broadcast_receivers",
        "Current monitor broadcast receiver count.",
        diagnostics
            .pipeline_progress
            .broadcast_receivers
            .to_string(),
    );
    append_gauge_metric(
        &mut output,
        "jetstream_turbo_pipeline_successful_broadcasts_total",
        "Successful sends into the monitor broadcast channel.",
        diagnostics
            .pipeline_progress
            .successful_broadcasts
            .to_string(),
    );
    append_gauge_metric(
        &mut output,
        "jetstream_turbo_pipeline_ready",
        "Whether progress is currently healthy (1 = healthy, 0 = otherwise).",
        if diagnostics.pipeline_progress.readiness_state
            == crate::turbocharger::PipelineReadinessState::Healthy
        {
            "1"
        } else {
            "0"
        }
        .to_string(),
    );
    append_gauge_metric(
        &mut output,
        "jetstream_turbo_failure_containment_active",
        "Whether a run-loop failure incident is active (1 = yes, 0 = no).",
        bool_metric_value(diagnostics.failure_containment.active),
    );
    append_gauge_metric(
        &mut output,
        "jetstream_turbo_failure_containment_persistent",
        "Whether an active incident crossed the persistence threshold (1 = yes, 0 = no).",
        bool_metric_value(diagnostics.failure_containment.persistent),
    );
    append_gauge_metric(
        &mut output,
        "jetstream_turbo_failure_containment_recurrence",
        "Current recurrence count for the active safe failure fingerprint.",
        diagnostics.failure_containment.recurrence.to_string(),
    );
    append_gauge_metric(
        &mut output,
        "jetstream_turbo_failure_containment_delay_milliseconds",
        "Current bounded run-loop recovery delay in milliseconds.",
        optional_u64_metric_value(diagnostics.failure_containment.current_delay_ms),
    );
    if let (Some(subtype), Some(stage)) = (
        diagnostics.failure_containment.subtype,
        diagnostics.failure_containment.stage,
    ) {
        output.push_str("# HELP jetstream_turbo_failure_containment_info Bounded identity of the active failure incident.\n");
        output.push_str("# TYPE jetstream_turbo_failure_containment_info gauge\n");
        output.push_str(&format!(
            "jetstream_turbo_failure_containment_info{{subtype=\"{}\",stage=\"{}\",boundary=\"{}\"}} 1\n",
            subtype.as_str(),
            stage.as_str(),
            if diagnostics.failure_containment.boundary_present {
                "present"
            } else {
                "absent"
            }
        ));
    }
    append_gauge_metric(
        &mut output,
        "jetstream_turbo_committed_source_velocity",
        "Smoothed committed source seconds advanced per wall second.",
        optional_f64_metric_value(diagnostics.pipeline_progress.committed_source_velocity),
    );
    append_gauge_metric(
        &mut output,
        "jetstream_turbo_net_convergence_rate",
        "Committed source velocity above real time (positive means converging).",
        optional_f64_metric_value(diagnostics.pipeline_progress.net_convergence_rate),
    );
    append_gauge_metric(
        &mut output,
        "jetstream_turbo_catch_up_eta_seconds",
        "Estimated seconds to converge, available only for stable positive convergence.",
        optional_u64_metric_value(diagnostics.pipeline_progress.catch_up_eta_seconds),
    );
    append_gauge_metric(
        &mut output,
        "jetstream_turbo_running_permit_holders",
        "Batches currently holding a concurrency permit.",
        diagnostics
            .pipeline_progress
            .running_permit_holders
            .to_string(),
    );
    append_gauge_metric(
        &mut output,
        "jetstream_turbo_queued_batches",
        "Batches waiting to acquire a concurrency permit.",
        diagnostics.pipeline_progress.queued_batches.to_string(),
    );
    append_gauge_metric(
        &mut output,
        "jetstream_turbo_batch_completion_throughput_per_second",
        "Batch completions per second over the bounded recent sample window.",
        diagnostics
            .pipeline_progress
            .batch_completion_throughput_per_second
            .to_string(),
    );
    append_runtime_identity_metrics(&mut output, &diagnostics.runtime_identity);
    append_completion_frontier_metrics(&mut output, &diagnostics.completion_frontier);
    append_stage_timing_metrics(&mut output, &diagnostics.pipeline_progress.stage_timings);
    append_bluesky_fetch_metrics(&mut output, &diagnostics.bluesky_fetch);
    append_bluesky_coordination_metrics(&mut output, &diagnostics.bluesky_coordination);

    output.push_str("# HELP jetstream_turbo_reconnects_total Upstream reconnects grouped by initiating reason.\n");
    output.push_str("# TYPE jetstream_turbo_reconnects_total counter\n");
    let mut reconnect_reasons: Vec<_> = diagnostics
        .pipeline_progress
        .reconnect_reasons
        .iter()
        .collect();
    reconnect_reasons.sort_by_key(|(reason, _)| reason.as_str());
    for (reason, count) in reconnect_reasons {
        output.push_str(&format!(
            "jetstream_turbo_reconnects_total{{reason=\"{reason}\"}} {count}\n"
        ));
    }
    output.push_str("# HELP jetstream_turbo_ingress_rejections_total Rejected ingress events grouped by bounded reason and kind.\n");
    output.push_str("# TYPE jetstream_turbo_ingress_rejections_total counter\n");
    let mut kinds: Vec<_> = diagnostics
        .pipeline_progress
        .rejected_ingress_kinds
        .iter()
        .collect();
    kinds.sort_by_key(|(kind, _)| kind.as_str());
    for (kind, count) in kinds {
        output.push_str(&format!(
            "jetstream_turbo_ingress_rejections_total{{reason=\"missing_time_us\",kind=\"{kind}\"}} {count}\n"
        ));
    }

    output
}

fn append_runtime_identity_metrics(
    output: &mut String,
    identity: &crate::turbocharger::RuntimeIdentityDiagnostics,
) {
    append_gauge_metric(
        output,
        "jetstream_turbo_process_start_time_seconds",
        "Unix start time of this process epoch; cumulative counters reset when it changes.",
        identity.process_started_at_unix_seconds.to_string(),
    );
    output.push_str(
        "# HELP jetstream_turbo_release_info Release identity of the running process epoch.\n",
    );
    output.push_str("# TYPE jetstream_turbo_release_info gauge\n");
    match identity.release.identifier.as_deref() {
        Some(identifier) => output.push_str(&format!(
            "jetstream_turbo_release_info{{release=\"{}\",available=\"1\"}} 1\n",
            escape_prometheus_label(identifier)
        )),
        None => output.push_str("jetstream_turbo_release_info{available=\"0\"} 1\n"),
    }
    let termination = &identity.previous_termination;
    let classification = termination
        .classification
        .map(|classification| classification.as_str())
        .unwrap_or("unavailable");
    output.push_str("# HELP jetstream_turbo_previous_termination_info Previous-termination evidence for this process epoch.\n");
    output.push_str("# TYPE jetstream_turbo_previous_termination_info gauge\n");
    output.push_str(&format!(
        "jetstream_turbo_previous_termination_info{{state=\"{}\",class=\"{}\"}} 1\n",
        termination.state.as_str(),
        classification,
    ));
}

fn append_completion_frontier_metrics(
    output: &mut String,
    frontier: &crate::turbocharger::coordinator::CompletionFrontierSnapshot,
) {
    append_gauge_metric(
        output,
        "jetstream_turbo_frontier_pending_ranges",
        "Completed ingress ranges retained pending contiguous checkpoint advancement.",
        frontier.pending_range_count.to_string(),
    );
    append_gauge_metric(
        output,
        "jetstream_turbo_frontier_next_required_ordinal",
        "Next ingress ordinal required to advance the durable checkpoint.",
        frontier.next_required_ordinal.to_string(),
    );
    append_gauge_metric(
        output,
        "jetstream_turbo_frontier_furthest_completed_ordinal",
        "Furthest successfully completed ingress ordinal.",
        optional_u64_metric_value(frontier.furthest_completed_ordinal),
    );
    append_gauge_metric(
        output,
        "jetstream_turbo_frontier_durable_checkpoint_ordinal",
        "Durable checkpoint ingress ordinal (process-lifetime value, not reset by restarts).",
        optional_u64_metric_value(frontier.durable_checkpoint_ordinal),
    );
    append_gauge_metric(
        output,
        "jetstream_turbo_frontier_unresolved_gap_age_seconds",
        "Age of the current unresolved contiguous frontier gap; resets on advancement.",
        optional_u64_metric_value(frontier.unresolved_gap_age_seconds),
    );
}

fn append_stage_timing_metrics(
    output: &mut String,
    stage_timings: &[crate::turbocharger::StageTimingSnapshot],
) {
    output.push_str("# HELP jetstream_turbo_pipeline_stage_duration_seconds Cumulative per-stage batch execution time by bounded outcome.\n");
    output.push_str("# TYPE jetstream_turbo_pipeline_stage_duration_seconds histogram\n");
    let mut timings: Vec<&crate::turbocharger::StageTimingSnapshot> =
        stage_timings.iter().collect();
    timings.sort_by_key(|timing| (timing.stage as u8, timing.outcome as u8));
    for timing in timings {
        output.push_str(&format!(
            "jetstream_turbo_pipeline_stage_duration_seconds_sum{{stage=\"{}\",outcome=\"{}\"}} {:.6}\n\
             jetstream_turbo_pipeline_stage_duration_seconds_count{{stage=\"{}\",outcome=\"{}\"}} {}\n",
            timing.stage.as_str(),
            timing.outcome.as_str(),
            timing.duration_milliseconds_total as f64 / 1_000.0,
            timing.stage.as_str(),
            timing.outcome.as_str(),
            timing.count,
        ));
    }
}

fn append_bluesky_fetch_metrics(
    output: &mut String,
    fetch: &crate::client::BlueskyFetchDiagnostics,
) {
    append_fetch_kind_metrics(output, "profiles", &fetch.profiles);
    append_fetch_kind_metrics(output, "posts", &fetch.posts);
}

fn append_fetch_kind_metrics(
    output: &mut String,
    kind: &str,
    fetch: &crate::client::BlueskyFetchKindDiagnostics,
) {
    output.push_str(&format!(
        "# HELP jetstream_turbo_bluesky_api_requests_total Bluesky API requests by collector kind (rate = requests/sec).\n\
         # TYPE jetstream_turbo_bluesky_api_requests_total counter\n\
         jetstream_turbo_bluesky_api_requests_total{{kind=\"{kind}\"}} {}\n",
        fetch.requests_total
    ));
    output.push_str(&format!(
        "# HELP jetstream_turbo_bluesky_request_items_total Identifiers submitted across Bluesky API requests by kind (items per request = Δitems/Δrequests).\n\
         # TYPE jetstream_turbo_bluesky_request_items_total counter\n\
         jetstream_turbo_bluesky_request_items_total{{kind=\"{kind}\"}} {}\n",
        fetch.items_total
    ));
    output.push_str(&format!(
        "# HELP jetstream_turbo_bluesky_fetch_lock_duration_seconds Wall time per fetch operation: waiting for the collector lock plus holding it during resolution.\n\
         # TYPE jetstream_turbo_bluesky_fetch_lock_duration_seconds histogram\n\
         jetstream_turbo_bluesky_fetch_lock_duration_seconds_sum{{kind=\"{kind}\"}} {:.6}\n\
         jetstream_turbo_bluesky_fetch_lock_duration_seconds_count{{kind=\"{kind}\"}} {}\n",
        fetch.lock_duration_ns_total as f64 / 1_000_000_000.0,
        fetch.lock_duration_count
    ));
    output.push_str(&format!(
        "# HELP jetstream_turbo_bluesky_fetch_http_duration_seconds Wall time spent in the Bluesky HTTP fetch chain (incl. retries) per request.\n\
         # TYPE jetstream_turbo_bluesky_fetch_http_duration_seconds histogram\n\
         jetstream_turbo_bluesky_fetch_http_duration_seconds_sum{{kind=\"{kind}\"}} {:.6}\n\
         jetstream_turbo_bluesky_fetch_http_duration_seconds_count{{kind=\"{kind}\"}} {}\n",
        fetch.http_duration_ns_total as f64 / 1_000_000_000.0,
        fetch.http_duration_count
    ));
    output.push_str(&format!(
        "# HELP jetstream_turbo_bluesky_rate_limiter_wait_seconds Wall time spent waiting on the shared upstream rate limiter per attempt.\n\
         # TYPE jetstream_turbo_bluesky_rate_limiter_wait_seconds histogram\n\
         jetstream_turbo_bluesky_rate_limiter_wait_seconds_sum{{kind=\"{kind}\"}} {:.6}\n\
         jetstream_turbo_bluesky_rate_limiter_wait_seconds_count{{kind=\"{kind}\"}} {}\n",
        fetch.rate_limiter_wait_ns_total as f64 / 1_000_000_000.0,
        fetch.rate_limiter_wait_count
    ));
    output.push_str(&format!(
        "# HELP jetstream_turbo_bluesky_local_assembly_seconds Wall time spent decoding responses and assembling results locally per request.\n\
         # TYPE jetstream_turbo_bluesky_local_assembly_seconds histogram\n\
         jetstream_turbo_bluesky_local_assembly_seconds_sum{{kind=\"{kind}\"}} {:.6}\n\
         jetstream_turbo_bluesky_local_assembly_seconds_count{{kind=\"{kind}\"}} {}\n",
        fetch.assembly_duration_ns_total as f64 / 1_000_000_000.0,
        fetch.assembly_duration_count
    ));
    output.push_str(&format!(
        "# HELP jetstream_turbo_bluesky_fetch_errors_total Fetch-chain requests exhausted by error class (rate_limited = 429, upstream = 5xx/timeout/transport).\n\
         # TYPE jetstream_turbo_bluesky_fetch_errors_total counter\n\
         jetstream_turbo_bluesky_fetch_errors_total{{kind=\"{kind}\",class=\"rate_limited\"}} {}\n\
         jetstream_turbo_bluesky_fetch_errors_total{{kind=\"{kind}\",class=\"upstream\"}} {}\n",
        fetch.errors_rate_limited,
        fetch.errors_upstream
    ));
}

type CoordinationMetricDefinition = (
    &'static str,
    &'static str,
    &'static str,
    fn(&crate::client::CoordinationSnapshot) -> u64,
);

fn append_bluesky_coordination_metrics(
    output: &mut String,
    coordination: &crate::client::BlueskyCoordinationDiagnostics,
) {
    let kinds = [
        ("profiles", &coordination.profiles),
        ("posts", &coordination.posts),
    ];
    let metrics: [CoordinationMetricDefinition; 13] = [
        (
            "pending_keys",
            "Identifiers awaiting a Bluesky coordination claim.",
            "gauge",
            |snapshot: &crate::client::CoordinationSnapshot| snapshot.pending_keys as u64,
        ),
        (
            "in_flight_keys",
            "Identifiers currently owned by a Bluesky coordination claimant.",
            "gauge",
            |snapshot: &crate::client::CoordinationSnapshot| snapshot.in_flight_keys as u64,
        ),
        (
            "waiters",
            "Current registered Bluesky coordination waiters.",
            "gauge",
            |snapshot: &crate::client::CoordinationSnapshot| snapshot.waiters as u64,
        ),
        (
            "retained_identifier_bytes",
            "Current identifier bytes retained by Bluesky coordination.",
            "gauge",
            |snapshot: &crate::client::CoordinationSnapshot| {
                snapshot.retained_identifier_bytes as u64
            },
        ),
        (
            "key_capacity",
            "Configured hard ceiling for distinct Bluesky coordination keys.",
            "gauge",
            |snapshot: &crate::client::CoordinationSnapshot| snapshot.key_capacity as u64,
        ),
        (
            "waiter_capacity",
            "Configured hard ceiling for Bluesky coordination waiters.",
            "gauge",
            |snapshot: &crate::client::CoordinationSnapshot| snapshot.waiter_capacity as u64,
        ),
        (
            "key_high_watermark",
            "Maximum observed active Bluesky coordination keys.",
            "gauge",
            |snapshot: &crate::client::CoordinationSnapshot| snapshot.key_high_watermark as u64,
        ),
        (
            "waiter_high_watermark",
            "Maximum observed Bluesky coordination waiters.",
            "gauge",
            |snapshot: &crate::client::CoordinationSnapshot| snapshot.waiter_high_watermark as u64,
        ),
        (
            "retained_identifier_bytes_high_watermark",
            "Maximum observed identifier bytes retained by Bluesky coordination.",
            "gauge",
            |snapshot: &crate::client::CoordinationSnapshot| {
                snapshot.retained_identifier_bytes_high_watermark as u64
            },
        ),
        (
            "coalesced_waiters_total",
            "Waiters joined to already-active Bluesky coordination keys.",
            "counter",
            |snapshot: &crate::client::CoordinationSnapshot| snapshot.coalesced_waiters_total,
        ),
        (
            "completions_total",
            "Terminal Bluesky coordination key completions.",
            "counter",
            |snapshot: &crate::client::CoordinationSnapshot| snapshot.completions_total,
        ),
        (
            "cancellations_total",
            "Bluesky coordination claimant and waiter cancellations.",
            "counter",
            |snapshot: &crate::client::CoordinationSnapshot| snapshot.cancellations_total,
        ),
        (
            "failed_finalizations_total",
            "Bluesky coordination finalizations that found inconsistent state.",
            "counter",
            |snapshot: &crate::client::CoordinationSnapshot| snapshot.failed_finalizations_total,
        ),
    ];
    for (name, help, metric_type, value) in metrics {
        let metric_name = format!("jetstream_turbo_bluesky_coordination_{name}");
        output.push_str(&format!(
            "# HELP {metric_name} {help}\n# TYPE {metric_name} {metric_type}\n"
        ));
        for (kind, snapshot) in kinds {
            output.push_str(&format!(
                "{metric_name}{{kind=\"{kind}\"}} {}\n",
                value(snapshot)
            ));
        }
    }
}

fn append_gauge_metric(output: &mut String, name: &str, help: &str, value: String) {
    output.push_str("# HELP ");
    output.push_str(name);
    output.push(' ');
    output.push_str(help);
    output.push('\n');
    output.push_str("# TYPE ");
    output.push_str(name);
    output.push_str(" gauge\n");
    output.push_str(name);
    output.push(' ');
    output.push_str(&value);
    output.push('\n');
}

fn append_counter_metric(output: &mut String, name: &str, help: &str, value: u64) {
    output.push_str("# HELP ");
    output.push_str(name);
    output.push(' ');
    output.push_str(help);
    output.push('\n');
    output.push_str("# TYPE ");
    output.push_str(name);
    output.push_str(" counter\n");
    output.push_str(name);
    output.push(' ');
    output.push_str(&value.to_string());
    output.push('\n');
}

fn escape_prometheus_label(value: &str) -> String {
    value
        .replace('\\', "\\\\")
        .replace('"', "\\\"")
        .replace('\n', "\\n")
}

fn bool_metric_value(value: bool) -> String {
    if value {
        "1".to_string()
    } else {
        "0".to_string()
    }
}

fn optional_u64_metric_value(value: Option<u64>) -> String {
    value
        .map(|v| v.to_string())
        .unwrap_or_else(|| "NaN".to_string())
}

fn optional_i64_metric_value(value: Option<i64>) -> String {
    value
        .map(|v| v.to_string())
        .unwrap_or_else(|| "NaN".to_string())
}

fn optional_usize_metric_value(value: Option<usize>) -> String {
    value
        .map(|v| v.to_string())
        .unwrap_or_else(|| "NaN".to_string())
}

fn optional_f64_metric_value(value: Option<f64>) -> String {
    value
        .map(|v| v.to_string())
        .unwrap_or_else(|| "NaN".to_string())
}

fn optional_bool_metric_value(value: Option<bool>) -> String {
    match value {
        Some(true) => "1".to_string(),
        Some(false) => "0".to_string(),
        None => "NaN".to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::{
        health_http_response, monitor_receive_loop, monitor_send_loop,
        prometheus_metrics_from_diagnostics, readiness_http_status,
    };
    use crate::models::enriched::EnrichedRecord;
    use crate::models::jetstream::{CommitData, JetstreamMessage, MessageKind, OperationType};
    use crate::turbocharger::{
        CacheStateDiagnostics, HealthDiagnostics, HealthStatus, MemoryPeakDiagnostics,
        NotRedisStateDiagnostics, PipelineProgress, PipelineReadinessState,
        ProcessMemoryDiagnostics, ProgressThresholds, ReadinessDiagnostics, SQLiteStateDiagnostics,
    };
    use axum::extract::ws::Message;
    use axum::http::StatusCode;
    use futures::stream;
    use serde_json::Value;
    use std::sync::{Arc, Mutex};
    use std::time::Duration;
    use tokio::sync::{broadcast, mpsc};
    use tokio::time::{interval, MissedTickBehavior};

    fn sample_diagnostics() -> HealthDiagnostics {
        let progress = PipelineProgress::new(6, 10_000);
        let _ = progress.valid_ingress();
        HealthDiagnostics {
            runtime_identity: crate::turbocharger::RuntimeIdentityDiagnostics::load(
                Some("test-release"),
                std::path::Path::new("/definitely/missing/termination.env"),
            ),
            process_memory: ProcessMemoryDiagnostics {
                pid: 42,
                rss_bytes: Some(1024),
                virtual_memory_bytes: Some(4096),
                source: "test",
                collection_error: None,
                peaks_24h: MemoryPeakDiagnostics {
                    window_seconds: 86_400,
                    samples_collected: 240,
                    latest_sample_unix_seconds: Some(1_700_000_010),
                    latest_sample_age_seconds: Some(30),
                    rss_peak_bytes: Some(8192),
                    rss_peak_unix_seconds: Some(1_700_000_000),
                    virtual_memory_peak_bytes: Some(16_384),
                    virtual_memory_peak_unix_seconds: Some(1_700_000_000),
                },
            },
            runtime_memory: crate::turbocharger::RuntimeMemoryDiagnostics {
                latest_sample: Some(crate::turbocharger::RuntimeMemorySample {
                    captured_at_unix_millis: 1_700_000_000_000,
                    phase: crate::turbocharger::WorkloadPhase::DatabaseContention,
                    pressure_state: crate::turbocharger::MemoryPressureState::Throttled,
                    process: crate::turbocharger::ProcessMemoryBreakdown {
                        rss_bytes: Some(1024),
                        anonymous_rss_bytes: Some(768),
                        file_rss_bytes: Some(256),
                        shared_rss_bytes: None,
                        swap_bytes: Some(0),
                        virtual_memory_bytes: Some(4096),
                        collection_error: None,
                    },
                    cgroup: crate::turbocharger::CgroupMemoryDiagnostics {
                        current_bytes: Some(2048),
                        high_bytes: Some(8192),
                        max_bytes: Some(16_384),
                        events: crate::turbocharger::runtime_memory::CgroupMemoryEvents {
                            oom: Some(1),
                            oom_kill: Some(0),
                            ..Default::default()
                        },
                        ..Default::default()
                    },
                    components: crate::turbocharger::MemoryComponentDiagnostics {
                        user_cache_entries: 1,
                        user_cache_entry_limit: 10,
                        user_cache_evictions: 4,
                        user_cache_bytes: 100,
                        user_cache_limit_bytes: 1000,
                        post_cache_entries: 2,
                        post_cache_entry_limit: 20,
                        post_cache_evictions: 5,
                        post_cache_bytes: 200,
                        post_cache_limit_bytes: 2000,
                        negative_cache_entries: 3,
                        negative_cache_entry_limit: 30,
                        negative_cache_evictions: 6,
                        negative_cache_bytes: 30,
                        negative_cache_limit_bytes: 300,
                        coordination_bytes: 40,
                        input_channel_bytes: 50,
                        input_channel_limit_bytes: 500,
                        in_flight_payload_bytes: 60,
                        in_flight_payload_limit_bytes: 600,
                        monitor_broadcast_bytes: 70,
                        monitor_broadcast_limit_bytes: 700,
                        sqlx_connections: 2,
                        sqlx_idle_connections: 1,
                        sqlx_max_connections: 4,
                        sqlite_cache_bytes_per_connection: 64 * 1024,
                        sqlite_mmap_bytes: 256 * 1024 * 1024,
                        sqlite_temp_store: "memory".to_string(),
                    },
                    ..Default::default()
                }),
                samples_retained: 12,
                sample_capacity: 720,
                sample_interval_seconds: 5,
                latest_sample_age_seconds: Some(3),
                pressure_state: crate::turbocharger::MemoryPressureState::Throttled,
                envelope: Some(crate::turbocharger::MemoryEnvelope {
                    recovery_bytes: 4096,
                    soft_pressure_bytes: 8192,
                    emergency_bytes: 12_288,
                    external_hard_limit_bytes: 16_384,
                    host_memory_bytes: 32_768,
                    required_host_headroom_bytes: 16_384,
                    pressure_confirmation_seconds: 10,
                    recovery_confirmation_seconds: 20,
                }),
            },
            cache_state: CacheStateDiagnostics {
                user_entries: 1,
                post_entries: 2,
                user_capacity: 10,
                post_capacity: 20,
                user_hits: 3,
                user_misses: 4,
                post_hits: 5,
                post_misses: 6,
                total_requests: 18,
                cache_evictions: 0,
                negative_post_entries: 1,
                negative_post_capacity: 8,
                negative_post_hits: 2,
                negative_post_evictions: 0,
                post_recoveries: 1,
                post_found: 7,
                post_missing: 2,
                post_unavailable: 3,
                partial_records_total: 3,
                isolation_broad_outage: 1,
                isolation_singleton_poison: 1,
                isolation_budget_exhausted: 1,
            },
            sqlite_state: SQLiteStateDiagnostics {
                available: true,
                db_size_bytes: Some(8192),
                wal_size_bytes: Some(0),
                page_count: Some(2),
                page_size_bytes: Some(4096),
                freelist_count: Some(0),
                cache_size_pages: Some(-64000),
                mmap_size_bytes: Some(268435456),
                journal_mode: Some("wal".to_string()),
                journal_size_limit_bytes: Some(5368709120),
                temp_store: Some(2),
                partial_records: Some(3),
                vacuum_pending: Some(true),
                vacuum_pending_reason: Some(crate::storage::VacuumPendingReason::Bloat),
                vacuum_pending_since: Some(chrono::Utc::now()),
                vacuum_last_run_at: Some(chrono::Utc::now()),
                vacuum_last_run_duration_ms: Some(1250),
                vacuum_last_run_bytes_reclaimed: Some(4096),
                freelist_ratio: Some(0.25),
                over_budget: Some(false),
                over_budget_after_vacuum: Some(true),
                collection_error: None,
            },
            not_redis_state: NotRedisStateDiagnostics {
                connected: true,
                engine: "not_redis".to_string(),
                stream_name: "hydrated_jetstream".to_string(),
                stream_length: Some(7),
                configured_max_length: Some(100),
                collection_error: None,
            },
            pipeline_progress: progress.snapshot(ProgressThresholds {
                startup_grace: Duration::from_secs(1),
                ingress_idle: Duration::from_secs(10),
                batch_execution: Duration::from_secs(10),
                recovery_successes: 1,
            }),
            completion_frontier: crate::turbocharger::coordinator::CompletionFrontier::new(None)
                .snapshot(),
            failure_containment: Default::default(),
            bluesky_fetch: Default::default(),
            bluesky_coordination: crate::client::BlueskyCoordinationDiagnostics {
                profiles: crate::client::CoordinationSnapshot {
                    key_capacity: 150,
                    waiter_capacity: 600,
                    key_high_watermark: 12,
                    waiter_high_watermark: 20,
                    retained_identifier_bytes_high_watermark: 512,
                    coalesced_waiters_total: 8,
                    completions_total: 40,
                    cancellations_total: 2,
                    failed_finalizations_total: 0,
                    ..Default::default()
                },
                posts: crate::client::CoordinationSnapshot {
                    key_capacity: 150,
                    waiter_capacity: 600,
                    key_high_watermark: 14,
                    waiter_high_watermark: 24,
                    retained_identifier_bytes_high_watermark: 768,
                    coalesced_waiters_total: 10,
                    completions_total: 50,
                    cancellations_total: 3,
                    failed_finalizations_total: 0,
                    ..Default::default()
                },
            },
        }
    }

    fn sample_health(healthy: bool) -> HealthStatus {
        HealthStatus {
            healthy,
            serving: healthy,
            recovering: false,
            live: healthy,
            stale: !healthy,
            redis_connected: healthy,
            sqlite_available: healthy,
            session_count: if healthy { 1 } else { 0 },
            diagnostics: sample_diagnostics(),
            readiness: ReadinessDiagnostics {
                state: if healthy {
                    PipelineReadinessState::Healthy
                } else {
                    PipelineReadinessState::Stale
                },
                stage: None,
                reason: (!healthy).then(|| "dependency_unhealthy".to_string()),
                transport_connected: healthy,
                recovery_phase: if healthy {
                    crate::models::recovery::RecoveryPhase::Live
                } else {
                    crate::models::recovery::RecoveryPhase::Connecting
                },
                unrecoverable_gap: None,
            },
        }
    }

    #[test]
    fn readiness_http_status_is_ok_when_healthy() {
        assert_eq!(readiness_http_status(&sample_health(true)), StatusCode::OK);
    }

    #[test]
    fn readiness_http_status_is_503_when_unhealthy() {
        assert_eq!(
            readiness_http_status(&sample_health(false)),
            StatusCode::SERVICE_UNAVAILABLE
        );
    }

    #[test]
    fn health_http_response_is_healthy_and_ok_for_healthy_status() {
        let (status_code, response) = health_http_response(sample_health(true));
        assert_eq!(status_code, StatusCode::OK);
        assert_eq!(response.status, "healthy");
        assert!(response.data.healthy);
        assert_eq!(response.data.diagnostics.cache_state.user_capacity, 10);
        assert_eq!(
            response.data.diagnostics.not_redis_state.stream_name,
            "hydrated_jetstream"
        );
    }

    #[test]
    fn health_http_response_keeps_diagnostics_available_when_unhealthy() {
        let (status_code, response) = health_http_response(sample_health(false));
        assert_eq!(status_code, StatusCode::OK);
        assert_eq!(response.status, "unhealthy");
        assert!(!response.data.healthy);
    }

    #[test]
    fn health_response_serializes_diagnostics_snapshot() {
        let (_status_code, response) = health_http_response(sample_health(true));
        let json: Value = serde_json::to_value(response).expect("health response should serialize");

        assert_eq!(json["status"], "healthy");
        assert!(json["data"]["diagnostics"]["process_memory"]["pid"].is_number());
        assert!(
            json["data"]["diagnostics"]["process_memory"]["peaks_24h"]["rss_peak_bytes"]
                .is_number()
        );
        assert!(json["data"]["diagnostics"]["cache_state"]["user_capacity"].is_number());
        assert!(json["data"]["diagnostics"]["sqlite_state"]["journal_mode"].is_string());
        assert!(json["data"]["diagnostics"]["sqlite_state"]["vacuum_pending"].is_boolean());
        assert_eq!(
            json["data"]["diagnostics"]["sqlite_state"]["vacuum_pending_reason"],
            "bloat"
        );
        assert!(json["data"]["diagnostics"]["sqlite_state"]["vacuum_pending_since"].is_string());
        assert!(json["data"]["diagnostics"]["sqlite_state"]["vacuum_last_run_at"].is_string());
        assert_eq!(
            json["data"]["diagnostics"]["sqlite_state"]["vacuum_last_run_duration_ms"],
            1250
        );
        assert_eq!(
            json["data"]["diagnostics"]["sqlite_state"]["vacuum_last_run_bytes_reclaimed"],
            4096
        );
        assert_eq!(
            json["data"]["diagnostics"]["sqlite_state"]["freelist_ratio"],
            0.25
        );
        assert_eq!(
            json["data"]["diagnostics"]["sqlite_state"]["over_budget"],
            false
        );
        assert_eq!(
            json["data"]["diagnostics"]["sqlite_state"]["over_budget_after_vacuum"],
            true
        );
        assert!(json["data"]["diagnostics"]["not_redis_state"]["stream_name"].is_string());
        assert_eq!(json["data"]["readiness"]["state"], "healthy");
        assert!(json["data"]["diagnostics"]["pipeline_progress"]["ingress_messages"].is_number());
        assert_eq!(
            json["data"]["diagnostics"]["bluesky_coordination"]["profiles"]["key_capacity"],
            150
        );
        assert_eq!(
            json["data"]["diagnostics"]["bluesky_coordination"]["posts"]["completed_result_owners"],
            0
        );
    }

    #[test]
    fn metrics_response_includes_runtime_diagnostics_values() {
        let mut diagnostics = sample_diagnostics();
        diagnostics.failure_containment.subtype =
            Some(crate::turbocharger::PipelineFailureSubtype::BatchOrdering);
        diagnostics.failure_containment.stage =
            Some(crate::turbocharger::PipelineFailureStage::Ingress);
        diagnostics.failure_containment.boundary_present = false;
        let output = prometheus_metrics_from_diagnostics(&diagnostics);

        assert!(output.contains("jetstream_turbo_process_memory_rss_bytes 1024"));
        assert!(output.contains("jetstream_turbo_process_memory_virtual_bytes 4096"));
        assert!(output.contains("jetstream_turbo_runtime_memory_samples_retained 12"));
        assert!(
            output.contains("jetstream_turbo_memory_pressure_state_info{state=\"throttled\"} 1")
        );
        assert!(output.contains(
            "jetstream_turbo_memory_workload_phase_info{phase=\"database_contention\"} 1"
        ));
        assert!(
            output.contains("jetstream_turbo_memory_component_bytes{component=\"post_cache\"} 200")
        );
        assert!(output.contains("jetstream_turbo_cgroup_oom_events_total 1"));
        assert!(output.contains("jetstream_turbo_process_memory_peak_window_seconds 86400"));
        assert!(output.contains("jetstream_turbo_process_memory_samples_24h 240"));
        assert!(output.contains("jetstream_turbo_process_memory_rss_peak_24h_bytes 8192"));
        assert!(output.contains("jetstream_turbo_process_memory_virtual_peak_24h_bytes 16384"));
        assert!(output.contains("jetstream_turbo_cache_user_entries 1"));
        assert!(output.contains("jetstream_turbo_cache_post_entries 2"));
        assert!(output.contains("jetstream_turbo_sqlite_available 1"));
        assert!(output.contains("jetstream_turbo_sqlite_db_size_bytes 8192"));
        assert!(output.contains("jetstream_turbo_db_size_bytes 8192"));
        assert!(output.contains("jetstream_turbo_db_freelist_ratio 0.25"));
        assert!(output.contains("jetstream_turbo_vacuum_pending 1"));
        assert!(output.contains("jetstream_turbo_vacuum_last_duration_ms 1250"));
        assert!(output.contains("jetstream_turbo_db_over_budget 0"));
        assert!(output.contains("jetstream_turbo_not_redis_connected 1"));
        assert!(output.contains("jetstream_turbo_not_redis_stream_length 7"));
        assert!(output.contains("jetstream_turbo_pipeline_ingress_messages_total 1"));
        assert!(output.contains("jetstream_turbo_pipeline_maximum_batches 6"));
        assert!(output
            .contains("jetstream_turbo_bluesky_coordination_key_capacity{kind=\"profiles\"} 150"));
        assert!(output.contains(
            "jetstream_turbo_bluesky_coordination_waiter_high_watermark{kind=\"posts\"} 24"
        ));
        assert!(output.contains(
            "jetstream_turbo_bluesky_coordination_completions_total{kind=\"profiles\"} 40"
        ));
        assert!(output.contains("jetstream_turbo_running_permit_holders 0"));
        assert!(output.contains("jetstream_turbo_queued_batches 0"));
        assert!(output.contains("jetstream_turbo_committed_source_velocity NaN"));
        assert!(output.contains(
            "jetstream_turbo_failure_containment_info{subtype=\"batch_ordering\",stage=\"ingress\",boundary=\"absent\"} 1"
        ));
        assert!(output.contains(
            "jetstream_turbo_optional_hydration_post_outcomes_total{outcome=\"temporarily_unavailable\"} 3"
        ));
        assert!(output.contains("jetstream_turbo_optional_hydration_partial_records 3"));
        assert!(output.contains("jetstream_turbo_optional_hydration_negative_cache_entries 1"));
        assert!(!output.contains("at://"));
        assert!(!output.to_ascii_lowercase().contains("authorization"));
        assert!(!output.contains("request_fingerprint="));
        assert!(!output.contains("did:plc:"));
        assert!(!output.contains("fingerprint=\""));
    }

    #[test]
    fn metrics_expose_runtime_identity_frontier_and_bounded_stage_labels() {
        let mut diagnostics = sample_diagnostics();
        let frontier = crate::turbocharger::coordinator::CompletionFrontierSnapshot {
            pending_range_count: 2,
            next_required_ordinal: 5,
            furthest_completed_ordinal: Some(9),
            durable_checkpoint_ordinal: Some(4),
            unresolved_gap_age_seconds: Some(17),
        };
        diagnostics.completion_frontier = frontier;
        diagnostics.pipeline_progress.stage_timings.push(
            crate::turbocharger::StageTimingSnapshot {
                stage: crate::turbocharger::PipelineStage::Hydration,
                outcome: crate::turbocharger::PipelineStageOutcome::Timeout,
                count: 1,
                duration_milliseconds_total: 1500,
                duration_milliseconds_max: 1500,
            },
        );
        let output = prometheus_metrics_from_diagnostics(&diagnostics);

        // Runtime identity is rendered beside every scrape so counter resets
        // are attributable to a process epoch and release.
        assert!(output.contains("jetstream_turbo_process_start_time_seconds"));
        assert!(output
            .contains("jetstream_turbo_release_info{release=\"test-release\",available=\"1\"} 1"));
        assert!(output.contains(
            "jetstream_turbo_previous_termination_info{state=\"missing\",class=\"unavailable\"} 1"
        ));

        // Frontier gauges render available values without cursor content.
        assert!(output.contains("jetstream_turbo_frontier_pending_ranges 2"));
        assert!(output.contains("jetstream_turbo_frontier_next_required_ordinal 5"));
        assert!(output.contains("jetstream_turbo_frontier_furthest_completed_ordinal 9"));
        assert!(output.contains("jetstream_turbo_frontier_durable_checkpoint_ordinal 4"));
        assert!(output.contains("jetstream_turbo_frontier_unresolved_gap_age_seconds 17"));

        // Stage timings render as histogram sums/counts over bounded enums.
        assert!(output.contains(
            "jetstream_turbo_pipeline_stage_duration_seconds_sum{stage=\"hydration\",outcome=\"timeout\"}"
        ));
        assert!(output.contains(
            "jetstream_turbo_pipeline_stage_duration_seconds_count{stage=\"hydration\",outcome=\"timeout\"} 1"
        ));
        const APPROVED_STAGES: [&str; 8] = [
            "ingress",
            "duplicate_detection",
            "hydration",
            "storage",
            "publication",
            "broadcast",
            "checkpoint_persistence",
            "end_to_end",
        ];
        const APPROVED_OUTCOMES: [&str; 4] = ["success", "error", "timeout", "cancellation"];
        for line in output.lines().filter(|line| {
            line.starts_with("jetstream_turbo_pipeline_stage_duration_seconds")
                && !line.starts_with('#')
        }) {
            let stage = APPROVED_STAGES
                .iter()
                .any(|stage| line.contains(&format!("stage=\"{stage}\"")));
            let outcome = APPROVED_OUTCOMES
                .iter()
                .any(|outcome| line.contains(&format!("outcome=\"{outcome}\"")));
            assert!(stage && outcome, "unbounded stage label: {line}");
        }

        // No request query strings or raw source event identifiers leak into
        // the metrics surface.
        assert!(!output.contains("actors="));
        assert!(!output.contains("uris="));
        assert!(!output.contains('?'));
    }

    #[test]
    fn metrics_render_unavailable_runtime_identity_states() {
        let mut diagnostics = sample_diagnostics();
        diagnostics.runtime_identity = crate::turbocharger::RuntimeIdentityDiagnostics::load(
            None,
            std::path::Path::new("/definitely/missing/termination.env"),
        );
        diagnostics.completion_frontier =
            crate::turbocharger::coordinator::CompletionFrontierSnapshot {
                pending_range_count: 0,
                next_required_ordinal: 1,
                furthest_completed_ordinal: None,
                durable_checkpoint_ordinal: None,
                unresolved_gap_age_seconds: None,
            };
        let output = prometheus_metrics_from_diagnostics(&diagnostics);

        assert!(output.contains("jetstream_turbo_release_info{available=\"0\"} 1"));
        assert!(output.contains(
            "jetstream_turbo_previous_termination_info{state=\"missing\",class=\"unavailable\"} 1"
        ));
        assert!(output.contains("jetstream_turbo_frontier_furthest_completed_ordinal NaN"));
        assert!(output.contains("jetstream_turbo_frontier_durable_checkpoint_ordinal NaN"));
        assert!(output.contains("jetstream_turbo_frontier_unresolved_gap_age_seconds NaN"));
    }

    #[test]
    fn settled_coordination_metrics_are_zero_and_use_only_bounded_kind_labels() {
        let output = prometheus_metrics_from_diagnostics(&sample_diagnostics());
        assert!(output
            .contains("jetstream_turbo_bluesky_coordination_pending_keys{kind=\"profiles\"} 0"));
        assert!(output
            .contains("jetstream_turbo_bluesky_coordination_in_flight_keys{kind=\"posts\"} 0"));
        assert!(
            output.contains("jetstream_turbo_bluesky_coordination_waiters{kind=\"profiles\"} 0")
        );
        assert!(output.contains(
            "jetstream_turbo_bluesky_coordination_retained_identifier_bytes{kind=\"posts\"} 0"
        ));

        for line in output.lines().filter(|line| {
            line.starts_with("jetstream_turbo_bluesky_coordination_") && !line.starts_with('#')
        }) {
            assert!(
                line.contains("{kind=\"profiles\"}") || line.contains("{kind=\"posts\"}"),
                "unexpected coordination label set: {line}"
            );
            assert_eq!(
                line.matches('=').count(),
                1,
                "unbounded label found: {line}"
            );
        }
    }

    #[test]
    fn metrics_response_uses_nan_for_missing_optional_values() {
        let mut diagnostics = sample_diagnostics();
        diagnostics.process_memory.rss_bytes = None;
        diagnostics
            .process_memory
            .peaks_24h
            .latest_sample_age_seconds = None;
        diagnostics.process_memory.peaks_24h.rss_peak_bytes = None;
        diagnostics.process_memory.peaks_24h.rss_peak_unix_seconds = None;
        diagnostics
            .process_memory
            .peaks_24h
            .virtual_memory_peak_bytes = None;
        diagnostics
            .process_memory
            .peaks_24h
            .virtual_memory_peak_unix_seconds = None;
        diagnostics.sqlite_state.db_size_bytes = None;
        diagnostics.sqlite_state.freelist_ratio = None;
        diagnostics.sqlite_state.vacuum_pending = None;
        diagnostics.sqlite_state.vacuum_last_run_duration_ms = None;
        diagnostics.sqlite_state.over_budget = None;
        diagnostics.not_redis_state.stream_length = None;
        diagnostics.not_redis_state.configured_max_length = None;

        let output = prometheus_metrics_from_diagnostics(&diagnostics);
        assert!(output.contains("jetstream_turbo_process_memory_rss_bytes NaN"));
        assert!(output.contains("jetstream_turbo_process_memory_latest_sample_age_seconds NaN"));
        assert!(output.contains("jetstream_turbo_process_memory_rss_peak_24h_bytes NaN"));
        assert!(output.contains("jetstream_turbo_process_memory_virtual_peak_24h_unix_seconds NaN"));
        assert!(output.contains("jetstream_turbo_sqlite_db_size_bytes NaN"));
        assert!(output.contains("jetstream_turbo_db_size_bytes NaN"));
        assert!(output.contains("jetstream_turbo_db_freelist_ratio NaN"));
        assert!(output.contains("jetstream_turbo_vacuum_pending NaN"));
        assert!(output.contains("jetstream_turbo_vacuum_last_duration_ms NaN"));
        assert!(output.contains("jetstream_turbo_db_over_budget NaN"));
        assert!(output.contains("jetstream_turbo_not_redis_stream_length NaN"));
        assert!(output.contains("jetstream_turbo_not_redis_configured_max_length NaN"));
    }

    fn sample_record() -> EnrichedRecord {
        EnrichedRecord::new(JetstreamMessage {
            did: "did:plc:test".to_string().into(),
            time_us: Some(1640995200000000),
            seq: Some(1),
            kind: MessageKind::Commit,
            commit: Some(CommitData {
                rev: Some("rev-1".to_string()),
                operation_type: OperationType::Create,
                collection: Some("app.bsky.feed.post".to_string()),
                rkey: Some("rkey-1".to_string()),
                record: Some(crate::models::jetstream::owned_record(
                    serde_json::json!({ "text": "hello" }),
                )),
                cid: Some("bafyrei".to_string()),
            }),
            raw_json: None,
        })
    }

    /// Consumes the interval's immediate first tick so the next tick fires after
    /// one full period, mirroring how `handle_websocket` primes its intervals.
    async fn primed_interval(period: Duration) -> tokio::time::Interval {
        let mut interval = interval(period);
        interval.set_missed_tick_behavior(MissedTickBehavior::Delay);
        interval.tick().await;
        interval
    }

    /// A test sink that records every message it is asked to send and never
    /// blocks, standing in for the WebSocket sender in `monitor_send_loop`.
    #[derive(Clone)]
    struct RecordingSink(Arc<Mutex<Vec<Message>>>);

    #[derive(Debug)]
    struct TestSinkError;

    impl std::fmt::Display for TestSinkError {
        fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
            write!(f, "test sink error")
        }
    }

    impl futures::Sink<Message> for RecordingSink {
        type Error = TestSinkError;

        fn poll_ready(
            self: std::pin::Pin<&mut Self>,
            _cx: &mut std::task::Context<'_>,
        ) -> std::task::Poll<Result<(), Self::Error>> {
            std::task::Poll::Ready(Ok(()))
        }

        fn start_send(self: std::pin::Pin<&mut Self>, message: Message) -> Result<(), Self::Error> {
            self.get_mut().0.lock().unwrap().push(message);
            Ok(())
        }

        fn poll_flush(
            self: std::pin::Pin<&mut Self>,
            _cx: &mut std::task::Context<'_>,
        ) -> std::task::Poll<Result<(), Self::Error>> {
            std::task::Poll::Ready(Ok(()))
        }

        fn poll_close(
            self: std::pin::Pin<&mut Self>,
            _cx: &mut std::task::Context<'_>,
        ) -> std::task::Poll<Result<(), Self::Error>> {
            std::task::Poll::Ready(Ok(()))
        }
    }

    #[tokio::test]
    async fn receive_loop_never_blocks_when_send_side_is_stalled() {
        let connection_id = uuid::Uuid::new_v4();
        let (broadcast_tx, broadcast_rx) = broadcast::channel(16);
        // The receiver side is never read, so the outgoing channel fills up and
        // stays full - the receive loop must drop instead of blocking.
        let (outgoing_tx, _stalled_outgoing_rx) = mpsc::channel::<EnrichedRecord>(2);
        let socket_rx = stream::pending::<Result<Message, std::io::Error>>();
        let peer_timeout_interval = primed_interval(Duration::from_secs(3600)).await;
        let lag_log_interval = primed_interval(Duration::from_secs(3600)).await;

        let task = tokio::spawn(monitor_receive_loop(
            connection_id,
            socket_rx,
            broadcast_rx,
            outgoing_tx,
            peer_timeout_interval,
            lag_log_interval,
        ));

        for _ in 0..10 {
            broadcast_tx.send(sample_record()).unwrap();
        }
        drop(broadcast_tx);

        let result = tokio::time::timeout(Duration::from_secs(5), task)
            .await
            .expect("receive loop must not block on a stalled consumer")
            .unwrap();
        // All 10 messages were drained from the broadcast: 2 buffered, 8 dropped.
        assert_eq!(result, (0, 8));
    }

    #[tokio::test]
    async fn try_send_failure_increments_drop_total_without_closing_connection() {
        let connection_id = uuid::Uuid::new_v4();
        let (broadcast_tx, broadcast_rx) = broadcast::channel(16);
        let (outgoing_tx, mut outgoing_rx) = mpsc::channel::<EnrichedRecord>(1);
        let socket_rx = stream::pending::<Result<Message, std::io::Error>>();
        let peer_timeout_interval = primed_interval(Duration::from_secs(3600)).await;
        let lag_log_interval = primed_interval(Duration::from_secs(3600)).await;

        let task = tokio::spawn(monitor_receive_loop(
            connection_id,
            socket_rx,
            broadcast_rx,
            outgoing_tx,
            peer_timeout_interval,
            lag_log_interval,
        ));

        // First record fills the outgoing channel.
        broadcast_tx.send(sample_record()).unwrap();
        let first = outgoing_rx.recv().await.expect("first record buffered");
        assert_eq!(first.get_did(), "did:plc:test");

        // Two more records arrive while the channel is full: both are dropped,
        // but the connection must stay alive (no break, no close).
        broadcast_tx.send(sample_record()).unwrap();
        broadcast_tx.send(sample_record()).unwrap();
        broadcast_tx.send(sample_record()).unwrap();
        tokio::task::yield_now().await;
        assert!(
            !task.is_finished(),
            "receive loop must survive try_send failures"
        );

        // Free a slot; the loop must keep processing rather than having exited.
        let second = outgoing_rx.recv().await.expect("second record buffered");
        assert_eq!(second.get_did(), "did:plc:test");
        broadcast_tx.send(sample_record()).unwrap();
        drop(broadcast_tx);

        let (_, dropped_total) = task.await.unwrap();
        assert_eq!(dropped_total, 2);

        // The message sent after recovery was buffered, proving the connection
        // stayed alive through the drops.
        let recovered = outgoing_rx.recv().await.expect("recovered record buffered");
        assert_eq!(recovered.get_did(), "did:plc:test");
    }

    #[tokio::test]
    async fn send_loop_exits_when_outgoing_channel_sender_is_dropped() {
        let connection_id = uuid::Uuid::new_v4();
        let (outgoing_tx, outgoing_rx) = mpsc::channel::<EnrichedRecord>(4);
        // Dropping the sender is the only shutdown signal the send loop needs.
        drop(outgoing_tx);

        let recorded = Arc::new(Mutex::new(Vec::new()));
        let sink = RecordingSink(recorded.clone());
        let heartbeat_interval = primed_interval(Duration::from_secs(3600)).await;

        let task = tokio::spawn(monitor_send_loop(
            connection_id,
            sink,
            outgoing_rx,
            heartbeat_interval,
        ));

        let sent = tokio::time::timeout(Duration::from_secs(5), task)
            .await
            .expect("send loop must exit when the outgoing channel closes")
            .unwrap();
        assert_eq!(sent, 0);
        assert!(recorded.lock().unwrap().is_empty());
    }

    #[tokio::test]
    async fn heartbeat_fires_with_biased_priority_under_full_outgoing_channel() {
        let connection_id = uuid::Uuid::new_v4();
        let (outgoing_tx, outgoing_rx) = mpsc::channel::<EnrichedRecord>(8);
        for _ in 0..3 {
            outgoing_tx.send(sample_record()).await.unwrap();
        }
        drop(outgoing_tx);

        let recorded = Arc::new(Mutex::new(Vec::new()));
        let sink = RecordingSink(recorded.clone());

        // Prime the interval so its next tick is already due when the loop
        // starts: with biased selection the heartbeat must win over the
        // backlogged data messages.
        let heartbeat_interval = primed_interval(Duration::from_millis(1)).await;
        tokio::time::sleep(Duration::from_millis(5)).await;

        let task = tokio::spawn(monitor_send_loop(
            connection_id,
            sink,
            outgoing_rx,
            heartbeat_interval,
        ));

        let sent = tokio::time::timeout(Duration::from_secs(5), task)
            .await
            .expect("send loop must drain a prefilled outgoing channel")
            .unwrap();
        assert_eq!(sent, 3);

        let messages = recorded.lock().unwrap();
        assert_eq!(
            messages[0],
            Message::Ping(b"jetstream-turbo".to_vec()),
            "biased select must let the heartbeat fire before backlogged data"
        );
        assert!(
            messages.iter().any(|m| matches!(m, Message::Ping(_))),
            "at least one heartbeat must be sent"
        );
        let data_count = messages
            .iter()
            .filter(|m| matches!(m, Message::Text(_)))
            .count();
        assert_eq!(data_count, 3);
    }
}
