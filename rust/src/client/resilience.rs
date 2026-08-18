use reqwest::{header::HeaderValue, StatusCode};
use serde::Serialize;
use sha2::{Digest, Sha256};
use std::fmt;
use std::time::Duration;

pub const MAX_BODY_EXCERPT_BYTES: usize = 512;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum BlueskyOperation {
    Profiles,
    Posts,
}

impl BlueskyOperation {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Profiles => "profiles",
            Self::Posts => "posts",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum UpstreamFailureCategory {
    Transport,
    RequestTimeout,
    RateLimited,
    ServerError,
    Authentication,
    Permission,
    PermanentResponse,
    Decode,
}

impl UpstreamFailureCategory {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Transport => "transport",
            Self::RequestTimeout => "request_timeout",
            Self::RateLimited => "rate_limited",
            Self::ServerError => "server_error",
            Self::Authentication => "authentication",
            Self::Permission => "permission",
            Self::PermanentResponse => "permanent_response",
            Self::Decode => "decode",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum IsolationOutcome {
    BroadOutage { category: UpstreamFailureCategory },
    SingletonPoison { request_fingerprint: String },
    BudgetExhausted,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UpstreamHttpError {
    pub operation: BlueskyOperation,
    pub status: Option<u16>,
    pub category: UpstreamFailureCategory,
    pub body_excerpt: Option<String>,
    pub attempts: u32,
    pub transient: bool,
    pub request_fingerprint: String,
    pub isolation: Option<IsolationOutcome>,
}

impl UpstreamHttpError {
    pub fn failure_fingerprint(&self) -> String {
        let status_class = self
            .status
            .map(|status| format!("{}xx", status / 100))
            .unwrap_or_else(|| "none".to_string());
        format!(
            "{}:{}:{}:{}",
            self.operation.as_str(),
            self.category.as_str(),
            status_class,
            self.request_fingerprint
        )
    }
}

impl fmt::Display for UpstreamHttpError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            formatter,
            "Bluesky {} request failed ({}, status={:?}, attempts={}, fingerprint={})",
            self.operation.as_str(),
            self.category.as_str(),
            self.status,
            self.attempts,
            self.request_fingerprint
        )
    }
}

impl std::error::Error for UpstreamHttpError {}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RequestRetryPolicy {
    /// Additional requests allowed after the initial attempt.
    pub max_retries: u32,
    pub base_delay: Duration,
    pub max_delay: Duration,
}

impl Default for RequestRetryPolicy {
    fn default() -> Self {
        Self {
            max_retries: 3,
            base_delay: Duration::from_millis(200),
            max_delay: Duration::from_secs(5),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ContainmentPolicy {
    pub min_delay: Duration,
    pub max_delay: Duration,
    pub persistence_threshold: u32,
    pub isolation_request_budget: u32,
}

impl Default for ContainmentPolicy {
    fn default() -> Self {
        Self {
            min_delay: Duration::from_secs(5),
            max_delay: Duration::from_secs(5 * 60),
            persistence_threshold: 3,
            isolation_request_budget: 8,
        }
    }
}

pub fn transient_category(status: StatusCode) -> Option<UpstreamFailureCategory> {
    match status {
        StatusCode::REQUEST_TIMEOUT => Some(UpstreamFailureCategory::RequestTimeout),
        StatusCode::TOO_MANY_REQUESTS => Some(UpstreamFailureCategory::RateLimited),
        status if status.is_server_error() => Some(UpstreamFailureCategory::ServerError),
        _ => None,
    }
}

pub fn stable_identifier_fingerprint(identifiers: &[String]) -> String {
    let mut sorted = identifiers.iter().map(String::as_str).collect::<Vec<_>>();
    sorted.sort_unstable();
    let mut digest = Sha256::new();
    for identifier in sorted {
        digest.update((identifier.len() as u64).to_be_bytes());
        digest.update(identifier.as_bytes());
    }
    let bytes = digest.finalize();
    bytes[..12]
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect()
}

pub fn bounded_body_excerpt(body: &str) -> String {
    if body.len() <= MAX_BODY_EXCERPT_BYTES {
        return body.to_string();
    }
    let mut end = MAX_BODY_EXCERPT_BYTES;
    while !body.is_char_boundary(end) {
        end -= 1;
    }
    format!("{}…", &body[..end])
}

pub fn retry_after_delta(value: Option<&HeaderValue>, maximum: Duration) -> Option<Duration> {
    let seconds = value?.to_str().ok()?.parse::<u64>().ok()?;
    Some(Duration::from_secs(seconds).min(maximum))
}

/// Returns exponential backoff with deterministic 75%-100% jitter, capped at `maximum`.
/// `entropy` lets callers use a fingerprint/clock-derived value and keeps tests deterministic.
pub fn bounded_exponential_jitter(
    base: Duration,
    maximum: Duration,
    retry_ordinal: u32,
    entropy: u64,
) -> Duration {
    let exponent = retry_ordinal.saturating_sub(1).min(31);
    let exponential = base.saturating_mul(1u32 << exponent).min(maximum);
    let millis = exponential.as_millis().min(u64::MAX as u128) as u64;
    let jitter_permille = 750 + (entropy % 251);
    Duration::from_millis((millis.saturating_mul(jitter_permille) / 1_000).max(1)).min(maximum)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn transient_statuses_are_classified() {
        assert_eq!(
            transient_category(StatusCode::REQUEST_TIMEOUT),
            Some(UpstreamFailureCategory::RequestTimeout)
        );
        assert_eq!(
            transient_category(StatusCode::TOO_MANY_REQUESTS),
            Some(UpstreamFailureCategory::RateLimited)
        );
        assert_eq!(
            transient_category(StatusCode::BAD_GATEWAY),
            Some(UpstreamFailureCategory::ServerError)
        );
        assert_eq!(transient_category(StatusCode::NOT_FOUND), None);
    }

    #[test]
    fn fingerprints_are_stable_and_order_independent() {
        let one = stable_identifier_fingerprint(&["b".into(), "a".into()]);
        let two = stable_identifier_fingerprint(&["a".into(), "b".into()]);
        assert_eq!(one, two);
        assert_eq!(one.len(), 24);
        assert_ne!(one, stable_identifier_fingerprint(&["a".into()]));
    }

    #[test]
    fn body_excerpt_is_utf8_safe_and_bounded() {
        let body = "é".repeat(MAX_BODY_EXCERPT_BYTES);
        let excerpt = bounded_body_excerpt(&body);
        assert!(excerpt.len() <= MAX_BODY_EXCERPT_BYTES + '…'.len_utf8());
        assert!(excerpt.ends_with('…'));
    }

    #[test]
    fn retry_after_accepts_only_delta_seconds_and_caps_it() {
        assert_eq!(
            retry_after_delta(
                Some(&HeaderValue::from_static("120")),
                Duration::from_secs(5)
            ),
            Some(Duration::from_secs(5))
        );
        assert_eq!(
            retry_after_delta(
                Some(&HeaderValue::from_static("tomorrow")),
                Duration::from_secs(5)
            ),
            None
        );
    }

    #[test]
    fn exponential_jitter_grows_and_caps() {
        let base = Duration::from_millis(100);
        let maximum = Duration::from_millis(250);
        assert_eq!(bounded_exponential_jitter(base, maximum, 1, 250), base);
        assert_eq!(
            bounded_exponential_jitter(base, maximum, 2, 250),
            Duration::from_millis(200)
        );
        assert_eq!(bounded_exponential_jitter(base, maximum, 20, 250), maximum);
    }
}
