use anyhow::Result;
use jetstream_monitor::{
    config::Settings,
    stats::{
        StatsAggregator, StreamStatsInternal, UptimeDetailedStats, UptimeMetricsSnapshot,
        UptimeTracker,
    },
    storage::{HourlyStat, HourlyUptime, Storage, UptimeResponse},
    stream::{StreamClient, StreamId},
    websocket,
};
use std::sync::Arc;

const HOURLY_INTERVAL_SECONDS: u64 = 3600;
const HOURLY_INTERVAL_SECONDS_I64: i64 = 3600;
const HOURLY_UPTIME_CONTRACT_VERSION: i64 = 2;

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

    let stats_internal = Arc::new(std::sync::RwLock::new(StreamStatsInternal::default()));
    stats_internal
        .write()
        .unwrap()
        .load_totals(lifetime_a, lifetime_b);

    let uptime_tracker = Arc::new(std::sync::RwLock::new(UptimeTracker::default()));
    uptime_tracker
        .write()
        .unwrap()
        .load_totals(lifetime_a, lifetime_b);
    let aggregator = StatsAggregator::new(
        settings.stream_a_name.clone(),
        settings.stream_b_name.clone(),
    );
    let broadcast_tx = Arc::new(aggregator.sender());

    let client_a = StreamClient::new(settings.stream_a_url.clone(), StreamId::A);
    let client_b = StreamClient::new(settings.stream_b_url.clone(), StreamId::B);

    let stats_for_stream = Arc::clone(&stats_internal);
    let uptime_for_status: Arc<std::sync::RwLock<UptimeTracker>> = Arc::clone(&uptime_tracker);
    tokio::spawn(async move {
        use futures::StreamExt;
        let (mut stream_a, mut status_a) = client_a.stream_with_status();
        let (mut stream_b, mut status_b) = client_b.stream_with_status();

        loop {
            tokio::select! {
                Some(msg) = stream_a.next() => {
                    let count = msg.count;
                    let delivery_latency_us = msg.delivery_latency_us;
                    stats_for_stream.write().unwrap().update(msg);
                    let mut tracker = uptime_for_status.write().unwrap();
                    tracker.record_total_count(StreamId::A, count);
                    if let Some(lat) = delivery_latency_us {
                        tracker.record_delivery_latency(StreamId::A, lat);
                    }
                }
                Some(msg) = stream_b.next() => {
                    let count = msg.count;
                    let delivery_latency_us = msg.delivery_latency_us;
                    stats_for_stream.write().unwrap().update(msg);
                    let mut tracker = uptime_for_status.write().unwrap();
                    tracker.record_total_count(StreamId::B, count);
                    if let Some(lat) = delivery_latency_us {
                        tracker.record_delivery_latency(StreamId::B, lat);
                    }
                }
                Some(status) = status_a.next() => {
                    uptime_for_status.write().unwrap().handle_connection_status(status);
                }
                Some(status) = status_b.next() => {
                    uptime_for_status.write().unwrap().handle_connection_status(status);
                }
                else => break,
            }
        }
    });

    aggregator.process(&stats_internal, &uptime_tracker);

    let stats_for_storage = Arc::clone(&stats_internal);
    let uptime_for_storage: Arc<std::sync::RwLock<UptimeTracker>> = Arc::clone(&uptime_tracker);
    let storage_arc = Arc::new(storage);
    let storage_for_api = Arc::clone(&storage_arc);
    let uptime_for_api: Arc<std::sync::RwLock<UptimeTracker>> = Arc::clone(&uptime_tracker);
    tokio::spawn(async move {
        let mut interval =
            tokio::time::interval(std::time::Duration::from_secs(HOURLY_INTERVAL_SECONDS));
        let mut last_hour = chrono::Utc::now().format("%Y-%m-%d %H").to_string();
        let mut previous_snapshot = {
            let up = uptime_for_storage.read().unwrap();
            up.get_metrics_snapshot()
        };

        loop {
            interval.tick().await;
            let current_hour = chrono::Utc::now().format("%Y-%m-%d %H").to_string();

            if current_hour != last_hour {
                let (count_a, count_b) = {
                    let internal = stats_for_storage.read().unwrap();
                    (internal.total_a, internal.total_b)
                };
                if let Err(e) = storage_arc
                    .save_hourly(chrono::Utc::now(), count_a, count_b)
                    .await
                {
                    tracing::error!("Failed to save hourly stats: {}", e);
                }

                let current_snapshot = {
                    let up = uptime_for_storage.read().unwrap();
                    up.get_metrics_snapshot()
                };
                let interval_metrics = build_interval_metrics(previous_snapshot, current_snapshot);

                if let Err(e) = storage_arc
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
                        HOURLY_UPTIME_CONTRACT_VERSION,
                    )
                    .await
                {
                    tracing::error!("Failed to save hourly uptime: {}", e);
                }

                if let Err(e) = storage_arc
                    .save_lifetime_totals(
                        current_snapshot.total_messages_a,
                        current_snapshot.total_messages_b,
                    )
                    .await
                {
                    tracing::error!("Failed to save lifetime totals: {}", e);
                }

                previous_snapshot = current_snapshot;
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

    let app = axum::Router::new()
        .route("/ws", axum::routing::get(websocket::ws_handler))
        .route("/api/history", axum::routing::get(get_history))
        .route("/api/uptime", axum::routing::get(get_uptime))
        .route(
            "/api/uptime-detailed",
            axum::routing::get(get_uptime_detailed),
        )
        .with_state((broadcast_tx, storage_for_api, uptime_for_api))
        .layer(tower_http::trace::TraceLayer::new_for_http())
        .fallback(serve_spa);

    let listener = tokio::net::TcpListener::bind(&settings.bind_address).await?;
    tracing::info!("Listening on {}", settings.bind_address);

    axum::serve(listener, app).await?;

    Ok(())
}

async fn get_history(
    axum::extract::Query(params): axum::extract::Query<std::collections::HashMap<String, String>>,
    axum::extract::State((_, storage, _)): axum::extract::State<(
        Arc<tokio::sync::broadcast::Sender<jetstream_monitor::StreamStats>>,
        Arc<Storage>,
        Arc<std::sync::RwLock<UptimeTracker>>,
    )>,
) -> axum::Json<Vec<HourlyStat>> {
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
    axum::extract::State((_, storage, _)): axum::extract::State<(
        Arc<tokio::sync::broadcast::Sender<jetstream_monitor::StreamStats>>,
        Arc<Storage>,
        Arc<std::sync::RwLock<UptimeTracker>>,
    )>,
) -> axum::Json<UptimeResponse> {
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
    axum::extract::State((_, storage, uptime_tracker)): axum::extract::State<(
        Arc<tokio::sync::broadcast::Sender<jetstream_monitor::StreamStats>>,
        Arc<Storage>,
        Arc<std::sync::RwLock<UptimeTracker>>,
    )>,
) -> axum::Json<UptimeDetailedStats> {
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
