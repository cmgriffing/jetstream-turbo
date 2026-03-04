use anyhow::Result;
use jetstream_monitor::{
    config::Settings,
    stats::{StatsAggregator, StreamStatsInternal, UptimeDetailedStats, UptimeTracker},
    storage::{HourlyStat, Storage, UptimeResponse},
    stream::{StreamClient, StreamId},
    websocket,
};
use std::sync::Arc;

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
    tracing::info!("Loaded lifetime totals: stream_a={}, stream_b={}", lifetime_a, lifetime_b);

    let stats_internal = Arc::new(std::sync::RwLock::new(StreamStatsInternal::default()));
    stats_internal.write().unwrap().load_totals(lifetime_a, lifetime_b);
    
    let uptime_tracker = Arc::new(std::sync::RwLock::new(UptimeTracker::default()));
    uptime_tracker.write().unwrap().load_totals(lifetime_a, lifetime_b);
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
                    stats_for_stream.write().unwrap().update(msg);
                    uptime_for_status.write().unwrap().record_message(StreamId::A);
                }
                Some(msg) = stream_b.next() => {
                    stats_for_stream.write().unwrap().update(msg);
                    uptime_for_status.write().unwrap().record_message(StreamId::B);
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
        let mut interval = tokio::time::interval(std::time::Duration::from_secs(3600));
        let mut last_hour = chrono::Utc::now().format("%Y-%m-%d %H").to_string();

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

                let detailed = {
                    let up = uptime_for_storage.read().unwrap();
                    up.get_detailed_stats(3600)
                };

                if let Err(e) = storage_arc
                    .save_hourly_uptime(
                        chrono::Utc::now(),
                        detailed.uptime_a_seconds,
                        detailed.uptime_b_seconds,
                        detailed.downtime_a_seconds,
                        detailed.downtime_b_seconds,
                        detailed.disconnect_count_a,
                        detailed.disconnect_count_b,
                        detailed.avg_latency_a_ms,
                        detailed.avg_latency_b_ms,
                        detailed.total_messages_a,
                        detailed.total_messages_b,
                    )
                    .await
                {
                    tracing::error!("Failed to save hourly uptime: {}", e);
                }

                if let Err(e) = storage_arc
                    .save_lifetime_totals(
                        detailed.total_messages_a,
                        detailed.total_messages_b,
                    )
                    .await
                {
                    tracing::error!("Failed to save lifetime totals: {}", e);
                }

                last_hour = current_hour;
            }
        }
    });

    async fn serve_spa(
        req: axum::http::Request<axum::body::Body>,
    ) -> std::result::Result<axum::http::Response<axum::body::Body>, axum::response::ErrorResponse> {
        let manifest_dir = std::env::var("CARGO_MANIFEST_DIR")
            .or_else(|_| {
                std::env::current_dir()
                    .map(|p| p.to_string_lossy().to_string())
            })
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
                    Some("js") => ("application/javascript", "public, max-age=31536000, immutable"),
                    Some("css") => ("text/css", "public, max-age=31536000, immutable"),
                    Some("woff2") => ("font/woff2", "public, max-age=31536000, immutable"),
                    Some("json") => ("application/json", "public, max-age=3600"),
                    _ => ("application/octet-stream", "public, max-age=31536000, immutable"),
                };
                let mut response = axum::http::Response::new(axum::body::Body::from(content));
                response.headers_mut().insert(
                    axum::http::header::CONTENT_TYPE,
                    mime_type.parse().unwrap(),
                );
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
        
        let mut response = axum::http::Response::new(axum::body::Body::from("<html><body>404</body></html>"));
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
        .unwrap_or(24);

    let since = chrono::Utc::now() - chrono::Duration::hours(hours);

    match storage.get_uptime_since(since).await {
        Ok(data) => {
            let span_seconds = if data.is_empty() {
                hours * 3600
            } else {
                let first = chrono::DateTime::parse_from_rfc3339(&format!("{}:00+00:00", data.first().unwrap().hour))
                    .map(|dt| dt.with_timezone(&chrono::Utc))
                    .unwrap_or_else(|_| chrono::Utc::now() - chrono::Duration::hours(hours));
                let last = chrono::DateTime::parse_from_rfc3339(&format!("{}:00+00:00", data.last().unwrap().hour))
                    .map(|dt| dt.with_timezone(&chrono::Utc))
                    .unwrap_or_else(|_| chrono::Utc::now());
                (last - first).num_seconds() + 3600
            };
            axum::Json(UptimeResponse { data, span_seconds })
        }
        Err(_) => axum::Json(UptimeResponse { data: vec![], span_seconds: hours * 3600 }),
    }
}

async fn get_uptime_detailed(
    axum::extract::Query(params): axum::extract::Query<std::collections::HashMap<String, String>>,
    axum::extract::State((_, _, uptime_tracker)): axum::extract::State<(
        Arc<tokio::sync::broadcast::Sender<jetstream_monitor::StreamStats>>,
        Arc<Storage>,
        Arc<std::sync::RwLock<UptimeTracker>>,
    )>,
) -> axum::Json<UptimeDetailedStats> {
    let hours: i64 = params
        .get("hours")
        .and_then(|h| h.parse().ok())
        .unwrap_or(24);

    let period_seconds = (hours * 3600) as u64;
    let detailed = uptime_tracker
        .read()
        .unwrap()
        .get_detailed_stats(period_seconds);

    axum::Json(detailed)
}
