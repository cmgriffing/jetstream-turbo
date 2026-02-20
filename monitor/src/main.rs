use anyhow::Result;
use jetstream_monitor::{
    config::Settings,
    stats::{StatsAggregator, StreamStatsInternal},
    storage::Storage,
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
    let aggregator = StatsAggregator::new(
        settings.stream_a_name.clone(),
        settings.stream_b_name.clone(),
    );
    let broadcast_tx = Arc::new(aggregator.sender());

    let client_a = StreamClient::new(settings.stream_a_url.clone(), StreamId::A);
    let client_b = StreamClient::new(settings.stream_b_url.clone(), StreamId::B);

    let stats_for_stream = Arc::clone(&stats_internal);
    tokio::spawn(async move {
        use futures::StreamExt;
        let mut stream_a = client_a.stream_counts();
        let mut stream_b = client_b.stream_counts();

        loop {
            tokio::select! {
                Some(msg) = stream_a.next() => {
                    stats_for_stream.write().unwrap().update(msg);
                }
                Some(msg) = stream_b.next() => {
                    stats_for_stream.write().unwrap().update(msg);
                }
                else => break,
            }
        }
    });

    aggregator.process(&stats_internal);

    let stats_for_storage = Arc::clone(&stats_internal);
    let storage_arc = Arc::new(storage);
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
        .with_state(broadcast_tx)
        .layer(tower_http::trace::TraceLayer::new_for_http());

    let listener = tokio::net::TcpListener::bind(&settings.bind_address).await?;
    tracing::info!("Listening on {}", settings.bind_address);

    axum::serve(listener, app).await?;

    Ok(())
}
