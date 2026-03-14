use crate::models::errors::{TurboError, TurboResult};
use crate::turbocharger::{HealthStatus, TurboCharger, TurboStats};
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
use tokio::sync::broadcast;
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
        .route("/ws", get(ws_handler))
        .with_state(turbocharger)
}

async fn health_check(
    State(turbocharger): State<Arc<TurboCharger>>,
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

async fn ws_handler(
    State(turbocharger): State<Arc<TurboCharger>>,
    ws: WebSocketUpgrade,
) -> axum::response::Response {
    ws.on_upgrade(move |socket| handle_websocket(socket, turbocharger.subscribe()))
}

async fn handle_websocket(
    socket: WebSocket,
    mut broadcast_rx: broadcast::Receiver<crate::models::enriched::EnrichedRecord>,
) {
    let (mut sender, mut socket_rx) = socket.split();

    loop {
        tokio::select! {
            msg = broadcast_rx.recv() => {
                match msg {
                    Ok(record) => {
                        if let Ok(json) = serde_json::to_string(&record) {
                            if sender.send(Message::Text(json)).await.is_err() {
                                break;
                            }
                        }
                    }
                    Err(broadcast::error::RecvError::Lagged(_)) => continue,
                    Err(broadcast::error::RecvError::Closed) => break,
                }
            }
            msg = socket_rx.next() => {
                match msg {
                    Some(Ok(Message::Close(_))) | None => break,
                    _ => {}
                }
            }
        }
    }
}

pub async fn create_server(port: u16, turbocharger: Arc<TurboCharger>) -> TurboResult<()> {
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
        .map_err(TurboError::Io)?;

    info!("Starting HTTP server on port {}", port);

    axum::serve(listener, app)
        .await
        .map_err(|e| TurboError::Io(std::io::Error::other(e)))?;

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
    let status_code = readiness_http_status(&status);
    let response_status = if status.healthy { "healthy" } else { "unhealthy" };

    (
        status_code,
        HealthResponse {
            status: response_status.to_string(),
            data: status,
        },
    )
}

#[cfg(test)]
mod tests {
    use super::{health_http_response, readiness_http_status};
    use crate::turbocharger::{
        CacheStateDiagnostics, HealthDiagnostics, HealthStatus, NotRedisStateDiagnostics,
        ProcessMemoryDiagnostics, SQLiteStateDiagnostics,
    };
    use axum::http::StatusCode;
    use serde_json::Value;

    fn sample_diagnostics() -> HealthDiagnostics {
        HealthDiagnostics {
            process_memory: ProcessMemoryDiagnostics {
                pid: 42,
                rss_bytes: Some(1024),
                virtual_memory_bytes: Some(4096),
                source: "test",
                collection_error: None,
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
        }
    }

    fn sample_health(healthy: bool) -> HealthStatus {
        HealthStatus {
            healthy,
            redis_connected: healthy,
            sqlite_available: healthy,
            session_count: if healthy { 1 } else { 0 },
            diagnostics: sample_diagnostics(),
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
    fn health_http_response_is_unhealthy_and_503_for_unhealthy_status() {
        let (status_code, response) = health_http_response(sample_health(false));
        assert_eq!(status_code, StatusCode::SERVICE_UNAVAILABLE);
        assert_eq!(response.status, "unhealthy");
        assert!(!response.data.healthy);
    }

    #[test]
    fn health_response_serializes_diagnostics_snapshot() {
        let (_status_code, response) = health_http_response(sample_health(true));
        let json: Value = serde_json::to_value(response).expect("health response should serialize");

        assert_eq!(json["status"], "healthy");
        assert!(json["data"]["diagnostics"]["process_memory"]["pid"].is_number());
        assert!(json["data"]["diagnostics"]["cache_state"]["user_capacity"].is_number());
        assert!(json["data"]["diagnostics"]["sqlite_state"]["journal_mode"].is_string());
        assert!(json["data"]["diagnostics"]["not_redis_state"]["stream_name"].is_string());
    }
}
