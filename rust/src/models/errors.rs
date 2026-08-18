use thiserror::Error;

pub type TurboResult<T> = Result<T, TurboError>;

#[derive(Debug, Error)]
pub enum TurboError {
    // Connection errors
    #[error("Jetstream connection failed: {0}")]
    JetstreamConnection(#[source] Box<tokio_tungstenite::tungstenite::Error>),

    #[error("WebSocket connection failed: {0}")]
    WebSocketConnection(String),

    // HTTP/ API errors
    #[error("HTTP request failed: {0}")]
    HttpRequest(#[source] Box<reqwest::Error>),

    #[error("API rate limit exceeded")]
    RateLimitExceeded,

    #[error("Invalid response from API: {0}")]
    InvalidApiResponse(String),

    // Configuration errors
    #[error("Configuration error: {0}")]
    Configuration(#[source] Box<config::ConfigError>),

    #[error("Environment variable missing: {0}")]
    MissingEnvVar(String),

    // Storage errors
    #[error("SQLite database error: {0}")]
    Database(#[source] Box<sqlx::Error>),

    #[error("Redis operation failed: {0}")]
    RedisOperation(#[source] Box<not_redis::RedisError>),

    // Serialization errors
    #[error("JSON serialization failed: {0}")]
    JsonSerialization(#[source] Box<serde_json::Error>),

    #[error("JSON deserialization failed: {0}")]
    JsonDeserialization(#[source] Box<simd_json::Error>),

    // Cache errors
    #[error("Cache operation failed: {0}")]
    CacheOperation(String),

    // Business logic errors
    #[error("Invalid message format: {0}")]
    InvalidMessage(String),

    #[error("Hydration failed: {0}")]
    HydrationFailed(String),

    #[error("Storage rotation failed: {0}")]
    RotationFailed(String),

    // System errors
    #[error("IO error: {0}")]
    Io(#[source] Box<std::io::Error>),

    #[error("Task join error: {0}")]
    TaskJoin(#[source] Box<tokio::task::JoinError>),

    #[error("Elapsed timeout")]
    Timeout(#[from] tokio::time::error::Elapsed),

    #[error("Batch {batch_id} timed out in {stage} after {timeout_secs}s")]
    BatchStageTimeout {
        batch_id: u64,
        stage: String,
        timeout_secs: u64,
    },

    // Generic errors
    #[error("Internal error: {0}")]
    Internal(String),

    #[error("Not found: {0}")]
    NotFound(String),

    #[error("Permission denied: {0}")]
    PermissionDenied(String),

    #[error("Session expired: {0}")]
    ExpiredToken(String),
}

macro_rules! impl_boxed_from {
    ($variant:ident, $source:ty) => {
        impl From<$source> for TurboError {
            fn from(source: $source) -> Self {
                TurboError::$variant(Box::new(source))
            }
        }
    };
}

impl_boxed_from!(JetstreamConnection, tokio_tungstenite::tungstenite::Error);
impl_boxed_from!(HttpRequest, reqwest::Error);
impl_boxed_from!(Configuration, config::ConfigError);
impl_boxed_from!(Database, sqlx::Error);
impl_boxed_from!(RedisOperation, not_redis::RedisError);
impl_boxed_from!(JsonSerialization, serde_json::Error);
impl_boxed_from!(JsonDeserialization, simd_json::Error);
impl_boxed_from!(Io, std::io::Error);
impl_boxed_from!(TaskJoin, tokio::task::JoinError);

impl TurboError {
    pub fn is_retryable(&self) -> bool {
        matches!(
            self,
            TurboError::HttpRequest(_)
                | TurboError::RateLimitExceeded
                | TurboError::Database(_)
                | TurboError::RedisOperation(_)
                | TurboError::WebSocketConnection(_)
                | TurboError::Timeout(_)
                | TurboError::BatchStageTimeout { .. }
                | TurboError::ExpiredToken(_)
        )
    }

    pub fn is_critical(&self) -> bool {
        matches!(
            self,
            TurboError::Configuration(_)
                | TurboError::MissingEnvVar(_)
                | TurboError::PermissionDenied(_)
        )
    }

    pub fn is_expired_token(&self) -> bool {
        matches!(self, TurboError::ExpiredToken(_))
    }
}
