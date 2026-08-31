use anyhow::Result;
use jetstream_monitor::{
    config::Settings,
    diagnostics::DiagnosticLogger,
    incidents::{IncidentStore, MonitorIdentity},
    stats::{
        comparison_eligibility, AvailabilitySnapshot, ObservationConfig, StatsAggregator,
        StreamStatsInternal, UptimeDetailedStats, UptimeMetricsSnapshot, UptimeTracker,
    },
    storage::{
        AvailabilityHistory, EventTimeHistory, HourlyStat, HourlyUptime, ReliabilityHistory,
        Storage, UptimeResponse,
    },
    stream::{
        BackoffPolicy, Effect, IncidentCommand, StreamClient, StreamEvent, StreamId,
        TransitionProcessor,
    },
    telemetry::Metrics,
    websocket,
};
use jetstream_monitor::api::{
    ApiError, ApiResponse, HealthSnapshot, HealthStatus, StorageHealth, StreamHealth,
};
use utoipa::OpenApi;
use jetstream_monitor::incidents::LedgerHealth;
use jetstream_monitor::incidents::IncidentId;
#[allow(unused_imports)]
use jetstream_monitor::incidents::IncidentEventType as _IE_unused;
use std::collections::HashMap;
use std::{sync::Arc, time::Duration};

const API_SERVER_URL: &str = "http://localhost:3001";
const HOURLY_INTERVAL_SECONDS: u64 = 3600;
const HOURLY_INTERVAL_SECONDS_I64: i64 = 3600;
const HOURLY_UPTIME_CONTRACT_VERSION: i64 = 4;
const BASELINE_1_URL: &str =
    "wss://jetstream.us-east.bsky.network/subscribe?wantedCollections=app.bsky.feed.post";
const BASELINE_2_URL: &str =
    "wss://jetstream.us-west.bsky.network/subscribe?wantedCollections=app.bsky.feed.post";
const BASELINE_1_NAME: &str = "Baseline 1 (jetstream.us-east)";
const BASELINE_2_NAME: &str = "Baseline 2 (jetstream.us-west)";

/// Bounded per-stream state captured by the transition loop for health reads.
#[derive(Debug, Clone, Default)]
struct StreamObserved {
    transport: String,
    delivery: String,
    delivery_idle: bool,
    age_started: Option<std::time::Instant>,
    last_record_at: Option<std::time::Instant>,
    active_incident_id: Option<String>,
    liveness: Option<Arc<jetstream_monitor::stream::LivenessClock>>,
}

#[derive(Debug, Default)]
struct Operational {
    observation_loop_alive: bool,
    streams: HashMap<StreamId, StreamObserved>,
}

fn ops_loop_alive(operational: &Arc<std::sync::Mutex<Operational>>, alive: bool) {
    operational.lock().unwrap().observation_loop_alive = alive;
}

fn delivery_label(state: jetstream_monitor::stream::DeliveryState) -> &'static str {
    use jetstream_monitor::stream::DeliveryState;
    match state {
        DeliveryState::Unknown => "unknown",
        DeliveryState::Waiting => "waiting",
        DeliveryState::Delivering => "delivering",
        DeliveryState::Idle => "idle",
    }
}

#[derive(Clone)]
struct AppState {
    #[allow(dead_code)]
    broadcast_tx: Arc<tokio::sync::broadcast::Sender<jetstream_monitor::StreamStats>>,
    storage: Arc<Storage>,
    uptime: Arc<std::sync::RwLock<UptimeTracker>>,
    operational: Arc<std::sync::Mutex<Operational>>,
    ledger_health: Arc<LedgerHealth>,
    hourly_health: Arc<LedgerHealth>,
    metrics: Arc<Metrics>,
    process_epoch: Arc<String>,
    release: Arc<String>,
    started_at: chrono::DateTime<chrono::Utc>,
    #[allow(dead_code)]
    incident_store: Arc<IncidentStore>,
}

#[derive(Debug, Clone, Copy)]
struct HourlyIntervalMetrics {
    uptime_a_seconds: u64,
    uptime_b_seconds: u64,
    downtime_a_seconds: u64,
    downtime_b_seconds: u64,
    disconnect_count_a: u64,
    disconnect_count_b: u64,
    avg_connect_time_a_ms: u64,
    avg_connect_time_b_ms: u64,
    messages_a: u64,
    messages_b: u64,
    delivery_latency_a_ms: f64,
    delivery_latency_b_ms: f64,
    mttr_a_ms: u64,
    mttr_b_ms: u64,
    baseline_1_uptime_seconds: u64,
    baseline_2_uptime_seconds: u64,
    baseline_1_downtime_seconds: u64,
    baseline_2_downtime_seconds: u64,
    baseline_1_messages: u64,
    baseline_2_messages: u64,
}

#[derive(Debug, Clone, Copy)]
struct WindowRollup {
    uptime_a_seconds: u64,
    uptime_b_seconds: u64,
    downtime_a_seconds: u64,
    downtime_b_seconds: u64,
}

fn delta_counter(current: u64, previous: u64) -> u64 {
    if current >= previous {
        current - previous
    } else {
        // Counter reset or process restart: treat the current value as interval data.
        current
    }
}

fn availability_interval(
    previous: &AvailabilitySnapshot,
    current: &AvailabilitySnapshot,
) -> AvailabilityHistory {
    let reconnect_reasons = current
        .reason_counts
        .iter()
        .map(|(reason, count)| {
            (
                reason.clone(),
                delta_counter(
                    *count,
                    previous.reason_counts.get(reason).copied().unwrap_or(0),
                ),
            )
        })
        .collect();
    AvailabilityHistory {
        transport_up_seconds: delta_counter(
            current.transport_up_seconds,
            previous.transport_up_seconds,
        ),
        transport_down_seconds: delta_counter(
            current.transport_down_seconds,
            previous.transport_down_seconds,
        ),
        delivery_up_seconds: delta_counter(
            current.delivery_up_seconds,
            previous.delivery_up_seconds,
        ),
        delivery_down_seconds: delta_counter(
            current.delivery_down_seconds,
            previous.delivery_down_seconds,
        ),
        reconnect_reasons,
        client_recovery_ms: delta_counter(current.client_recovery_ms, previous.client_recovery_ms),
        coverage: if current.observation_coverage_known {
            "observed".to_string()
        } else {
            "unknown".to_string()
        },
        outage_episodes: delta_counter(current.outage_episodes, previous.outage_episodes),
        reconnect_attempts: delta_counter(current.reconnect_attempts, previous.reconnect_attempts),
        idle_episodes: delta_counter(current.idle_episodes, previous.idle_episodes),
        transport_recoveries: delta_counter(current.transport_recoveries, previous.transport_recoveries),
        delivery_recoveries: delta_counter(
            current.delivery_recoveries,
            previous.delivery_recoveries,
        ),
        delivery_recovery_ms: delta_counter(
            current.delivery_recovery_ms,
            previous.delivery_recovery_ms,
        ),
    }
}

fn average_from_counter_delta(
    current_sum: u64,
    previous_sum: u64,
    current_count: u64,
    previous_count: u64,
) -> u64 {
    let sum_delta = delta_counter(current_sum, previous_sum);
    let count_delta = delta_counter(current_count, previous_count);
    if count_delta > 0 {
        sum_delta / count_delta
    } else {
        0
    }
}

