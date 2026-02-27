use anyhow::Result;
use jetstream_monitor::{
    config::Settings,
    stats::{StatsAggregator, StreamStatsInternal, UptimeTracker},
    storage::{HourlyStat, Storage},
    stream::{StreamClient, StreamId},
    websocket,
};
use std::sync::Arc;

const INDEX_HTML: &str = include_str!("../static/index.html");

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

    let stats_internal = Arc::new(std::sync::RwLock::new(StreamStatsInternal::default()));
    let uptime_tracker = Arc::new(std::sync::RwLock::new(UptimeTracker::default()));
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
                }
                Some(msg) = stream_b.next() => {
                    stats_for_stream.write().unwrap().update(msg);
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
    tokio::spawn(async move {
        let mut interval = tokio::time::interval(std::time::Duration::from_secs(3600));
        let mut last_hour = chrono::Utc::now().format("%Y-%m-%d %H").to_string();

        loop {
            interval.tick().await;
            let current_hour = chrono::Utc::now().format("%Y-%m-%d %H").to_string();

            if current_hour != last_hour {
                let (count_a, count_b) = {
                    let internal = stats_for_storage.read().unwrap();
                    (internal.count_a, internal.count_b)
                };
                if let Err(e) = storage_arc
                    .save_hourly(chrono::Utc::now(), count_a, count_b)
                    .await
                {
                    tracing::error!("Failed to save hourly stats: {}", e);
                }

                let (uptime_a, uptime_b) = {
                    let up = uptime_for_storage.read().unwrap();
                    up.get_current_uptime_seconds()
                };
                if let Err(e) = storage_arc
                    .save_hourly_uptime(chrono::Utc::now(), uptime_a, uptime_b)
                    .await
                {
                    tracing::error!("Failed to save hourly uptime: {}", e);
                }

                last_hour = current_hour;
            }
        }
    });

    let app = axum::Router::new()
        .route(
            "/",
            axum::routing::get(|| async { axum::response::Html(INDEX_HTML.to_string()) }),
        )
        .route("/ws", axum::routing::get(websocket::ws_handler))
        .route("/api/history", axum::routing::get(get_history))
        .route("/api/uptime", axum::routing::get(get_uptime))
        .with_state((broadcast_tx, storage_for_api))
        .layer(tower_http::trace::TraceLayer::new_for_http());

    let listener = tokio::net::TcpListener::bind(&settings.bind_address).await?;
    tracing::info!("Listening on {}", settings.bind_address);

    axum::serve(listener, app).await?;

    Ok(())
}

async fn get_history(
    axum::extract::Query(params): axum::extract::Query<std::collections::HashMap<String, String>>,
    axum::extract::State((_, storage)): axum::extract::State<(Arc<tokio::sync::broadcast::Sender<jetstream_monitor::StreamStats>>, Arc<Storage>)>,
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
    axum::extract::State((_, storage)): axum::extract::State<(Arc<tokio::sync::broadcast::Sender<jetstream_monitor::StreamStats>>, Arc<Storage>)>,
) -> axum::Json<Vec<jetstream_monitor::storage::HourlyUptime>> {
    let hours: i64 = params
        .get("hours")
        .and_then(|h| h.parse().ok())
        .unwrap_or(24);

    let since = chrono::Utc::now() - chrono::Duration::hours(hours);

    match storage.get_uptime_since(since).await {
        Ok(uptime) => axum::Json(uptime),
        Err(_) => axum::Json(vec![]),
    }
}
