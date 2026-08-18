use reqwest::{header::HeaderValue, StatusCode};
use serde::Serialize;
use sha2::{Digest, Sha256};
use std::fmt;
use std::time::Duration;

pub const MAX_DIAGNOSTIC_SUMMARY_BYTES: usize = 512;
const MAX_DIAGNOSTIC_INPUT_BYTES: usize = 4 * 1024;
const REDACTED_AT_URI: &str = "[redacted-at-uri]";
const REDACTED_AUTHORIZATION: &str = "[redacted-authorization]";
const REDACTED_TOKEN: &str = "[redacted-token]";

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
    pub diagnostic_summary: Option<String>,
    pub attempts: u32,
    pub retry_limit: u32,
    pub request_cardinality: usize,
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

/// Produces a privacy-safe diagnostic summary without exposing request identifiers or secrets.
pub fn sanitize_diagnostic_summary(input: &str) -> String {
    let bounded_input = truncate_utf8(input, MAX_DIAGNOSTIC_INPUT_BYTES);
    let normalized = bounded_input
        .chars()
        .map(|character| {
            if character.is_control() {
                ' '
            } else {
                character
            }
        })
        .collect::<String>();
    let mut sanitized = redact_prefixed_value(&normalized, "authorization", REDACTED_AUTHORIZATION);
    sanitized = redact_at_uris(&sanitized);
    sanitized = redact_token_shaped_content(&sanitized);
    truncate_with_marker(&sanitized, MAX_DIAGNOSTIC_SUMMARY_BYTES)
}

fn truncate_utf8(value: &str, maximum_bytes: usize) -> &str {
    if value.len() <= maximum_bytes {
        return value;
    }
    let mut end = maximum_bytes;
    while !value.is_char_boundary(end) {
        end -= 1;
    }
    &value[..end]
}

fn truncate_with_marker(value: &str, maximum_bytes: usize) -> String {
    if value.len() <= maximum_bytes {
        return value.to_string();
    }
    format!("{}…", truncate_utf8(value, maximum_bytes))
}

fn redact_prefixed_value(input: &str, prefix: &str, replacement: &str) -> String {
    let lowercase = input.to_ascii_lowercase();
    let mut output = String::with_capacity(input.len());
    let mut position = 0;
    while let Some(relative) = lowercase[position..].find(prefix) {
        let start = position + relative;
        output.push_str(&input[position..start]);
        let mut end = start + prefix.len();
        while input[end..].starts_with([' ', ':', '=']) {
            end += input[end..].chars().next().map_or(0, char::len_utf8);
        }
        if input[end..]
            .get(.."bearer".len())
            .is_some_and(|value| value.eq_ignore_ascii_case("bearer"))
        {
            end += "bearer".len();
            while input[end..].starts_with(' ') {
                end += 1;
            }
        }
        while let Some(character) = input[end..].chars().next() {
            if character.is_whitespace() || matches!(character, ',' | ';' | '}' | ']' | '"') {
                break;
            }
            end += character.len_utf8();
        }
        output.push_str(prefix);
        output.push_str(": ");
        output.push_str(replacement);
        position = end;
    }
    output.push_str(&input[position..]);
    output
}

fn redact_at_uris(input: &str) -> String {
    let mut output = String::with_capacity(input.len());
    let mut position = 0;
    while let Some(relative) = input[position..].find("at://") {
        let start = position + relative;
        output.push_str(&input[position..start]);
        let mut end = start + "at://".len();
        while let Some(character) = input[end..].chars().next() {
            if character.is_whitespace()
                || matches!(character, ',' | ';' | '}' | ']' | ')' | '"' | '\'')
            {
                break;
            }
            end += character.len_utf8();
        }
        output.push_str(REDACTED_AT_URI);
        position = end;
    }
    output.push_str(&input[position..]);
    output
}

fn redact_token_shaped_content(input: &str) -> String {
    input
        .split_inclusive(|character: char| {
            !(character.is_ascii_alphanumeric() || matches!(character, '.' | '_' | '-' | '~'))
        })
        .map(|part| {
            let token_end = part
                .find(|character: char| {
                    !(character.is_ascii_alphanumeric()
                        || matches!(character, '.' | '_' | '-' | '~'))
                })
                .unwrap_or(part.len());
            let (token, delimiter) = part.split_at(token_end);
            if is_token_shaped(token) {
                format!("{REDACTED_TOKEN}{delimiter}")
            } else {
                part.to_string()
            }
        })
        .collect()
}

fn is_token_shaped(value: &str) -> bool {
    let jwt_shaped = value.starts_with("eyJ") && value.matches('.').count() == 2;
    let long_opaque = value.len() >= 32
        && value.bytes().any(|byte| byte.is_ascii_alphabetic())
        && value.bytes().any(|byte| byte.is_ascii_digit());
    jwt_shaped || long_opaque
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
    fn diagnostic_summary_is_utf8_safe_and_bounded() {
        let body = "é".repeat(MAX_DIAGNOSTIC_SUMMARY_BYTES);
        let summary = sanitize_diagnostic_summary(&body);
        assert!(summary.len() <= MAX_DIAGNOSTIC_SUMMARY_BYTES + '…'.len_utf8());
        assert!(summary.ends_with('…'));
    }

    #[test]
    fn diagnostic_summary_redacts_sensitive_values_and_normalizes_controls() {
        let summary = sanitize_diagnostic_summary(
            "authorization: Bearer secret-token-123456789012345678901234\nuri=at://did:plc:raw/app.bsky.feed.post/raw\u{0} jwt=eyJhbGciOiJIUzI1NiJ9.payload.signature",
        );
        assert!(!summary.contains("secret-token"));
        assert!(!summary.contains("did:plc:raw"));
        assert!(!summary.contains("eyJhbGci"));
        assert!(!summary.contains('\n'));
        assert!(!summary.contains('\u{0}'));
        assert!(summary.contains(REDACTED_AUTHORIZATION));
        assert!(summary.contains(REDACTED_AT_URI));
        assert!(summary.contains(REDACTED_TOKEN));
    }

    #[test]
    fn diagnostic_summary_handles_oversized_malformed_non_json_content() {
        let body = format!(
            "not-json {{ broken {}",
            "x".repeat(MAX_DIAGNOSTIC_INPUT_BYTES * 2)
        );
        let summary = sanitize_diagnostic_summary(&body);
        assert!(summary.starts_with("not-json { broken"));
        assert!(summary.len() <= MAX_DIAGNOSTIC_SUMMARY_BYTES + '…'.len_utf8());
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