fn build_interval_metrics(
    previous: UptimeMetricsSnapshot,
    current: UptimeMetricsSnapshot,
) -> HourlyIntervalMetrics {
    HourlyIntervalMetrics {
        uptime_a_seconds: delta_counter(current.uptime_a_seconds, previous.uptime_a_seconds),
        uptime_b_seconds: delta_counter(current.uptime_b_seconds, previous.uptime_b_seconds),
        downtime_a_seconds: delta_counter(current.downtime_a_seconds, previous.downtime_a_seconds),
        downtime_b_seconds: delta_counter(current.downtime_b_seconds, previous.downtime_b_seconds),
        disconnect_count_a: delta_counter(current.disconnect_count_a, previous.disconnect_count_a),
        disconnect_count_b: delta_counter(current.disconnect_count_b, previous.disconnect_count_b),
        avg_connect_time_a_ms: average_from_counter_delta(
            current.connect_time_sum_a_ms,
            previous.connect_time_sum_a_ms,
            current.connect_time_count_a,
            previous.connect_time_count_a,
        ),
        avg_connect_time_b_ms: average_from_counter_delta(
            current.connect_time_sum_b_ms,
            previous.connect_time_sum_b_ms,
            current.connect_time_count_b,
            previous.connect_time_count_b,
        ),
        messages_a: delta_counter(current.total_messages_a, previous.total_messages_a),
        messages_b: delta_counter(current.total_messages_b, previous.total_messages_b),
        delivery_latency_a_ms: current.delivery_latency_a_ms,
        delivery_latency_b_ms: current.delivery_latency_b_ms,
        mttr_a_ms: average_from_counter_delta(
            current.total_recovery_time_a_ms,
            previous.total_recovery_time_a_ms,
            current.recovery_count_a,
            previous.recovery_count_a,
        ),
        mttr_b_ms: average_from_counter_delta(
            current.total_recovery_time_b_ms,
            previous.total_recovery_time_b_ms,
            current.recovery_count_b,
            previous.recovery_count_b,
        ),
        baseline_1_uptime_seconds: delta_counter(
            current.baseline_1_uptime_seconds,
            previous.baseline_1_uptime_seconds,
        ),
        baseline_2_uptime_seconds: delta_counter(
            current.baseline_2_uptime_seconds,
            previous.baseline_2_uptime_seconds,
        ),
        baseline_1_downtime_seconds: delta_counter(
            current.baseline_1_downtime_seconds,
            previous.baseline_1_downtime_seconds,
        ),
        baseline_2_downtime_seconds: delta_counter(
            current.baseline_2_downtime_seconds,
            previous.baseline_2_downtime_seconds,
        ),
        baseline_1_messages: delta_counter(
            current.baseline_1_total_messages,
            previous.baseline_1_total_messages,
        ),
        baseline_2_messages: delta_counter(
            current.baseline_2_total_messages,
            previous.baseline_2_total_messages,
        ),
    }
}

fn to_non_negative_u64(value: i64) -> u64 {
    if value > 0 {
        value as u64
    } else {
        0
    }
}

