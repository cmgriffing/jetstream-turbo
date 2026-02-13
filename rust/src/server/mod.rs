use crate::models::errors::{TurboError, TurboResult};
use crate::turbocharger::{HealthStatus, TurboCharger, TurboStats};
use axum::{
    extract::{Query, State},
    http::StatusCode,
    response::Json,
    routing::{get, Router},
};
use serde::{Deserialize, Serialize};
use std::sync::Arc;
use tracing::info;

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

pub fn create_router(turbocharger: Arc<TurboCharger>) -> Router {
    Router::new()
        .route("/health", get(health_check))
        .route("/stats", get(get_stats))
        .route("/metrics", get(get_metrics))
        .with_state(turbocharger)
}

async fn health_check(
    State(turbocharger): State<Arc<TurboCharger>>,
) -> Result<Json<HealthResponse>, StatusCode> {
    match turbocharger.health_check().await {
        Ok(status) => Ok(Json(HealthResponse {
            status: if status.healthy {
                "healthy"
            } else {
                "unhealthy"
            }
            .to_string(),
            data: status,
        })),
        Err(_) => Err(StatusCode::INTERNAL_SERVER_ERROR),
    }
}

async fn get_stats(
    State(turbocharger): State<Arc<TurboCharger>>,
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

async fn get_metrics() -> &'static str {
    // This would return Prometheus metrics in a real implementation
    "# HELP jetstream_turbo_messages_total Total number of messages processed\n\
    # TYPE jetstream_turbo_messages_total counter\n\
    jetstream_turbo_messages_total 0\n\
    # HELP jetstream_turbo_cache_hit_rate Cache hit rate\n\
    # TYPE jetstream_turbo_cache_hit_rate gauge\n\
    jetstream_turbo_cache_hit_rate 0.0\n"
}

pub async fn create_server(port: u16, turbocharger: Arc<TurboCharger>) -> TurboResult<()> {
    let app = Router::new()
        .nest("/api/v1", create_router(turbocharger))
        .route("/", get(|| async { "jetstream-turbo API server" }))
        .route("/ready", get(|| async { "OK" }));

    let listener = tokio::net::TcpListener::bind(format!("0.0.0.0:{port}"))
        .await
        .map_err(TurboError::Io)?;

    info!("Starting HTTP server on port {}", port);

    axum::serve(listener, app)
        .await
        .map_err(|e| TurboError::Io(std::io::Error::other(e)))?;

    Ok(())
}
