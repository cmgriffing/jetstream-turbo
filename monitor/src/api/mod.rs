//! Versioned operational API: shared models, handlers, and OpenAPI contract.

pub mod health;

pub use health::{HealthSnapshot, HealthStatus, StorageHealth, StreamHealth};

/// Stable machine-readable error shape for all `/api/v1` failures.
#[derive(Debug, Clone, serde::Serialize, utoipa::ToSchema)]
pub struct ApiError {
    /// Bounded machine-readable error code.
    pub code: ApiErrorCode,
    /// Stable human-readable message describing the failure.
    pub message: String,
    /// API semantic version reported with every error.
    pub api_version: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, utoipa::ToSchema)]
#[serde(rename_all = "snake_case")]
pub enum ApiErrorCode {
    InvalidRequest,
    NotFound,
    StorageUnavailable,
    InternalError,
}

impl ApiError {
    pub fn invalid_request(message: impl Into<String>) -> Self {
        Self {
            code: ApiErrorCode::InvalidRequest,
            message: message.into(),
            api_version: API_VERSION.to_string(),
        }
    }

    pub fn not_found() -> Self {
        Self {
            code: ApiErrorCode::NotFound,
            message: "the requested resource does not exist or has been removed".to_string(),
            api_version: API_VERSION.to_string(),
        }
    }

    pub fn storage_unavailable() -> Self {
        Self {
            code: ApiErrorCode::StorageUnavailable,
            message: "required storage is currently unavailable".to_string(),
            api_version: API_VERSION.to_string(),
        }
    }
}

/// Envelope shared by every `/api/v1` success response.
#[derive(Debug, Clone, serde::Serialize, utoipa::ToSchema)]
pub struct ApiResponse<T> {
    pub data: T,
}

/// API semantic version, independent of the binary release identity.
pub const API_VERSION: &str = "v1";

/// Standard no-store cache-control header value for operational responses.
pub const NO_STORE: &str = "no-store";