fn rollup_window(data: &[HourlyUptime]) -> WindowRollup {
    let uptime_a_seconds = data
        .iter()
        .map(|row| to_non_negative_u64(row.stream_a_seconds))
        .sum();
    let uptime_b_seconds = data
        .iter()
        .map(|row| to_non_negative_u64(row.stream_b_seconds))
        .sum();
    let downtime_a_seconds = data
        .iter()
        .map(|row| to_non_negative_u64(row.stream_a_downtime_seconds))
        .sum();
    let downtime_b_seconds = data
        .iter()
        .map(|row| to_non_negative_u64(row.stream_b_downtime_seconds))
        .sum();

    WindowRollup {
        uptime_a_seconds,
        uptime_b_seconds,
        downtime_a_seconds,
        downtime_b_seconds,
    }
}

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt::init();

    let settings = Settings::load()?;
    tracing::info!(
        "Loaded settings: stream_a={}, stream_b={}",
        settings.stream_a_url,
        settings.stream_b_url
    );

    let storage = Storage::new(&settings.database_url).await?;
    tracing::info!("Initialized database");

    let (lifetime_a, lifetime_b) = storage.get_lifetime_totals().await.unwrap_or((0, 0));
    tracing::info!(
        "Loaded lifetime totals: stream_a={}, stream_b={}",
        lifetime_a,
        lifetime_b
    );
    let storage_arc = Arc::new(storage);

    let stats_internal = Arc::new(std::sync::RwLock::new(StreamStatsInternal::default()));
    stats_internal
        .write()
        .unwrap()
        .load_totals(lifetime_a, lifetime_b);

    let uptime_tracker = Arc::new(std::sync::RwLock::new(
        UptimeTracker::with_event_time_thresholds(
            Duration::from_secs(settings.live_lag_threshold_seconds),
            Duration::from_secs(settings.watermark_skew_threshold_seconds),
            Duration::from_secs(settings.stream_idle_timeout_seconds),
        )
        .with_comparison_config(ObservationConfig {
            horizon: Duration::from_secs(settings.comparison_horizon_seconds),
            bucket_width: Duration::from_secs(settings.comparison_bucket_width_seconds),
            settlement_allowance: Duration::from_secs(
                settings.comparison_settlement_allowance_seconds,
            ),
        }),
    ));
    uptime_tracker
        .write()
        .unwrap()
        .load_totals(lifetime_a, lifetime_b);
    let aggregator = StatsAggregator::new(
        settings.stream_a_name.clone(),
        settings.stream_b_name.clone(),
        BASELINE_1_NAME.to_string(),
        BASELINE_2_NAME.to_string(),
    );
    let broadcast_tx = Arc::new(aggregator.sender());

    let stream_idle_timeout = Duration::from_secs(settings.stream_idle_timeout_seconds.max(1));
    let connect_timeout = Duration::from_secs(settings.connection_timeout_seconds.max(1));
    let heartbeat_interval = Duration::from_secs(settings.heartbeat_interval_seconds.max(1));
    let liveness_deadline = Duration::from_secs(settings.transport_liveness_deadline_seconds.max(1));
    let backoff = BackoffPolicy::new(
        Duration::from_secs(settings.reconnect_backoff_min_seconds.max(1)),
        Duration::from_secs(settings.reconnect_backoff_max_seconds.max(1)),
    );

    let diagnostics = Arc::new(DiagnosticLogger::new(
        settings.diagnostics_log_path.clone(),
        settings.diagnostics_log_max_bytes,
    ));
    let _diagnostics = diagnostics;

    let operational = Arc::new(std::sync::Mutex::new(Operational::default()));
    for stream_id in [
        StreamId::A,
        StreamId::B,
        StreamId::Baseline1,
        StreamId::Baseline2,
    ] {
        operational.lock().unwrap().streams.insert(
            stream_id,
            StreamObserved {
                transport: "disconnected".to_string(),
                delivery: "unknown".to_string(),
                delivery_idle: false,
                age_started: None,
                last_record_at: None,
                active_incident_id: None,
                liveness: Some(Arc::new(
                    jetstream_monitor::stream::LivenessClock::new(),
                )),
            },
        );
    }

    let liveness_a = operational.lock().unwrap().streams[&StreamId::A]
        .liveness
        .clone()
        .expect("liveness clock");
    let liveness_b = operational.lock().unwrap().streams[&StreamId::B]
        .liveness
        .clone()
        .expect("liveness clock");
    let liveness_b1 = operational.lock().unwrap().streams[&StreamId::Baseline1]
        .liveness
        .clone()
        .expect("liveness clock");
    let liveness_b2 = operational.lock().unwrap().streams[&StreamId::Baseline2]
        .liveness
        .clone()
        .expect("liveness clock");

    let client_a = StreamClient::new(settings.stream_a_url.clone(), StreamId::A)
        .with_liveness_clock(liveness_a)
        .with_idle_timeout(stream_idle_timeout)
        .with_heartbeat_interval(heartbeat_interval)
        .with_liveness_deadline(liveness_deadline)
        .with_connect_timeout(connect_timeout)
        .with_backoff_policy(backoff);
    let client_b = StreamClient::new(settings.stream_b_url.clone(), StreamId::B)
        .with_liveness_clock(liveness_b)
        .with_idle_timeout(stream_idle_timeout)
        .with_heartbeat_interval(heartbeat_interval)
        .with_liveness_deadline(liveness_deadline)
        .with_connect_timeout(connect_timeout)
        .with_backoff_policy(backoff);
    let client_baseline_1 = StreamClient::new(BASELINE_1_URL.to_string(), StreamId::Baseline1)
        .with_liveness_clock(liveness_b1)
        .with_idle_timeout(stream_idle_timeout)
        .with_heartbeat_interval(heartbeat_interval)
        .with_liveness_deadline(liveness_deadline)
        .with_connect_timeout(connect_timeout)
        .with_backoff_policy(backoff);
    let client_baseline_2 = StreamClient::new(BASELINE_2_URL.to_string(), StreamId::Baseline2)
        .with_liveness_clock(liveness_b2)
        .with_idle_timeout(stream_idle_timeout)
        .with_heartbeat_interval(heartbeat_interval)
        .with_liveness_deadline(liveness_deadline)
        .with_connect_timeout(connect_timeout)
        .with_backoff_policy(backoff);

    // Process identity, metrics, and the durable incident ledger.
    let monitor_identity = MonitorIdentity {
        process_epoch: IncidentId::generate().as_str().to_string(),
        release: settings.monitor_release.clone(),
    };
    let metrics = Arc::new(Metrics::new(monitor_identity.process_epoch.clone()));
    let incident_store = Arc::new(
        IncidentStore::new(storage_arc.pool().clone())
            .await
            .unwrap_or_else(|e| {
                tracing::error!("Failed to initialize incident ledger storage: {}", e);
                std::process::exit(1);
            }),
    );

    // Reconcile incidents left open by a previous process before observing.
    match incident_store
        .reconcile_open_incidents(&monitor_identity, chrono::Utc::now())
        .await
    {
        Ok(n) if n > 0 => tracing::warn!("Marked {} inherited open incidents incomplete", n),
        Ok(_) => {}
        Err(e) => {
            tracing::error!("Failed to reconcile open incidents at startup: {}", e);
            metrics.record_incident_storage_failure();
        }
    }

    let ledger_health = Arc::new(LedgerHealth::default());
    let hourly_health = Arc::new(LedgerHealth::default());
    let process_started_at = chrono::Utc::now();
    let (incident_tx, mut incident_rx) = tokio::sync::mpsc::unbounded_channel::<IncidentCommand>();
    {
        let store = Arc::clone(&incident_store);
        let identity = monitor_identity.clone();
        let metrics = Arc::clone(&metrics);
        let health = Arc::clone(&ledger_health);
        tokio::spawn(async move {
            while let Some(command) = incident_rx.recv().await {
                if let Err(error) = apply_incident_command(&store, &command, &identity).await {
                    health.record_failure(chrono::Utc::now());
                    metrics.record_incident_storage_failure();
                    tracing::error!(
                        target: "monitor::ledger",
                        error = %error,
                        operation = command.name(),
                        "incident persistence failed"
                    );
                } else {
                    health.record_success(chrono::Utc::now());
                }
            }
        });
    }

    // Periodic retention cleanup for terminal-state incidents.
    {
        let store = Arc::clone(&incident_store);
        let retention_days = settings.incident_retention_days.max(1);
        tokio::spawn(async move {
            let mut interval = tokio::time::interval(std::time::Duration::from_secs(3600));
            loop {
                interval.tick().await;
                match store.retention_cleanup(retention_days).await {
                    Ok(n) if n > 0 => {
                        tracing::info!(target: "monitor::ledger", removed = n, "retention cleanup removed expired incidents")
                    }
                    Ok(_) => {}
                    Err(error) => {
                        tracing::error!(target: "monitor::ledger", error = %error, "retention cleanup failed")
                    }
                }
            }
        });
    }

    let stats_for_stream = Arc::clone(&stats_internal);
    let uptime_for_status: Arc<std::sync::RwLock<UptimeTracker>> = Arc::clone(&uptime_tracker);
    let operational_for_loop = Arc::clone(&operational);
    let metrics_for_loop = Arc::clone(&metrics);
    tokio::spawn(async move {
        use futures::StreamExt;
        ops_loop_alive(&operational_for_loop, true);
        let mut stream_a = Box::pin(client_a.stream());
        let mut stream_b = Box::pin(client_b.stream());
        let mut stream_b1 = Box::pin(client_baseline_1.stream());
        let mut stream_b2 = Box::pin(client_baseline_2.stream());

        let mut processors = [
            TransitionProcessor::new(StreamId::A),
            TransitionProcessor::new(StreamId::B),
            TransitionProcessor::new(StreamId::Baseline1),
            TransitionProcessor::new(StreamId::Baseline2),
        ];

        async fn apply_stream_event(
            index: usize,
            event: StreamEvent,
            processors: &mut [TransitionProcessor; 4],
            uptime: &std::sync::RwLock<UptimeTracker>,
            stats_internal: &std::sync::RwLock<StreamStatsInternal>,
            incident_tx: &tokio::sync::mpsc::UnboundedSender<IncidentCommand>,
            metrics: &Metrics,
            operational: &Arc<std::sync::Mutex<Operational>>,
        ) {
            let wall_now = chrono::Utc::now();
            let stream_of_index = processors[index].stream_id();
            let status_stream_id = processors[index].stream_id();
            let is_record = matches!(event, StreamEvent::Record(_));
            let effects = processors[index].process(event, wall_now);
            for effect in effects {
                match effect {
                    Effect::ConnectionStatus(status) => {
                        uptime.write().unwrap().handle_connection_status(status);
                    }
                    Effect::Record(record) => {
                        metrics.record_useful_record();
                        let delivery_latency_us = record.delivery_latency_us;
                        uptime.write().unwrap().record_stream_message(&record);
                        match record.stream_id {
                            StreamId::A | StreamId::B => {
                                if let Some(lat) = delivery_latency_us {
                                    uptime
                                        .write()
                                        .unwrap()
                                        .record_delivery_latency(record.stream_id, lat);
                                }
                                stats_internal.write().unwrap().update(record);
                            }
                            StreamId::Baseline1 | StreamId::Baseline2 => {}
                        }
                    }
                    Effect::Incident(command) => {
                        {
                            let mut ops = operational.lock().unwrap();
                            if let Some(entry) = ops.streams.get_mut(&stream_of_index) {
                                match &command {
                                    IncidentCommand::Open { incident_id, .. } => {
                                        entry.active_incident_id =
                                            Some(incident_id.as_str().to_string());
                                    }
                                    IncidentCommand::Resolve { .. } => {
                                        entry.active_incident_id = None;
                                    }
                                    _ => {}
                                }
                            }
                        }
                        let _ = incident_tx.send(command);
                    }
                    Effect::OutageStarted => metrics.record_transport_outage(),
                    Effect::AttemptFailed { .. } => {
                        metrics.record_reconnect_attempt();
                        uptime.write().unwrap().record_reconnect_attempt(status_stream_id);
                    }
                    Effect::IdleEpisode { silence_ms } => {
                        metrics.record_idle_episode();
                        uptime
                            .write()
                            .unwrap()
                            .record_delivery_idle(stream_of_index, silence_ms);
                    }
                }
            }
            metrics.set_transport_state(
                processors[index].stream_id(),
                processors[index].transport_state(),
            );
            metrics.set_delivery_state(
                processors[index].stream_id(),
                processors[index].delivery_state(),
            );
            metrics.set_connection_epoch(
                processors[index].stream_id(),
                processors[index].connection_epoch(),
            );

            let now = std::time::Instant::now();
            let mut ops = operational.lock().unwrap();
            if let Some(entry) = ops.streams.get_mut(&stream_of_index) {
                if is_record {
                    entry.last_record_at = Some(now);
                }
                let transport = match processors[index].transport_state() {
                    jetstream_monitor::stream::TransportState::Connected => "connected",
                    jetstream_monitor::stream::TransportState::Connecting => "connecting",
                    jetstream_monitor::stream::TransportState::Disconnected => "disconnected",
                };
                let delivery = delivery_label(processors[index].delivery_state());
                if entry.transport != transport || entry.delivery != delivery {
                    entry.age_started = Some(now);
                }
                entry.transport = transport.to_string();
                entry.delivery = delivery.to_string();
            }
            drop(ops);
        }

        loop {
            tokio::select! {
                Some(event) = stream_a.next() => {
                    apply_stream_event(0,
                        event,
                        &mut processors,
                        &uptime_for_status,
                        &stats_for_stream,
                        &incident_tx,
                        &metrics_for_loop,
                        &operational_for_loop,
                    ).await;
                }
                Some(event) = stream_b.next() => {
                    apply_stream_event(1,
                        event,
                        &mut processors,
                        &uptime_for_status,
                        &stats_for_stream,
                        &incident_tx,
                        &metrics_for_loop,
                        &operational_for_loop,
                    ).await;
                }
                Some(event) = stream_b1.next() => {
                    apply_stream_event(2,
                        event,
                        &mut processors,
                        &uptime_for_status,
                        &stats_for_stream,
                        &incident_tx,
                        &metrics_for_loop,
                        &operational_for_loop,
                    ).await;
                }
                Some(event) = stream_b2.next() => {
                    apply_stream_event(3,
                        event,
                        &mut processors,
                        &uptime_for_status,
                        &stats_for_stream,
                        &incident_tx,
                        &metrics_for_loop,
                        &operational_for_loop,
                    ).await;
                }
                else => {
                    ops_loop_alive(&operational_for_loop, false);
                    break;
                }
            }
        }
    });

    aggregator.process(&stats_internal, &uptime_tracker);

    let stats_for_storage = Arc::clone(&stats_internal);
    let uptime_for_storage: Arc<std::sync::RwLock<UptimeTracker>> = Arc::clone(&uptime_tracker);
    let hourly_health_for_task = Arc::clone(&hourly_health);
    let storage_arc_hourly = Arc::clone(&storage_arc);
    {
        // Periodic metrics refresh for retention count and dashboard subscribers.
        let metrics_for_ops = Arc::clone(&metrics);
        let incident_store_for_ops = Arc::clone(&incident_store);
        let broadcast_for_ops = Arc::clone(&broadcast_tx);
        let hourly_for_gauge = Arc::clone(&hourly_health);
        let ledger_for_gauge = Arc::clone(&ledger_health);
        let process_started_at = process_started_at;
        tokio::spawn(async move {
            let mut interval = tokio::time::interval(std::time::Duration::from_secs(5));
            loop {
                interval.tick().await;
                metrics_for_ops.set_process_start_seconds_ago(
                    (chrono::Utc::now() - process_started_at).num_seconds().max(0) as u64,
                );
                metrics_for_ops
                    .set_dashboard_subscribers(broadcast_for_ops.receiver_count() as i64);
                metrics_for_ops.set_incident_retained_count(
                    incident_store_for_ops.incident_count().await.unwrap_or(0),
                );
                let now = chrono::Utc::now();
                if let Some(age) = hourly_for_gauge.last_success_age_seconds(now) {
                    metrics_for_ops.set_hourly_snapshot_age(age);
                    metrics_for_ops.set_hourly_last_success_age(age);
                }
                if let Some(age) = ledger_for_gauge.last_success_age_seconds(now) {
                    metrics_for_ops.set_incident_last_success_age(age);
                }
            }
        });
    }
    tokio::spawn(async move {
        let mut interval =
            tokio::time::interval(std::time::Duration::from_secs(HOURLY_INTERVAL_SECONDS));
        let mut last_hour = chrono::Utc::now().format("%Y-%m-%d %H").to_string();
        let mut previous_snapshot = {
            let up = uptime_for_storage.read().unwrap();
            up.get_metrics_snapshot()
        };
        let mut previous_availability = {
            let up = uptime_for_storage.read().unwrap();
            (
                up.availability_snapshot(StreamId::A),
                up.availability_snapshot(StreamId::B),
                up.availability_snapshot(StreamId::Baseline1),
                up.availability_snapshot(StreamId::Baseline2),
            )
        };

        loop {
            interval.tick().await;
            let current_hour = chrono::Utc::now().format("%Y-%m-%d %H").to_string();

            if current_hour != last_hour {
                let (count_a, count_b, baseline_1_count, baseline_2_count) = {
                    let internal = stats_for_storage.read().unwrap();
                    let up = uptime_for_storage.read().unwrap();
                    (
                        internal.total_a,
                        internal.total_b,
                        up.baseline_1.total_messages,
                        up.baseline_2.total_messages,
                    )
                };
                if let Err(e) = storage_arc_hourly
                    .save_hourly(
                        chrono::Utc::now(),
                        count_a,
                        count_b,
                        baseline_1_count,
                        baseline_2_count,
                    )
                    .await
                {
                    tracing::error!("Failed to save hourly stats: {}", e);
                    hourly_health_for_task.record_failure(chrono::Utc::now());
                }

                let current_snapshot = {
                    let up = uptime_for_storage.read().unwrap();
                    up.get_metrics_snapshot()
                };
                let interval_metrics = build_interval_metrics(previous_snapshot, current_snapshot);
                let current_availability = {
                    let up = uptime_for_storage.read().unwrap();
                    (
                        up.availability_snapshot(StreamId::A),
                        up.availability_snapshot(StreamId::B),
                        up.availability_snapshot(StreamId::Baseline1),
                        up.availability_snapshot(StreamId::Baseline2),
                    )
                };

                if let Err(e) = storage_arc_hourly
                    .save_hourly_uptime(
                        chrono::Utc::now(),
                        interval_metrics.uptime_a_seconds,
                        interval_metrics.uptime_b_seconds,
                        interval_metrics.downtime_a_seconds,
                        interval_metrics.downtime_b_seconds,
                        interval_metrics.disconnect_count_a,
                        interval_metrics.disconnect_count_b,
                        interval_metrics.avg_connect_time_a_ms,
                        interval_metrics.avg_connect_time_b_ms,
                        interval_metrics.messages_a,
                        interval_metrics.messages_b,
                        interval_metrics.delivery_latency_a_ms,
                        interval_metrics.delivery_latency_b_ms,
                        interval_metrics.mttr_a_ms,
                        interval_metrics.mttr_b_ms,
                        interval_metrics.baseline_1_uptime_seconds,
                        interval_metrics.baseline_2_uptime_seconds,
                        interval_metrics.baseline_1_downtime_seconds,
                        interval_metrics.baseline_2_downtime_seconds,
                        interval_metrics.baseline_1_messages,
                        interval_metrics.baseline_2_messages,
                        current_availability.0.outage_episodes
                            .saturating_sub(previous_availability.0.outage_episodes),
                        current_availability.1.outage_episodes
                            .saturating_sub(previous_availability.1.outage_episodes),
                        current_availability.0.reconnect_attempts
                            .saturating_sub(previous_availability.0.reconnect_attempts),
                        current_availability.1.reconnect_attempts
                            .saturating_sub(previous_availability.1.reconnect_attempts),
                        HOURLY_UPTIME_CONTRACT_VERSION,
                    )
                    .await
                {
                    tracing::error!("Failed to save hourly uptime: {}", e);
                    hourly_health_for_task.record_failure(chrono::Utc::now());
                }

                let reliability = ReliabilityHistory {
                    stream_a: availability_interval(
                        &previous_availability.0,
                        &current_availability.0,
                    ),
                    stream_b: availability_interval(
                        &previous_availability.1,
                        &current_availability.1,
                    ),
                    baseline_1: availability_interval(
                        &previous_availability.2,
                        &current_availability.2,
                    ),
                    baseline_2: availability_interval(
                        &previous_availability.3,
                        &current_availability.3,
                    ),
                    event_time: {
                        let tracker = uptime_for_storage.read().unwrap();
                        let stream_a = tracker.event_time_snapshot(StreamId::A);
                        let stream_b = tracker.event_time_snapshot(StreamId::B);
                        EventTimeHistory {
                            stream_a,
                            stream_b,
                            baseline_1: tracker.event_time_snapshot(StreamId::Baseline1),
                            baseline_2: tracker.event_time_snapshot(StreamId::Baseline2),
                            comparison: comparison_eligibility(
                                &stream_a,
                                &stream_b,
                                tracker.watermark_skew_threshold(),
                            ),
                            comparisons: tracker.pairwise_comparisons(),
                        }
                    },
                };
                if let Err(e) = storage_arc_hourly
                    .save_hourly_reliability(chrono::Utc::now(), &reliability)
                    .await
                {
                    tracing::error!("Failed to save hourly reliability: {}", e);
                    hourly_health_for_task.record_failure(chrono::Utc::now());
                }

                if let Err(e) = storage_arc_hourly
                    .save_lifetime_totals(
                        current_snapshot.total_messages_a,
                        current_snapshot.total_messages_b,
                    )
                    .await
                {
                    tracing::error!("Failed to save lifetime totals: {}", e);
                    hourly_health_for_task.record_failure(chrono::Utc::now());
                }
                hourly_health_for_task.record_success(chrono::Utc::now());

                previous_snapshot = current_snapshot;
                previous_availability = current_availability;
                last_hour = current_hour;
            }
        }
    });

    async fn serve_spa(
        req: axum::http::Request<axum::body::Body>,
    ) -> std::result::Result<axum::http::Response<axum::body::Body>, axum::response::ErrorResponse>
    {
        let manifest_dir = std::env::var("CARGO_MANIFEST_DIR")
            .or_else(|_| std::env::current_dir().map(|p| p.to_string_lossy().to_string()))
            .unwrap_or_default();

        let static_base = std::path::Path::new(&manifest_dir).join("frontend/dist");
        let path = req.uri().path().to_string();

        if path != "/" {
            let file_path = static_base.join(&path[1..]);
            if let Ok(content) = tokio::fs::read(&file_path).await {
                let (mime_type, cache_control) = match std::path::Path::new(&file_path)
                    .extension()
                    .and_then(|e| e.to_str())
                {
                    Some("js") => (
                        "application/javascript",
                        "public, max-age=31536000, immutable",
                    ),
                    Some("css") => ("text/css", "public, max-age=31536000, immutable"),
                    Some("woff2") => ("font/woff2", "public, max-age=31536000, immutable"),
                    Some("json") => ("application/json", "public, max-age=3600"),
                    _ => (
                        "application/octet-stream",
                        "public, max-age=31536000, immutable",
                    ),
                };
                let mut response = axum::http::Response::new(axum::body::Body::from(content));
                response
                    .headers_mut()
                    .insert(axum::http::header::CONTENT_TYPE, mime_type.parse().unwrap());
                response.headers_mut().insert(
                    axum::http::header::CACHE_CONTROL,
                    cache_control.parse().unwrap(),
                );
                return Ok(response);
            }
        }

        let index_path = static_base.join("index.html");
        if let Ok(content) = tokio::fs::read(&index_path).await {
            let mut response = axum::http::Response::new(axum::body::Body::from(content));
            response.headers_mut().insert(
                axum::http::header::CONTENT_TYPE,
                "text/html".parse().unwrap(),
            );
            response.headers_mut().insert(
                axum::http::header::CACHE_CONTROL,
                "no-cache".parse().unwrap(),
            );
            return Ok(response);
        }

        let mut response =
            axum::http::Response::new(axum::body::Body::from("<html><body>404</body></html>"));
        response.headers_mut().insert(
            axum::http::header::CONTENT_TYPE,
            "text/html".parse().unwrap(),
        );
        response.headers_mut().insert(
            axum::http::header::CACHE_CONTROL,
            "no-cache".parse().unwrap(),
        );
        Ok(response)
    }

    let incident_state_for_router: Arc<jetstream_monitor::incidents::IncidentStore> =
        Arc::clone(&incident_store);
    let app_state = AppState {
        broadcast_tx: Arc::clone(&broadcast_tx),
        storage: Arc::clone(&storage_arc),
        uptime: Arc::clone(&uptime_tracker),
        operational: Arc::clone(&operational),
        ledger_health: Arc::clone(&ledger_health),
        hourly_health: Arc::clone(&hourly_health),
        metrics: Arc::clone(&metrics),
        process_epoch: Arc::new(monitor_identity.process_epoch.clone()),
        release: Arc::new(monitor_identity.release.clone()),
        started_at: process_started_at,
        incident_store: Arc::clone(&incident_store),
    };
    let ws_state = jetstream_monitor::websocket::WsState {
        broadcast_tx: Arc::clone(&broadcast_tx),
    };
    let ws_router = axum::Router::new()
        .route("/ws", axum::routing::get(websocket::ws_handler))
        .with_state(ws_state);
    let operational_router = axum::Router::new()
        .route("/api/history", axum::routing::get(get_history))
        .route("/api/uptime", axum::routing::get(get_uptime))
        .route(
            "/api/uptime-detailed",
            axum::routing::get(get_uptime_detailed),
        )
        .route("/api/v1/health", axum::routing::get(get_api_health))
        .route("/api/v1/metrics", axum::routing::get(get_api_metrics))
        .route("/openapi.json", axum::routing::get(get_openapi))
        .with_state(app_state);
    let incidents_router = axum::Router::new()
        .route("/api/v1/incidents", axum::routing::get(jetstream_monitor::api::incidents::incident_list))
        .route(
            "/api/v1/incidents/{incidentId}",
            axum::routing::get(jetstream_monitor::api::incidents::incident_detail),
        )
        .route(
            "/api/v1/incidents/{incidentId}/events",
            axum::routing::get(jetstream_monitor::api::incidents::incident_events),
        )
        .with_state(Arc::clone(&incident_state_for_router));
    let operational_router = operational_router.merge(incidents_router);
    let app = ws_router
        .merge(operational_router)
        .layer(tower_http::trace::TraceLayer::new_for_http())
        .fallback(serve_spa);

    let listener = tokio::net::TcpListener::bind(&settings.bind_address).await?;
    tracing::info!("Listening on {}", settings.bind_address);

    axum::serve(listener, app).await?;

    Ok(())
}

