use thiserror::Error;

pub type TurboResult<T> = Result<T, TurboError>;

#[derive(Debug, Error)]
pub enum TurboError {
    // Connection errors
    #[error("Jetstream connection failed: {0}")]
    JetstreamConnection(#[from] tokio_tungstenite::tungstenite::Error),

    #[error("WebSocket connection failed: {0}")]
    WebSocketConnection(String),

    // HTTP/ API errors
    #[error("HTTP request failed: {0}")]
    HttpRequest(#[from] reqwest::Error),

    #[error("API rate limit exceeded")]
    RateLimitExceeded,

    #[error("Invalid response from API: {0}")]
    InvalidApiResponse(String),

    // Configuration errors
    #[error("Configuration error: {0}")]
    Configuration(#[from] config::ConfigError),

    #[error("Environment variable missing: {0}")]
    MissingEnvVar(String),

    // Storage errors
    #[error("SQLite database error: {0}")]
    Database(#[from] sqlx::Error),

    #[error("S3 operation failed: {0}")]
    S3Operation(
        #[from]
        aws_sdk_s3::error::SdkError<
            aws_sdk_s3::operation::put_object::PutObjectError,
            aws_sdk_http::endpoint::Resolver,
        >,
    ),

    #[error("Redis operation failed: {0}")]
    RedisOperation(#[from] redis::RedisError),

    // Serialization errors
    #[error("JSON serialization failed: {0}")]
    JsonSerialization(#[from] serde_json::Error),

    #[error("JSON deserialization failed: {0}")]
    JsonDeserialization(#[from] simd_json::Error),

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
    Io(#[from] std::io::Error),

    #[error("Task join error: {0}")]
    TaskJoin(#[from] tokio::task::JoinError),

    #[error("Elapsed timeout")]
    Timeout(#[from] tokio::time::error::Elapsed),

    // Generic errors
    #[error("Internal error: {0}")]
    Internal(String),

    #[error("Not found: {0}")]
    NotFound(String),

    #[error("Permission denied: {0}")]
    PermissionDenied(String),
}

impl TurboError {
    pub fn is_retryable(&self) -> bool {
        matches!(
            self,
            TurboError::HttpRequest(_)
                | TurboError::RateLimitExceeded
                | TurboError::Database(_)
                | TurboError::S3Operation(_)
                | TurboError::RedisOperation(_)
                | TurboError::WebSocketConnection(_)
                | TurboError::Timeout(_)
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
}