/// Apply one ordered incident command to the durable store.
async fn apply_incident_command(
    store: &IncidentStore,
    command: &IncidentCommand,
    identity: &MonitorIdentity,
) -> anyhow::Result<()> {
    match command {
        IncidentCommand::Open {
            incident_id,
            stream,
            trigger,
            detected_at,
            last_useful_record_at,
            connection_epoch,
        } => {
            store
                .open_incident(
                    incident_id,
                    stream,
                    *trigger,
                    *detected_at,
                    *last_useful_record_at,
                    *connection_epoch,
                    identity,
                )
                .await?;
        }
        IncidentCommand::AppendEvent { incident_id, event } => {
            store.append_event(incident_id, event.clone()).await?;
        }
        IncidentCommand::TransportRecovered {
            incident_id,
            recovered_at,
        } => {
            store
                .record_transport_recovered(incident_id, *recovered_at)
                .await?;
        }
        IncidentCommand::Resolve {
            incident_id,
            resolved_at,
        } => {
            store.resolve_incident(incident_id, *resolved_at).await?;
        }
        IncidentCommand::ReconcileOpen { .. } => {
            // Startup reconciliation runs directly, not through the queue.
            let _ = identity;
        }
    }
    Ok(())
}

async fn get_history(
    axum::extract::Query(params): axum::extract::Query<std::collections::HashMap<String, String>>,
    axum::extract::State(app): axum::extract::State<AppState>,
) -> axum::Json<Vec<HourlyStat>> {
    let storage = &app.storage;
    let hours: i64 = params
        .get("hours")
        .and_then(|h| h.parse().ok())
        .unwrap_or(24);

    let since = chrono::Utc::now() - chrono::Duration::hours(hours);

    match storage.get_stats_since(since).await {
        Ok(stats) => axum::Json(stats),
        Err(_) => axum::Json(vec![]),
    }
}

async fn get_uptime(
    axum::extract::Query(params): axum::extract::Query<std::collections::HashMap<String, String>>,
    axum::extract::State(app): axum::extract::State<AppState>,
) -> axum::Json<UptimeResponse> {
    let storage = &app.storage;
    let hours: i64 = params
        .get("hours")
        .and_then(|h| h.parse().ok())
        .unwrap_or(24)
        .max(0);

    let since = chrono::Utc::now() - chrono::Duration::hours(hours);

    match storage.get_uptime_since(since).await {
        Ok(data) => {
            let span_seconds = if data.is_empty() {
                hours * 3600
            } else {
                let first = chrono::DateTime::parse_from_rfc3339(&format!(
                    "{}:00+00:00",
                    data.first().unwrap().hour
                ))
                .map(|dt| dt.with_timezone(&chrono::Utc))
                .unwrap_or_else(|_| chrono::Utc::now() - chrono::Duration::hours(hours));
                let last = chrono::DateTime::parse_from_rfc3339(&format!(
                    "{}:00+00:00",
                    data.last().unwrap().hour
                ))
                .map(|dt| dt.with_timezone(&chrono::Utc))
                .unwrap_or_else(|_| chrono::Utc::now());
                (last - first).num_seconds() + 3600
            };
            axum::Json(UptimeResponse {
                data,
                span_seconds,
                requested_window_seconds: hours * HOURLY_INTERVAL_SECONDS_I64,
                interval_seconds: HOURLY_INTERVAL_SECONDS_I64,
            })
        }
        Err(_) => axum::Json(UptimeResponse {
            data: vec![],
            span_seconds: hours * HOURLY_INTERVAL_SECONDS_I64,
            requested_window_seconds: hours * HOURLY_INTERVAL_SECONDS_I64,
            interval_seconds: HOURLY_INTERVAL_SECONDS_I64,
        }),
    }
}

async fn get_uptime_detailed(
    axum::extract::Query(params): axum::extract::Query<std::collections::HashMap<String, String>>,
    axum::extract::State(app): axum::extract::State<AppState>,
) -> axum::Json<UptimeDetailedStats> {
    let storage = &app.storage;
    let uptime_tracker = &app.uptime;
    let hours: i64 = params
        .get("hours")
        .and_then(|h| h.parse().ok())
        .unwrap_or(24)
        .max(0);

    let period_seconds = (hours as u64).saturating_mul(HOURLY_INTERVAL_SECONDS);
    let mut detailed = uptime_tracker
        .read()
        .unwrap()
        .get_detailed_stats(period_seconds);

    let since = chrono::Utc::now() - chrono::Duration::hours(hours);
    if let Ok(data) = storage.get_uptime_since(since).await {
        if data.is_empty() {
            return axum::Json(detailed);
        }

        let window = rollup_window(&data);

        let observed_a_seconds = window
            .uptime_a_seconds
            .saturating_add(window.downtime_a_seconds);
        let observed_b_seconds = window
            .uptime_b_seconds
            .saturating_add(window.downtime_b_seconds);

        detailed.window_requested_seconds = period_seconds;
        detailed.window_observed_a_seconds = observed_a_seconds;
        detailed.window_observed_b_seconds = observed_b_seconds;
        detailed.window_uptime_a_seconds = window.uptime_a_seconds;
        detailed.window_uptime_b_seconds = window.uptime_b_seconds;
        detailed.window_downtime_a_seconds = window.downtime_a_seconds;
        detailed.window_downtime_b_seconds = window.downtime_b_seconds;
        detailed.window_uptime_a_percent = if observed_a_seconds > 0 {
            (window.uptime_a_seconds as f64 / observed_a_seconds as f64) * 100.0
        } else {
            0.0
        };
        detailed.window_uptime_b_percent = if observed_b_seconds > 0 {
            (window.uptime_b_seconds as f64 / observed_b_seconds as f64) * 100.0
        } else {
            0.0
        };

        // Legacy fields retained for compatibility: mirror explicit window fields.
        detailed.uptime_a_seconds = detailed.window_uptime_a_seconds;
        detailed.uptime_b_seconds = detailed.window_uptime_b_seconds;
        detailed.downtime_a_seconds = detailed.window_downtime_a_seconds;
        detailed.downtime_b_seconds = detailed.window_downtime_b_seconds;
        detailed.uptime_a_percent = detailed.window_uptime_a_percent;
        detailed.uptime_b_percent = detailed.window_uptime_b_percent;
    }

    axum::Json(detailed)
}

#[utoipa::path(
    get,
    path = "/api/v1/health",
    operation_id = "getHealth",
    tag = "observability",
    responses(
        (status = 200, description = "Monitor operational; status may be healthy or degraded",
         body = inline(ApiResponse<HealthSnapshot>),
         headers(("cache-control" = String, description = "no-store"))),
        (status = 503, description = "Monitor cannot provide trustworthy observation", body = ApiError),
    )
)]
async fn get_api_health(
    axum::extract::State(app): axum::extract::State<AppState>,
) -> axum::http::Response<axum::body::Body> {
    use jetstream_monitor::stream::StreamId;

    let ops = app.operational.lock().unwrap_or_else(|p| p.into_inner());
    let now = chrono::Utc::now();

    let mut streams = Vec::new();
    for (stream_id, stream_key) in [
        (StreamId::A, "a"),
        (StreamId::B, "b"),
        (StreamId::Baseline1, "baseline1"),
        (StreamId::Baseline2, "baseline2"),
    ] {
        let availability = app.uptime.read().unwrap().availability_snapshot(stream_id);
        let lag_us = app.uptime.read().unwrap().event_time_snapshot(stream_id).source_lag_us;
        let entry = match ops.streams.get(&stream_id) {
            Some(entry) => entry,
            None => continue,
        };
        streams.push(StreamHealth {
            stream_id: stream_key.to_string(),
            transport: entry.transport.clone(),
            delivery: entry.delivery.clone(),
            delivery_idle: entry.delivery_idle,
            state_age_seconds: entry
                .age_started
                .map(|t| t.elapsed().as_secs())
                .unwrap_or(0),
            last_useful_record_age_seconds: entry
                .last_record_at
                .map(|t| t.elapsed().as_secs()),
            last_pong_age_seconds: entry.liveness.as_ref().and_then(|c| c.age_seconds()),
            source_lag_seconds: lag_us
                .map(|us| us / 1_000_000),
            connection_epoch: 0,
            reconnect_attempts: availability.reconnect_attempts,
            outage_elapsed_ms: availability.outage_elapsed_ms,
            outage_episodes: availability.outage_episodes,
            idle_episodes: availability.idle_episodes,
            active_incident_id: entry.active_incident_id.clone(),
        });
    }
    let observation_loop_alive = ops.observation_loop_alive;
    drop(ops);

    let snapshot = HealthSnapshot::compute(
        now,
        (*app.process_epoch).clone(),
        (*app.release).clone(),
        app.started_at,
        observation_loop_alive,
        StorageHealth {
            available: app.ledger_health.healthy(),
            last_success_age_seconds: app.ledger_health.last_success_age_seconds(now),
            stale_after_seconds: 3600,
        },
        StorageHealth {
            available: true,
            last_success_age_seconds: app.hourly_health.last_success_age_seconds(now),
            stale_after_seconds: 7200,
        },
        streams,
    );

    let status_code = match snapshot.status {
        HealthStatus::Unhealthy => axum::http::StatusCode::SERVICE_UNAVAILABLE,
        _ => axum::http::StatusCode::OK,
    };
    let body = serde_json::to_string(&ApiResponse { data: snapshot })
        .unwrap_or_else(|_| "{}".to_string());
    axum::http::Response::builder()
        .status(status_code)
        .header("content-type", "application/json")
        .header("cache-control", jetstream_monitor::api::NO_STORE)
        .body(axum::body::Body::from(body))
        .unwrap()
}

fn generate_openapi_document() -> utoipa::openapi::OpenApi {
    ApiDoc::openapi()
}

#[utoipa::path(
    get,
    path = "/openapi.json",
    operation_id = "getOpenApiDocument",
    responses(
        (status = 200, description = "Canonical OpenAPI 3.1 discovery document",
         content_type = "application/json",
         headers(
             ("etag" = String, description = "Contract ETag"),
             ("cache-control" = String, description = "public, max-age=60"),
             ("x-monitor-release" = String, description = "Deployed monitor release"),
         )),
    ),
    security(()),
)]
async fn get_openapi(
    axum::extract::State(app): axum::extract::State<AppState>,
) -> axum::http::Response<axum::body::Body> {
    let mut doc = generate_openapi_document();
    jetstream_monitor::api::openapi::set_servers(&mut doc, API_SERVER_URL);
    let body = jetstream_monitor::api::openapi::render_document(&doc);
    axum::http::Response::builder()
        .status(axum::http::StatusCode::OK)
        .header("content-type", "application/vnd.oai.openapi+json")
        .header("etag", jetstream_monitor::api::openapi::contract_etag(&body))
        .header("cache-control", "public, max-age=60")
        .header("x-monitor-release", (*app.release).clone())
        .body(axum::body::Body::from(body))
        .unwrap()
}

#[utoipa::path(
    get,
    path = "/api/v1/metrics",
    operation_id = "getMetrics",
    tag = "observability",
    responses(
        (status = 200, description = "Prometheus text exposition",
         content_type = "text/plain; version=0.0.4",
         headers(("cache-control" = String, description = "no-store"))),
    )
)]
async fn get_api_metrics(axum::extract::State(app): axum::extract::State<AppState>) -> axum::http::Response<axum::body::Body> {
    let body = app.metrics.render();
    axum::http::Response::builder()
        .status(axum::http::StatusCode::OK)
        .header("content-type", Metrics::content_type())
        .header("cache-control", jetstream_monitor::api::NO_STORE)
        .body(axum::body::Body::from(body))
        .unwrap()
}

/// Canonical OpenAPI 3.1 contract for the operational API.
#[derive(utoipa::OpenApi)]
#[openapi(
    info(
        title = "Jetstream Monitor Operational API",
        version = jetstream_monitor::api::OPENAPI_CONTRACT_VERSION,
        description = "Read-only operational surface for the jetstream monitor: monitor self-health, Prometheus metrics, and the durable delivery-disruption incident ledger. The API is unauthenticated and sanitized; deployment may restrict detailed endpoints at the reverse proxy. Pagination uses opaque cursors. Operational endpoints are versioned under /api/v1; breaking changes require a new version.",
    ),
    paths(
        jetstream_monitor::api::incidents::incident_list,
        jetstream_monitor::api::incidents::incident_detail,
        jetstream_monitor::api::incidents::incident_events,
        get_api_health,
        get_api_metrics,
        get_openapi,
    ),
    components(
        schemas(
            jetstream_monitor::api::incidents::IncidentSummaryDto,
            jetstream_monitor::api::incidents::IncidentListDto,
            jetstream_monitor::api::incidents::IncidentEventDto,
            jetstream_monitor::api::incidents::IncidentEventListDto,
            ApiResponse<jetstream_monitor::api::incidents::IncidentListDto>,
            ApiResponse<jetstream_monitor::api::incidents::IncidentSummaryDto>,
            ApiResponse<jetstream_monitor::api::incidents::IncidentEventListDto>,
            jetstream_monitor::api::health::HealthSnapshot,
            jetstream_monitor::api::health::HealthStatus,
            jetstream_monitor::api::health::StorageHealth,
            jetstream_monitor::api::health::StreamHealth,
            ApiResponse<HealthSnapshot>,
            jetstream_monitor::api::ApiError,
        )
    ),
    tags(
        (name = "incidents", description = "Durable delivery-disruption incidents and their ordered event histories"),
        (name = "observability", description = "Monitor self-health and Prometheus metrics"),
    ),
    security(
        (),
    ),
)]
struct ApiDoc;

#[cfg(test)]
mod openapi_tests {
    use super::*;

    fn document() -> utoipa::openapi::OpenApi {
        let mut doc = ApiDoc::openapi();
        jetstream_monitor::api::openapi::set_servers(&mut doc, "http://localhost:3001");
        doc
    }

    fn snapshot_path() -> std::path::PathBuf {
        let manifest_dir = std::env::var("CARGO_MANIFEST_DIR").unwrap();
        std::path::Path::new(&manifest_dir).join("openapi/openapi.json")
    }

    fn normalized_json(doc: &utoipa::openapi::OpenApi) -> String {
        jetstream_monitor::api::openapi::render_document(doc)
    }

    #[test]
    fn contract_is_openapi_31_with_stable_ops() {
        let raw = serde_json::to_value(document()).unwrap();
        assert_eq!(raw["openapi"], "3.1.0");
        let paths = raw["paths"].as_object().unwrap();
        for path in [
            "/api/v1/health",
            "/api/v1/metrics",
            "/api/v1/incidents",
            "/api/v1/incidents/{incidentId}",
            "/api/v1/incidents/{incidentId}/events",
            "/openapi.json",
        ] {
            assert!(paths.contains_key(path), "missing path {path}");
        }
        let ops = [
            "getHealth",
            "getMetrics",
            "listIncidents",
            "getIncident",
            "listIncidentEvents",
            "getOpenApiDocument",
        ];
        for operation_id in ops {
            let found = paths.values().any(|p| {
                p.as_object()
                    .map(|methods| {
                        methods.values().any(|v| {
                            v.get("operationId").and_then(|id| id.as_str()) == Some(operation_id)
                        })
                    })
                    .unwrap_or(false)
            });
            assert!(found, "missing operationId {operation_id}");
        }
    }

    #[test]
    fn contract_matches_committed_snapshot() {
        let doc = document();
        let rendered = normalized_json(&doc);
        let path = snapshot_path();
        match std::env::var("OPENAPI_SNAPSHOT_WRITE").as_deref() {
            Ok("1") => {
                let _ = std::fs::create_dir_all(path.parent().unwrap());
                std::fs::write(&path, format!("{rendered}\n")).unwrap();
            }
            _ => {}
        }
        let committed = std::fs::read_to_string(&path)
            .expect("checked-in OpenAPI snapshot must exist; run with OPENAPI_SNAPSHOT_WRITE=1 to refresh");
        assert_eq!(
            rendered.trim(),
            committed.trim(),
            "runtime OpenAPI drifted from the committed snapshot"
        );
    }

    #[test]
    fn contract_declares_limits_defaults_and_media_types() {
        let raw = serde_json::to_value(document()).unwrap();
        let serialized = serde_json::to_string(&raw).unwrap();
        assert!(serialized.contains("text/plain; version=0.0.4"));
        assert!(serialized.contains("maximum"));
        // Production server metadata is declared.
        assert_eq!(raw["servers"][0]["url"], "http://localhost:3001");
        // Deployed security posture: no authentication required.
        assert!(!serialized.contains("\"security\":[{\"apiKeyAuth\""));
    }
}

#[cfg(test)]
mod incident_api_tests {
    use axum::http::StatusCode;
    use jetstream_monitor::api::incidents::{
        build_filter, incident_detail_response, incident_events_response,
        list_incidents_response, IncidentListQuery,
    };
    use jetstream_monitor::incidents::store::IncidentStore;
    use jetstream_monitor::incidents::{
        HandshakeFailureReason, IncidentEvent, IncidentEventType, IncidentId, IncidentTrigger,
        IncidentTrigger as _, MonitorIdentity,
    };
    use std::str::FromStr;

    async fn temp_store(name: &str) -> IncidentStore {
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let path = std::env::temp_dir().join(format!("incident-api-{name}-{nanos}.db"));
        let url = format!("sqlite://{}?mode=rwc", path.display());
        let options = sqlx::sqlite::SqliteConnectOptions::from_str(&url)
            .unwrap()
            .foreign_keys(true)
            .create_if_missing(true);
        let pool = sqlx::SqlitePool::connect_with(options).await.unwrap();
        IncidentStore::new(pool).await.unwrap()
    }

    async fn seed(store: &IncidentStore, id: &str, stream: &str, silences: u64) {
        let identity = MonitorIdentity {
            process_epoch: "test".to_string(),
            release: "0.1.0".to_string(),
        };
        let incident = IncidentId::from_string(id).unwrap();
        store
            .open_incident(
                &incident,
                stream,
                IncidentTrigger::DeliveryIdle,
                chrono::Utc::now(),
                Some(chrono::Utc::now() - chrono::Duration::seconds(30)),
                1,
                &identity,
            )
            .await
            .unwrap();
        if silences > 0 {
            store
                .append_event(
                    &incident,
                    IncidentEvent {
                        sequence: 1,
                        event_type: IncidentEventType::ReconnectAttemptFailed,
                        occurred_at: chrono::Utc::now(),
                        reason: Some(HandshakeFailureReason::ConnectTimeout.as_str().to_string()),
                        attempt_ordinal: Some(1),
                        scheduled_delay_ms: Some(1_000),
                    },
                )
                .await
                .unwrap();
        }
    }

    async fn body_json(response: axum::response::Response) -> serde_json::Value {
        let bytes = axum::body::to_bytes(
            response.into_body(),
            usize::MAX,
        )
        .await
        .unwrap();
        serde_json::from_slice(&bytes).unwrap()
    }

    #[tokio::test]
    async fn invalid_limit_and_cursor_return_400() {
        let store = temp_store("invalid-params").await;
        let mut query = IncidentListQuery {
            limit: Some(999),
            ..Default::default()
        };
        let response = list_incidents_response(&store, &query).await;
        assert_eq!(response.status(), StatusCode::BAD_REQUEST);

        query.cursor = Some("not-a-cursor".to_string());
        let response = list_incidents_response(&store, &query).await;
        assert_eq!(response.status(), StatusCode::BAD_REQUEST);
    }

    #[tokio::test]
    async fn paging_without_gaps_or_duplicates_and_invalid_event_cursors() {
        let store = temp_store("paging").await;
        let seeds = [
            ("01ARZ3NDEKTSV4RRFFQ69G5FAZ", "a", 0u64),
            ("01ARZ3NDEKTSV4RRFFQ69G5FBX", "a", 1),
            ("01ARZ3NDEKTSV4RRFFQ69G5FCX", "b", 2),
        ];
        for (id, stream, attempts) in seeds {
            seed(&store, id, stream, attempts).await;
        }

        // Page through the cursor chain without gaps or duplicates.
        let mut seen = Vec::new();
        let mut cursor: Option<String> = None;
        loop {
            let query = IncidentListQuery {
                cursor: cursor.clone(),
                limit: Some(2),
                ..Default::default()
            };
            let response = list_incidents_response(&store, &query).await;
            assert_eq!(response.status(), StatusCode::OK);
            assert!(
                response
                    .headers()
                    .get("cache-control")
                    .map(|v| v == "no-store")
                    .unwrap_or(false)
            );
            let json = body_json(response).await;
            for incident in json["data"]["incidents"].as_array().unwrap() {
                seen.push(incident["id"].as_str().unwrap().to_string());
            }
            match json["data"]["next_cursor"].as_str().map(str::to_string) {
                Some(next) => cursor = Some(next),
                None => break,
            }
        }
        assert_eq!(seen.len(), 3);
        assert_eq!(
            seen.iter().collect::<std::collections::HashSet<_>>().len(),
            seen.len()
        );

        // Events page without gaps or duplicates.
        let response =
            incident_events_response(&store, "01ARZ3NDEKTSV4RRFFQ69G5FBX", None, None).await;
        assert_eq!(response.status(), StatusCode::OK);
        let json = body_json(response).await;
        let events = json["data"]["events"].as_array().unwrap();
        assert_eq!(events.len(), 1);
        assert_eq!(events[0]["sequence"], 1);

        // Unknown incident -> 404 for detail and events.
        let response = incident_detail_response(&store, "01ARZ3NDEKTSV4RRFFQ69G5FZX").await;
        assert_eq!(response.status(), StatusCode::NOT_FOUND);
        let response =
            incident_events_response(&store, "01ARZ3NDEKTSV4RRFFQ69G5FZX", None, None).await;
        assert_eq!(response.status(), StatusCode::NOT_FOUND);

        // Stream filter narrows results.
        let filter = build_filter(Some("b"), None, None, None, None, None).unwrap();
        let query = IncidentListQuery {
            stream: filter.stream_id.clone(),
            ..Default::default()
        };
        let response = list_incidents_response(&store, &query).await;
        let json = body_json(response).await;
        assert_eq!(json["data"]["incidents"].as_array().unwrap().len(), 1);
        assert_eq!(json["data"]["incidents"][0]["stream_id"], "b");
    }
}
