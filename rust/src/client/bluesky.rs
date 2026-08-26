use crate::client::coordination::{BoundedCoordinator, CoordinationLimits, CoordinationSnapshot};
use crate::client::resilience::{
    bounded_exponential_jitter, retry_after_delta, sanitize_diagnostic_summary,
    stable_identifier_fingerprint, transient_category, BlueskyOperation, ContainmentPolicy,
    RequestRetryPolicy, UpstreamFailureCategory, UpstreamHttpError,
};
use crate::client::BlueskyAuthClient;
use crate::models::{
    bluesky::{BlueskyPost, BlueskyProfile, GetPostsBulkResponse, GetProfilesResponse},
    errors::{TurboError, TurboResult},
};
use crate::utils::serde_utils::string_utils::is_valid_at_uri;
use governor::{Quota, RateLimiter};
use reqwest::{Client, StatusCode};
use std::collections::{HashMap, VecDeque};
use std::num::NonZeroU32;
use std::sync::atomic::{AtomicU32, AtomicU64, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::sync::RwLock;
use tracing::{error, info, instrument, warn};

pub trait ProfileFetcher {
    fn bulk_fetch_profiles(
        &self,
        dids: &[String],
    ) -> impl std::future::Future<Output = TurboResult<Vec<Option<Arc<BlueskyProfile>>>>> + Send;
}

pub trait PostFetcher {
    fn bulk_fetch_posts(
        &self,
        uris: &[String],
    ) -> impl std::future::Future<Output = TurboResult<Vec<PostFetchOutcome>>> + Send;
}

#[derive(Debug, Clone)]
pub enum PostFetchOutcome {
    Found(Arc<BlueskyPost>),
    Missing,
    TemporarilyUnavailable(crate::client::HydrationFailure),
}

const REQUESTS_PER_SECOND_MS: u64 = 1000 / 10;

pub const DEFAULT_PROFILE_COORDINATION_KEY_CAPACITY: usize = 150;
pub const DEFAULT_PROFILE_COORDINATION_WAITER_CAPACITY: usize = 600;
pub const DEFAULT_POST_COORDINATION_KEY_CAPACITY: usize = 150;
pub const DEFAULT_POST_COORDINATION_WAITER_CAPACITY: usize = 600;

pub struct BlueskyClient {
    session_strings: Arc<RwLock<Vec<String>>>,
    refresh_jwt: Arc<RwLock<Option<String>>>,
    expires_at: Arc<RwLock<Option<String>>>,
    auth_client: Option<Arc<BlueskyAuthClient>>,
    isolation_recurrence: Arc<AtomicU32>,
    profile_batch_collector: Arc<RwLock<ProfileBatchCollector>>,
    post_batch_collector: Arc<RwLock<PostBatchCollector>>,
}

#[derive(Clone)]
struct BatchConfig {
    batch_size: usize,
    wait_ms: u64,
}

#[derive(Clone)]
struct BatchCollectorDeps {
    http_client: Client,
    session_strings: Arc<RwLock<Vec<String>>>,
    rate_limiter: Arc<
        RateLimiter<
            governor::state::NotKeyed,
            governor::state::InMemoryState,
            governor::clock::DefaultClock,
        >,
    >,
    api_base_url: String,
    retry_policy: RequestRetryPolicy,
    containment_policy: ContainmentPolicy,
    isolation_recurrence: Arc<AtomicU32>,
    auth_client: Option<Arc<BlueskyAuthClient>>,
    refresh_jwt: Arc<RwLock<Option<String>>>,
    expires_at: Arc<RwLock<Option<String>>>,
}

#[derive(Debug, Clone)]
enum SharedFetchError {
    Upstream(UpstreamHttpError),
    RateLimited,
    InvalidApiResponse(String),
    PermissionDenied(String),
    ExpiredToken(String),
    Internal(String),
}

impl SharedFetchError {
    fn from_error(error: &TurboError) -> Self {
        match error {
            TurboError::BlueskyUpstream(error) => Self::Upstream(error.clone()),
            TurboError::RateLimitExceeded => Self::RateLimited,
            TurboError::InvalidApiResponse(message) => Self::InvalidApiResponse(message.clone()),
            TurboError::PermissionDenied(message) => Self::PermissionDenied(message.clone()),
            TurboError::ExpiredToken(message) => Self::ExpiredToken(message.clone()),
            error => Self::Internal(error.to_string()),
        }
    }

    fn into_error(self) -> TurboError {
        match self {
            Self::Upstream(error) => error.into(),
            Self::RateLimited => TurboError::RateLimitExceeded,
            Self::InvalidApiResponse(message) => TurboError::InvalidApiResponse(message),
            Self::PermissionDenied(message) => TurboError::PermissionDenied(message),
            Self::ExpiredToken(message) => TurboError::ExpiredToken(message),
            Self::Internal(message) => TurboError::Internal(message),
        }
    }

    fn claimant_cancelled(operation: BlueskyOperation) -> Self {
        Self::Upstream(UpstreamHttpError {
            operation,
            status: None,
            category: UpstreamFailureCategory::Transport,
            diagnostic_summary: Some("coordination claimant cancelled".to_string()),
            attempts: 0,
            retry_limit: 0,
            request_cardinality: 0,
            transient: true,
            request_fingerprint: "claimant-cancelled".to_string(),
            isolation: None,
        })
    }
}

/// Monotonic fetch-path counters for one collector kind. Rates (requests/sec,
/// items per request, average latencies) are derived by differencing over time.
#[derive(Debug, Default)]
pub struct FetchDiagnostics {
    /// Identifiers submitted across all requests (items per request = Δitems / Δrequests).
    pub items_total: AtomicU64,
    /// Total wall time spent waiting for the collector lock plus holding it
    /// during fetch resolution, in nanoseconds. One sample per `add_and_fetch`.
    pub lock_duration_ns_total: AtomicU64,
    /// Number of lock-hold samples (one per `add_and_fetch` call).
    pub lock_duration_count: AtomicU64,
    /// Total wall time spent in the HTTP fetch chain (incl. retries and
    /// isolation bisection), in nanoseconds. One sample per `fetch_batch_with_retry`.
    pub http_duration_ns_total: AtomicU64,
    /// Number of HTTP chain samples.
    pub http_duration_count: AtomicU64,
    /// Total wall time spent waiting on the shared upstream rate limiter
    /// before each attempt, in nanoseconds. One sample per attempt.
    pub rate_limiter_wait_ns_total: AtomicU64,
    /// Number of rate-limiter wait samples (one per attempt).
    pub rate_limiter_wait_count: AtomicU64,
    /// Total wall time spent decoding responses and assembling results locally
    /// after a successful response, in nanoseconds. One sample per request.
    pub assembly_duration_ns_total: AtomicU64,
    /// Number of local-assembly samples.
    pub assembly_duration_count: AtomicU64,
    /// Requests exhausted with a 429 rate-limit response.
    pub errors_rate_limited: AtomicU64,
    /// Requests exhausted with a 5xx / timeout / transport failure.
    pub errors_upstream: AtomicU64,
}

/// Bounded hydration-substage identifiers used by latency telemetry. Label
/// values are limited to this fixed enum.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, serde::Serialize)]
#[serde(rename_all = "snake_case")]
pub enum HydrationSubstage {
    CacheLookup,
    RateLimiterWait,
    UpstreamHttp,
    LocalAssembly,
}

/// Aggregate latency snapshot for one hydration substage. A zero sample count
/// is the explicit "unavailable" state (the substage was never exercised).
#[derive(Debug, Clone, Copy, serde::Serialize)]
pub struct HydrationSubstageTimingSnapshot {
    pub substage: HydrationSubstage,
    pub sample_count: u64,
    pub duration_ns_total: u64,
}

/// Broad class of a fetch-chain failure, used to discriminate 429s from
/// upstream 5xx/timeout failures in diagnostics.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum FetchErrorClass {
    RateLimited,
    Upstream,
}

fn classify_fetch_error(error: &TurboError) -> Option<FetchErrorClass> {
    match error {
        TurboError::RateLimitExceeded => Some(FetchErrorClass::RateLimited),
        TurboError::BlueskyUpstream(upstream) => match upstream.category {
            UpstreamFailureCategory::RateLimited => Some(FetchErrorClass::RateLimited),
            UpstreamFailureCategory::ServerError
            | UpstreamFailureCategory::RequestTimeout
            | UpstreamFailureCategory::Transport => Some(FetchErrorClass::Upstream),
            _ => None,
        },
        TurboError::HttpRequest(_) | TurboError::Timeout(_) => Some(FetchErrorClass::Upstream),
        _ => None,
    }
}

/// A point-in-time snapshot of the fetch counters for one collector kind.
#[derive(Debug, Clone, Copy, Default, serde::Serialize)]
#[serde(rename_all = "snake_case")]
pub struct BlueskyFetchKindDiagnostics {
    pub requests_total: u64,
    pub items_total: u64,
    pub lock_duration_ns_total: u64,
    pub lock_duration_count: u64,
    pub http_duration_ns_total: u64,
    pub http_duration_count: u64,
    pub rate_limiter_wait_ns_total: u64,
    pub rate_limiter_wait_count: u64,
    pub assembly_duration_ns_total: u64,
    pub assembly_duration_count: u64,
    pub errors_rate_limited: u64,
    pub errors_upstream: u64,
}

impl BlueskyFetchKindDiagnostics {
    /// Bounded per-substage latency aggregates for this collector kind.
    pub fn substage_timings(&self) -> [HydrationSubstageTimingSnapshot; 4] {
        [
            HydrationSubstageTimingSnapshot {
                substage: HydrationSubstage::CacheLookup,
                sample_count: self.lock_duration_count,
                duration_ns_total: self.lock_duration_ns_total,
            },
            HydrationSubstageTimingSnapshot {
                substage: HydrationSubstage::RateLimiterWait,
                sample_count: self.rate_limiter_wait_count,
                duration_ns_total: self.rate_limiter_wait_ns_total,
            },
            HydrationSubstageTimingSnapshot {
                substage: HydrationSubstage::UpstreamHttp,
                sample_count: self.http_duration_count,
                duration_ns_total: self.http_duration_ns_total,
            },
            HydrationSubstageTimingSnapshot {
                substage: HydrationSubstage::LocalAssembly,
                sample_count: self.assembly_duration_count,
                duration_ns_total: self.assembly_duration_ns_total,
            },
        ]
    }
}

/// Fetch-path diagnostics for both collector kinds, surfaced on /health and /metrics.
#[derive(Debug, Clone, Copy, Default, serde::Serialize)]
#[serde(rename_all = "snake_case")]
pub struct BlueskyFetchDiagnostics {
    pub profiles: BlueskyFetchKindDiagnostics,
    pub posts: BlueskyFetchKindDiagnostics,
}

#[derive(Debug, Clone, Copy, Default, serde::Serialize)]
#[serde(rename_all = "snake_case")]
pub struct BlueskyCoordinationDiagnostics {
    pub profiles: CoordinationSnapshot,
    pub posts: CoordinationSnapshot,
}

#[cfg(test)]
#[derive(Debug, Clone, Copy, Default)]
struct CollectorOwnershipSnapshot {
    pending_keys: usize,
    in_flight_keys: usize,
    waiters: usize,
    retained_identifier_bytes: usize,
    completed_result_owners: usize,
}

#[cfg(test)]
#[derive(Debug, Clone, Copy, Default)]
struct BlueskyCollectorOwnershipSnapshot {
    profiles: CollectorOwnershipSnapshot,
    posts: CollectorOwnershipSnapshot,
}

struct ProfileBatchCollector {
    config: BatchConfig,
    http_client: Client,
    session_strings: Arc<RwLock<Vec<String>>>,
    rate_limiter: Arc<
        RateLimiter<
            governor::state::NotKeyed,
            governor::state::InMemoryState,
            governor::clock::DefaultClock,
        >,
    >,
    api_base_url: String,
    retry_policy: RequestRetryPolicy,
    containment_policy: ContainmentPolicy,
    isolation_recurrence: Arc<AtomicU32>,
    auth_client: Option<Arc<BlueskyAuthClient>>,
    refresh_jwt: Arc<RwLock<Option<String>>>,
    expires_at: Arc<RwLock<Option<String>>>,
    coordination: Arc<BoundedCoordinator<Option<Arc<BlueskyProfile>>, SharedFetchError>>,
    batches_total: AtomicU64,
    batches_partial: AtomicU64,
    fetch: FetchDiagnostics,
}

struct PostBatchCollector {
    config: BatchConfig,
    http_client: Client,
    session_strings: Arc<RwLock<Vec<String>>>,
    rate_limiter: Arc<
        RateLimiter<
            governor::state::NotKeyed,
            governor::state::InMemoryState,
            governor::clock::DefaultClock,
        >,
    >,
    api_base_url: String,
    retry_policy: RequestRetryPolicy,
    containment_policy: ContainmentPolicy,
    isolation_recurrence: Arc<AtomicU32>,
    auth_client: Option<Arc<BlueskyAuthClient>>,
    refresh_jwt: Arc<RwLock<Option<String>>>,
    expires_at: Arc<RwLock<Option<String>>>,
    coordination: Arc<BoundedCoordinator<Option<Arc<BlueskyPost>>, SharedFetchError>>,
    batches_total: AtomicU64,
    batches_partial: AtomicU64,
    fetch: FetchDiagnostics,
}

fn retry_entropy(fingerprint: &str, attempt: u32) -> u64 {
    fingerprint.bytes().fold(attempt as u64, |state, byte| {
        state.wrapping_mul(1099511628211) ^ byte as u64
    })
}

fn retry_delay(
    response: Option<&reqwest::Response>,
    status: Option<StatusCode>,
    retry_ordinal: u32,
    policy: RequestRetryPolicy,
    fingerprint: &str,
) -> Duration {
    if matches!(
        status,
        Some(StatusCode::TOO_MANY_REQUESTS | StatusCode::SERVICE_UNAVAILABLE)
    ) {
        if let Some(delay) = retry_after_delta(
            response.and_then(|response| response.headers().get("retry-after")),
            policy.max_delay,
        ) {
            return delay;
        }
    }
    bounded_exponential_jitter(
        policy.base_delay,
        policy.max_delay,
        retry_ordinal,
        retry_entropy(fingerprint, retry_ordinal),
    )
}

fn upstream_error(
    operation: BlueskyOperation,
    identifiers: &[String],
    status: Option<StatusCode>,
    category: UpstreamFailureCategory,
    body: Option<&str>,
    attempts: u32,
    retry_limit: u32,
) -> TurboError {
    UpstreamHttpError {
        operation,
        status: status.map(|status| status.as_u16()),
        category,
        diagnostic_summary: body.map(sanitize_diagnostic_summary),
        attempts,
        retry_limit,
        request_cardinality: identifiers.len(),
        transient: matches!(
            category,
            UpstreamFailureCategory::Transport
                | UpstreamFailureCategory::RequestTimeout
                | UpstreamFailureCategory::RateLimited
                | UpstreamFailureCategory::ServerError
        ),
        request_fingerprint: stable_identifier_fingerprint(identifiers),
        isolation: None,
    }
    .into()
}

fn record_request_retry(
    operation: BlueskyOperation,
    category: UpstreamFailureCategory,
    retry_ordinal: u32,
    policy: RequestRetryPolicy,
    delay: Duration,
) {
    metrics::counter!(
        "bluesky_request_retries_total",
        "operation" => operation.as_str(),
        "category" => category.as_str(),
        "retry_ordinal" => retry_ordinal.to_string(),
        "retry_limit" => policy.max_retries.to_string(),
    )
    .increment(1);
    metrics::histogram!(
        "bluesky_request_retry_delay_seconds",
        "operation" => operation.as_str(),
        "category" => category.as_str(),
    )
    .record(delay.as_secs_f64());
    warn!(
        operation = operation.as_str(),
        category = category.as_str(),
        retry_ordinal,
        retry_limit = policy.max_retries,
        delay_ms = delay.as_millis(),
        "Scheduling bounded Bluesky request retry"
    );
}

fn record_request_exhaustion(error: &TurboError) {
    let TurboError::BlueskyUpstream(error) = error else {
        return;
    };
    metrics::counter!(
        "bluesky_request_exhaustions_total",
        "operation" => error.operation.as_str(),
        "category" => error.category.as_str(),
        "attempts" => error.attempts.to_string(),
    )
    .increment(1);
    error!(
        operation = error.operation.as_str(),
        category = error.category.as_str(),
        status = error.status,
        attempts = error.attempts,
        retry_limit = error.retry_limit,
        request_cardinality = error.request_cardinality,
        request_fingerprint = error.request_fingerprint,
        upstream_summary = error.diagnostic_summary.as_deref(),
        "Bluesky request retry budget exhausted"
    );
}

fn transient_upstream_category(error: &TurboError) -> Option<UpstreamFailureCategory> {
    match error {
        TurboError::BlueskyUpstream(error) if error.transient => Some(error.category),
        _ => None,
    }
}

fn with_isolation_outcome(
    error: TurboError,
    outcome: crate::client::IsolationOutcome,
) -> TurboError {
    match error {
        TurboError::BlueskyUpstream(mut upstream) => {
            upstream.isolation = Some(outcome);
            TurboError::BlueskyUpstream(upstream)
        }
        other => other,
    }
}

impl BlueskyClient {
    pub fn new(
        session_strings: Vec<String>,
        auth_client: Option<Arc<BlueskyAuthClient>>,
        profile_batch_size: usize,
        post_batch_size: usize,
        profile_batch_wait_ms: u64,
        post_batch_wait_ms: u64,
    ) -> TurboResult<Self> {
        Self::new_with_policies(
            session_strings,
            auth_client,
            profile_batch_size,
            post_batch_size,
            profile_batch_wait_ms,
            post_batch_wait_ms,
            RequestRetryPolicy::default(),
            ContainmentPolicy::default(),
        )
    }

    #[allow(clippy::too_many_arguments)]
    pub fn new_with_policies(
        session_strings: Vec<String>,
        auth_client: Option<Arc<BlueskyAuthClient>>,
        profile_batch_size: usize,
        post_batch_size: usize,
        profile_batch_wait_ms: u64,
        post_batch_wait_ms: u64,
        retry_policy: RequestRetryPolicy,
        containment_policy: ContainmentPolicy,
    ) -> TurboResult<Self> {
        Self::new_with_policies_and_coordination(
            session_strings,
            auth_client,
            profile_batch_size,
            post_batch_size,
            profile_batch_wait_ms,
            post_batch_wait_ms,
            retry_policy,
            containment_policy,
            DEFAULT_PROFILE_COORDINATION_KEY_CAPACITY.max(profile_batch_size),
            DEFAULT_PROFILE_COORDINATION_WAITER_CAPACITY.max(profile_batch_size),
            DEFAULT_POST_COORDINATION_KEY_CAPACITY.max(post_batch_size),
            DEFAULT_POST_COORDINATION_WAITER_CAPACITY.max(post_batch_size),
        )
    }

    #[allow(clippy::too_many_arguments)]
    pub fn new_with_policies_and_coordination(
        session_strings: Vec<String>,
        auth_client: Option<Arc<BlueskyAuthClient>>,
        profile_batch_size: usize,
        post_batch_size: usize,
        profile_batch_wait_ms: u64,
        post_batch_wait_ms: u64,
        retry_policy: RequestRetryPolicy,
        containment_policy: ContainmentPolicy,
        profile_key_capacity: usize,
        profile_waiter_capacity: usize,
        post_key_capacity: usize,
        post_waiter_capacity: usize,
    ) -> TurboResult<Self> {
        let quota = Quota::with_period(Duration::from_millis(REQUESTS_PER_SECOND_MS))
            .expect("Valid quota")
            .allow_burst(NonZeroU32::new(1).unwrap());

        let http_client = Client::builder()
            .timeout(Duration::from_secs(30))
            .connect_timeout(Duration::from_secs(10))
            .user_agent("jetstream-turbo/0.1.0")
            .pool_max_idle_per_host(10)
            .pool_idle_timeout(Duration::from_secs(30))
            .tcp_keepalive(Duration::from_secs(60))
            .tcp_nodelay(true)
            .build()?;

        let session_strings = Arc::new(RwLock::new(session_strings));
        let refresh_jwt = Arc::new(RwLock::new(None));
        let expires_at = Arc::new(RwLock::new(None));
        let rate_limiter = Arc::new(RateLimiter::direct(quota));
        let api_base_url = "https://bsky.social/xrpc".to_string();
        let isolation_recurrence = Arc::new(AtomicU32::new(0));

        let collector_deps = BatchCollectorDeps {
            http_client: http_client.clone(),
            session_strings: session_strings.clone(),
            rate_limiter: rate_limiter.clone(),
            api_base_url: api_base_url.clone(),
            retry_policy,
            containment_policy,
            isolation_recurrence: Arc::clone(&isolation_recurrence),
            auth_client: auth_client.clone(),
            refresh_jwt: refresh_jwt.clone(),
            expires_at: expires_at.clone(),
        };

        let profile_batch_collector = Arc::new(RwLock::new(ProfileBatchCollector::new(
            BatchConfig {
                batch_size: profile_batch_size,
                wait_ms: profile_batch_wait_ms,
            },
            collector_deps.clone(),
            CoordinationLimits {
                key_capacity: profile_key_capacity,
                waiter_capacity: profile_waiter_capacity,
            },
        )?));

        let post_batch_collector = Arc::new(RwLock::new(PostBatchCollector::new(
            BatchConfig {
                batch_size: post_batch_size,
                wait_ms: post_batch_wait_ms,
            },
            collector_deps,
            CoordinationLimits {
                key_capacity: post_key_capacity,
                waiter_capacity: post_waiter_capacity,
            },
        )?));

        Ok(Self {
            session_strings,
            refresh_jwt,
            expires_at,
            auth_client,
            isolation_recurrence,
            profile_batch_collector,
            post_batch_collector,
        })
    }

    /// Point-in-time snapshot of the profile and post fetch counters.
    pub async fn fetch_diagnostics(&self) -> BlueskyFetchDiagnostics {
        BlueskyFetchDiagnostics {
            profiles: self.profile_batch_collector.read().await.fetch_snapshot(),
            posts: self.post_batch_collector.read().await.fetch_snapshot(),
        }
    }

    /// Point-in-time bounded coordination state with no identifier values.
    pub async fn coordination_diagnostics(&self) -> BlueskyCoordinationDiagnostics {
        BlueskyCoordinationDiagnostics {
            profiles: self
                .profile_batch_collector
                .read()
                .await
                .coordination
                .snapshot(),
            posts: self
                .post_batch_collector
                .read()
                .await
                .coordination
                .snapshot(),
        }
    }

    pub fn set_failure_recurrence(&self, recurrence: u32) {
        self.isolation_recurrence
            .store(recurrence, Ordering::Release);
    }

    pub async fn refresh_sessions(
        &self,
        new_sessions: Vec<String>,
        new_refresh_jwt: Option<String>,
        new_expires_at: Option<String>,
    ) {
        let mut sessions = self.session_strings.write().await;
        *sessions = new_sessions;
        info!("Refreshed {} session strings", sessions.len());

        if let Some(refresh_jwt) = new_refresh_jwt {
            let mut jwt = self.refresh_jwt.write().await;
            *jwt = Some(refresh_jwt);
        }

        if let Some(expires_at) = new_expires_at {
            let mut exp = self.expires_at.write().await;
            *exp = Some(expires_at.clone());
            info!("Session expires at: {}", expires_at);
        }
    }

    pub async fn should_refresh(&self) -> bool {
        let expires_at = self.expires_at.read().await;
        if let Some(ref exp) = *expires_at {
            if let Ok(exp_time) = chrono::DateTime::parse_from_rfc3339(exp) {
                let now = chrono::Utc::now();
                let duration_until_expiry = exp_time.signed_duration_since(now);
                return duration_until_expiry.num_seconds() < 3600;
            }
        }
        true
    }

    pub async fn get_refresh_jwt(&self) -> Option<String> {
        self.refresh_jwt.read().await.clone()
    }

    pub async fn refresh_session_with_fallback(&self) -> TurboResult<()> {
        if let Some(ref auth_client) = self.auth_client {
            if let Some(refresh_jwt) = self.get_refresh_jwt().await {
                match auth_client.refresh_session(&refresh_jwt).await {
                    Ok(auth_response) => {
                        self.refresh_sessions(
                            vec![auth_response.access_jwt],
                            Some(auth_response.refresh_jwt),
                            auth_response.expires_at,
                        )
                        .await;
                        info!("Session refreshed successfully");
                        return Ok(());
                    }
                    Err(TurboError::ExpiredToken(_)) => {
                        warn!("Refresh token expired, re-authenticating with credentials");
                    }
                    Err(e) => {
                        error!("Bluesky session refresh failed");
                        return Err(e);
                    }
                }
            }

            match auth_client.authenticate().await {
                Ok(auth_response) => {
                    self.refresh_sessions(
                        vec![auth_response.access_jwt],
                        Some(auth_response.refresh_jwt),
                        auth_response.expires_at,
                    )
                    .await;
                    info!("Re-authenticated successfully");
                    Ok(())
                }
                Err(e) => {
                    error!("Bluesky re-authentication failed");
                    Err(e)
                }
            }
        } else {
            Err(TurboError::ExpiredToken(
                "No auth client available for re-authentication".to_string(),
            ))
        }
    }

    pub async fn get_session_count(&self) -> usize {
        self.session_strings.read().await.len()
    }

    #[cfg(any(test, feature = "testing"))]
    pub async fn set_api_base_url_for_test(&self, api_base_url: String) {
        self.profile_batch_collector.write().await.api_base_url = api_base_url.clone();
        self.post_batch_collector.write().await.api_base_url = api_base_url;
    }

    #[cfg(test)]
    async fn collector_ownership_snapshot(&self) -> BlueskyCollectorOwnershipSnapshot {
        let coordination = self.coordination_diagnostics().await;
        let profiles = CollectorOwnershipSnapshot {
            pending_keys: coordination.profiles.pending_keys,
            in_flight_keys: coordination.profiles.in_flight_keys,
            waiters: coordination.profiles.waiters,
            retained_identifier_bytes: coordination.profiles.retained_identifier_bytes,
            completed_result_owners: coordination.profiles.completed_result_owners,
        };
        let posts = CollectorOwnershipSnapshot {
            pending_keys: coordination.posts.pending_keys,
            in_flight_keys: coordination.posts.in_flight_keys,
            waiters: coordination.posts.waiters,
            retained_identifier_bytes: coordination.posts.retained_identifier_bytes,
            completed_result_owners: coordination.posts.completed_result_owners,
        };

        BlueskyCollectorOwnershipSnapshot { profiles, posts }
    }
}

impl ProfileFetcher for BlueskyClient {
    #[instrument(name = "bulk_fetch_profiles", skip(self, dids), fields(count))]
    async fn bulk_fetch_profiles(
        &self,
        dids: &[String],
    ) -> TurboResult<Vec<Option<Arc<BlueskyProfile>>>> {
        tracing::Span::current().record("count", dids.len());

        if dids.is_empty() {
            return Ok(vec![]);
        }

        let fetch_started = Instant::now();
        let collector = self.profile_batch_collector.read().await;
        let profiles = collector.add_and_fetch(dids.to_vec()).await;
        collector.log_partial_percentage();
        let lock_elapsed_ns = fetch_started.elapsed().as_nanos() as u64;
        collector
            .fetch
            .lock_duration_ns_total
            .fetch_add(lock_elapsed_ns, Ordering::Relaxed);
        collector
            .fetch
            .lock_duration_count
            .fetch_add(1, Ordering::Relaxed);

        profiles
    }
}

impl PostFetcher for BlueskyClient {
    #[instrument(
        name = "bulk_fetch_posts",
        skip(self, uris),
        fields(count, valid_count)
    )]
    async fn bulk_fetch_posts(&self, uris: &[String]) -> TurboResult<Vec<PostFetchOutcome>> {
        if uris.is_empty() {
            return Ok(vec![]);
        }

        let count = uris.len();
        tracing::Span::current().record("count", count);

        let valid_uris: Vec<String> = uris
            .iter()
            .filter(|uri| !uri.is_empty() && is_valid_at_uri(uri))
            .cloned()
            .collect();

        let valid_count = valid_uris.len();
        tracing::Span::current().record("valid_count", valid_count);

        let filtered_count = uris.len() - valid_uris.len();
        if filtered_count > 0 {
            warn!(
                "Filtered {} invalid URIs out of {}",
                filtered_count,
                uris.len()
            );
        }

        if valid_uris.is_empty() {
            return Ok(vec![]);
        }

        let fetch_started = Instant::now();
        let collector = self.post_batch_collector.read().await;
        let fetch_result = collector.add_and_fetch(valid_uris.clone()).await;
        collector.log_partial_percentage();
        let lock_elapsed_ns = fetch_started.elapsed().as_nanos() as u64;
        collector
            .fetch
            .lock_duration_ns_total
            .fetch_add(lock_elapsed_ns, Ordering::Relaxed);
        collector
            .fetch
            .lock_duration_count
            .fetch_add(1, Ordering::Relaxed);

        fetch_result
            .into_iter()
            .map(|result| match result {
                Ok(Some(post)) => Ok(PostFetchOutcome::Found(post)),
                Ok(None) => Ok(PostFetchOutcome::Missing),
                Err(error) => {
                    let error = error.into_error();
                    optional_post_failure(&error, &valid_uris)
                        .map(PostFetchOutcome::TemporarilyUnavailable)
                        .ok_or(error)
                }
            })
            .collect()
    }
}

fn optional_post_failure(
    error: &TurboError,
    uris: &[String],
) -> Option<crate::client::HydrationFailure> {
    let upstream = match error {
        TurboError::BlueskyUpstream(error) => return Some(error.into()),
        TurboError::HttpRequest(_) => UpstreamHttpError {
            operation: BlueskyOperation::Posts,
            status: None,
            category: UpstreamFailureCategory::Transport,
            diagnostic_summary: None,
            attempts: 1,
            retry_limit: 0,
            request_cardinality: uris.len(),
            transient: true,
            request_fingerprint: stable_identifier_fingerprint(uris),
            isolation: None,
        },
        TurboError::Timeout(_) => UpstreamHttpError {
            operation: BlueskyOperation::Posts,
            status: None,
            category: UpstreamFailureCategory::RequestTimeout,
            diagnostic_summary: None,
            attempts: 1,
            retry_limit: 0,
            request_cardinality: uris.len(),
            transient: true,
            request_fingerprint: stable_identifier_fingerprint(uris),
            isolation: None,
        },
        TurboError::RateLimitExceeded => UpstreamHttpError {
            operation: BlueskyOperation::Posts,
            status: Some(429),
            category: UpstreamFailureCategory::RateLimited,
            diagnostic_summary: None,
            attempts: 1,
            retry_limit: 0,
            request_cardinality: uris.len(),
            transient: true,
            request_fingerprint: stable_identifier_fingerprint(uris),
            isolation: None,
        },
        TurboError::ExpiredToken(_) => UpstreamHttpError {
            operation: BlueskyOperation::Posts,
            status: Some(401),
            category: UpstreamFailureCategory::Authentication,
            diagnostic_summary: None,
            attempts: 1,
            retry_limit: 0,
            request_cardinality: uris.len(),
            transient: false,
            request_fingerprint: stable_identifier_fingerprint(uris),
            isolation: None,
        },
        TurboError::PermissionDenied(_) => UpstreamHttpError {
            operation: BlueskyOperation::Posts,
            status: Some(403),
            category: UpstreamFailureCategory::Permission,
            diagnostic_summary: None,
            attempts: 1,
            retry_limit: 0,
            request_cardinality: uris.len(),
            transient: false,
            request_fingerprint: stable_identifier_fingerprint(uris),
            isolation: None,
        },
        TurboError::InvalidApiResponse(_)
        | TurboError::JsonSerialization(_)
        | TurboError::JsonDeserialization(_) => UpstreamHttpError {
            operation: BlueskyOperation::Posts,
            status: None,
            category: UpstreamFailureCategory::Decode,
            diagnostic_summary: None,
            attempts: 1,
            retry_limit: 0,
            request_cardinality: uris.len(),
            transient: false,
            request_fingerprint: stable_identifier_fingerprint(uris),
            isolation: None,
        },
        _ => return None,
    };
    Some((&upstream).into())
}

impl ProfileBatchCollector {
    fn new(
        config: BatchConfig,
        deps: BatchCollectorDeps,
        limits: CoordinationLimits,
    ) -> TurboResult<Self> {
        let BatchCollectorDeps {
            http_client,
            session_strings,
            rate_limiter,
            api_base_url,
            retry_policy,
            containment_policy,
            isolation_recurrence,
            auth_client,
            refresh_jwt,
            expires_at,
        } = deps;
        let coordination = BoundedCoordinator::new(
            limits,
            config.batch_size,
            Duration::from_millis(config.wait_ms),
            SharedFetchError::claimant_cancelled(BlueskyOperation::Profiles),
        )
        .map_err(|error| TurboError::Internal(error.to_string()))?;
        Ok(Self {
            config,
            http_client,
            session_strings,
            rate_limiter,
            api_base_url,
            retry_policy,
            containment_policy,
            isolation_recurrence,
            auth_client,
            refresh_jwt,
            expires_at,
            coordination,
            batches_total: AtomicU64::new(0),
            batches_partial: AtomicU64::new(0),
            fetch: FetchDiagnostics::default(),
        })
    }

    async fn get_session_string(&self) -> TurboResult<String> {
        let sessions = self.session_strings.read().await;
        if sessions.is_empty() {
            return Err(TurboError::PermissionDenied(
                "No valid session strings available".to_string(),
            ));
        }
        Ok(sessions[0].clone())
    }

    async fn refresh_session_with_fallback(&self) -> TurboResult<()> {
        if let Some(ref auth_client) = self.auth_client {
            let refresh_jwt = self.refresh_jwt.read().await.clone();
            if let Some(refresh_jwt) = refresh_jwt {
                match auth_client.refresh_session(&refresh_jwt).await {
                    Ok(auth_response) => {
                        let mut sessions = self.session_strings.write().await;
                        *sessions = vec![auth_response.access_jwt];
                        let mut jwt = self.refresh_jwt.write().await;
                        *jwt = Some(auth_response.refresh_jwt);
                        if let Some(expires_at) = auth_response.expires_at {
                            let mut exp = self.expires_at.write().await;
                            *exp = Some(expires_at);
                        }
                        info!("Session refreshed successfully");
                        return Ok(());
                    }
                    Err(TurboError::ExpiredToken(_)) => {
                        warn!("Refresh token expired, re-authenticating with credentials");
                    }
                    Err(e) => {
                        error!("Bluesky session refresh failed");
                        return Err(e);
                    }
                }
            }

            match auth_client.authenticate().await {
                Ok(auth_response) => {
                    let mut sessions = self.session_strings.write().await;
                    *sessions = vec![auth_response.access_jwt];
                    let mut jwt = self.refresh_jwt.write().await;
                    *jwt = Some(auth_response.refresh_jwt);
                    if let Some(expires_at) = auth_response.expires_at {
                        let mut exp = self.expires_at.write().await;
                        *exp = Some(expires_at);
                    }
                    info!("Re-authenticated successfully");
                    Ok(())
                }
                Err(e) => {
                    error!("Bluesky re-authentication failed");
                    Err(e)
                }
            }
        } else {
            Err(TurboError::ExpiredToken(
                "No auth client available for re-authentication".to_string(),
            ))
        }
    }

    /// Times the full HTTP chain (incl. retries and isolation bisection) for
    /// one request. The counter pair feeds an average-latency metric.
    async fn fetch_batch_with_retry(
        &self,
        dids: &[String],
    ) -> TurboResult<Vec<Option<Arc<BlueskyProfile>>>> {
        let start = Instant::now();
        let result = self.fetch_batch_with_retry_inner(dids).await;
        let elapsed_ns = start.elapsed().as_nanos() as u64;
        self.fetch
            .http_duration_ns_total
            .fetch_add(elapsed_ns, Ordering::Relaxed);
        self.fetch
            .http_duration_count
            .fetch_add(1, Ordering::Relaxed);
        if let Err(error) = &result {
            match classify_fetch_error(error) {
                Some(FetchErrorClass::RateLimited) => {
                    self.fetch
                        .errors_rate_limited
                        .fetch_add(1, Ordering::Relaxed);
                }
                Some(FetchErrorClass::Upstream) => {
                    self.fetch.errors_upstream.fetch_add(1, Ordering::Relaxed);
                }
                None => {}
            }
        }
        result
    }

    /// The actual HTTP fetch chain with retries (timed by the wrapper above).
    async fn fetch_batch_with_retry_inner(
        &self,
        dids: &[String],
    ) -> TurboResult<Vec<Option<Arc<BlueskyProfile>>>> {
        let url = format!("{}/app.bsky.actor.getProfiles", self.api_base_url);
        let mut session_string = self.get_session_string().await?;
        let mut attempts = 0u32;
        let operation = BlueskyOperation::Profiles;
        let request_fingerprint = stable_identifier_fingerprint(dids);

        loop {
            attempts = attempts.saturating_add(1);
            let limiter_wait_start = Instant::now();
            self.rate_limiter.until_ready().await;
            self.fetch.rate_limiter_wait_ns_total.fetch_add(
                limiter_wait_start.elapsed().as_nanos() as u64,
                Ordering::Relaxed,
            );
            self.fetch
                .rate_limiter_wait_count
                .fetch_add(1, Ordering::Relaxed);

            let mut query_params: Vec<(&str, &str)> = Vec::new();
            for did in dids {
                query_params.push(("actors", did));
            }

            let response = self
                .http_client
                .get(&url)
                .header("Authorization", format!("Bearer {session_string}"))
                .query(&query_params)
                .send()
                .await;

            match response {
                Ok(resp) => match resp.status() {
                    StatusCode::OK => {
                        let assembly_start = Instant::now();
                        let body = resp.text().await?;
                        let profiles_response: GetProfilesResponse = serde_json::from_str(&body)
                            .map_err(|e| {
                                error!(operation = operation.as_str(), error = %e, "Failed to decode Bluesky response");
                                TurboError::InvalidApiResponse(format!("Failed to decode: {e}"))
                            })?;
                        let mut result = vec![None; dids.len()];
                        for (i, profile) in profiles_response.profiles.into_iter().enumerate() {
                            if i < result.len() {
                                result[i] = Some(Arc::new(profile.into()));
                            }
                        }
                        self.fetch.assembly_duration_ns_total.fetch_add(
                            assembly_start.elapsed().as_nanos() as u64,
                            Ordering::Relaxed,
                        );
                        self.fetch
                            .assembly_duration_count
                            .fetch_add(1, Ordering::Relaxed);
                        return Ok(result);
                    }
                    StatusCode::UNAUTHORIZED => {
                        if attempts > self.retry_policy.max_retries {
                            return Err(upstream_error(
                                operation,
                                dids,
                                Some(StatusCode::UNAUTHORIZED),
                                UpstreamFailureCategory::Authentication,
                                None,
                                attempts,
                                self.retry_policy.max_retries,
                            ));
                        }
                        if self.refresh_session_with_fallback().await.is_err() {
                            return Err(TurboError::ExpiredToken(
                                "Bluesky session recovery failed".to_string(),
                            ));
                        }
                        session_string = self.get_session_string().await?;
                        continue;
                    }
                    StatusCode::BAD_REQUEST => {
                        let error_text = resp.text().await.unwrap_or_default();
                        let is_expired = error_text.contains("ExpiredToken");
                        if is_expired {
                            if attempts > self.retry_policy.max_retries {
                                return Err(upstream_error(
                                    operation,
                                    dids,
                                    Some(StatusCode::BAD_REQUEST),
                                    UpstreamFailureCategory::Authentication,
                                    Some(&error_text),
                                    attempts,
                                    self.retry_policy.max_retries,
                                ));
                            }
                            if self.refresh_session_with_fallback().await.is_err() {
                                return Err(TurboError::ExpiredToken(
                                    "Bluesky session recovery failed".to_string(),
                                ));
                            }
                            session_string = self.get_session_string().await?;
                            continue;
                        }
                        return Err(upstream_error(
                            operation,
                            dids,
                            Some(StatusCode::BAD_REQUEST),
                            UpstreamFailureCategory::PermanentResponse,
                            Some(&error_text),
                            attempts,
                            self.retry_policy.max_retries,
                        ));
                    }
                    status => {
                        if let Some(category) = transient_category(status) {
                            if attempts <= self.retry_policy.max_retries {
                                let delay = retry_delay(
                                    Some(&resp),
                                    Some(status),
                                    attempts,
                                    self.retry_policy,
                                    &request_fingerprint,
                                );
                                record_request_retry(
                                    operation,
                                    category,
                                    attempts,
                                    self.retry_policy,
                                    delay,
                                );
                                tokio::time::sleep(delay).await;
                                continue;
                            }
                            let body = resp.text().await.unwrap_or_default();
                            let error = upstream_error(
                                operation,
                                dids,
                                Some(status),
                                category,
                                Some(&body),
                                attempts,
                                self.retry_policy.max_retries,
                            );
                            record_request_exhaustion(&error);
                            return Err(error);
                        }
                        let body = resp.text().await.unwrap_or_default();
                        return Err(upstream_error(
                            operation,
                            dids,
                            Some(status),
                            if status == StatusCode::FORBIDDEN {
                                UpstreamFailureCategory::Permission
                            } else {
                                UpstreamFailureCategory::PermanentResponse
                            },
                            Some(&body),
                            attempts,
                            self.retry_policy.max_retries,
                        ));
                    }
                },
                Err(error) => {
                    let category = if error.is_timeout() {
                        UpstreamFailureCategory::RequestTimeout
                    } else {
                        UpstreamFailureCategory::Transport
                    };
                    if attempts > self.retry_policy.max_retries {
                        let error = upstream_error(
                            operation,
                            dids,
                            None,
                            category,
                            None,
                            attempts,
                            self.retry_policy.max_retries,
                        );
                        record_request_exhaustion(&error);
                        return Err(error);
                    }
                    let delay = retry_delay(
                        None,
                        None,
                        attempts,
                        self.retry_policy,
                        &request_fingerprint,
                    );
                    record_request_retry(operation, category, attempts, self.retry_policy, delay);
                    tokio::time::sleep(delay).await;
                }
            }
        }
    }

    async fn fetch_claim(
        &self,
        dids: &[String],
    ) -> Vec<Result<Option<Arc<BlueskyProfile>>, SharedFetchError>> {
        self.fetch
            .items_total
            .fetch_add(dids.len() as u64, Ordering::Relaxed);
        match self.fetch_batch_with_retry(dids).await {
            Ok(fetched) => fetched.into_iter().map(Ok).collect(),
            Err(error)
                if transient_upstream_category(&error).is_some()
                    && dids.len() > 1
                    && self.isolation_recurrence.load(Ordering::Acquire)
                        >= self.containment_policy.persistence_threshold =>
            {
                self.isolate_profiles(dids.to_vec(), error).await
            }
            Err(error) => {
                let error = SharedFetchError::from_error(&error);
                vec![Err(error); dids.len()]
            }
        }
    }

    async fn isolate_profiles(
        &self,
        mut identifiers: Vec<String>,
        root_error: TurboError,
    ) -> Vec<Result<Option<Arc<BlueskyProfile>>, SharedFetchError>> {
        let requested = identifiers.clone();
        identifiers.sort_unstable();
        let mut remaining_budget = self.containment_policy.isolation_request_budget;
        let midpoint = identifiers.len() / 2;
        let first_halves = vec![
            identifiers[..midpoint].to_vec(),
            identifiers[midpoint..].to_vec(),
        ];
        let mut failures = VecDeque::new();
        let mut first_categories = Vec::new();
        let mut last_error = root_error;
        let mut resolved = HashMap::new();

        info!(
            operation = BlueskyOperation::Profiles.as_str(),
            request_fingerprint = %stable_identifier_fingerprint(&identifiers),
            request_budget = remaining_budget,
            "Starting bounded Bluesky request isolation"
        );

        for half in first_halves {
            if remaining_budget == 0 {
                let error = with_isolation_outcome(
                    last_error,
                    crate::client::IsolationOutcome::BudgetExhausted,
                );
                let error = SharedFetchError::from_error(&error);
                return requested
                    .into_iter()
                    .map(|identifier| {
                        resolved
                            .remove(&identifier)
                            .unwrap_or_else(|| Err(error.clone()))
                    })
                    .collect();
            }
            remaining_budget -= 1;
            match self.fetch_batch_with_retry(&half).await {
                Ok(results) => {
                    resolved.extend(half.into_iter().zip(results.into_iter().map(Ok)));
                }
                Err(error) => {
                    first_categories.push(transient_upstream_category(&error));
                    last_error = SharedFetchError::from_error(&error).into_error();
                    failures.push_back((half, error));
                }
            }
        }

        if failures.len() == 2
            && first_categories[0].is_some()
            && first_categories[0] == first_categories[1]
        {
            let error = with_isolation_outcome(
                last_error,
                crate::client::IsolationOutcome::BroadOutage {
                    category: first_categories[0].expect("category checked"),
                },
            );
            let error = SharedFetchError::from_error(&error);
            return requested
                .into_iter()
                .map(|identifier| {
                    resolved
                        .remove(&identifier)
                        .unwrap_or_else(|| Err(error.clone()))
                })
                .collect();
        }

        while let Some((failing, failure_error)) = failures.pop_front() {
            if failing.len() == 1 {
                let error = with_isolation_outcome(
                    failure_error,
                    crate::client::IsolationOutcome::SingletonPoison {
                        request_fingerprint: stable_identifier_fingerprint(&failing),
                    },
                );
                resolved.insert(
                    failing[0].clone(),
                    Err(SharedFetchError::from_error(&error)),
                );
                continue;
            }
            let midpoint = failing.len() / 2;
            for subset in [failing[..midpoint].to_vec(), failing[midpoint..].to_vec()] {
                if remaining_budget == 0 {
                    let error = with_isolation_outcome(
                        last_error,
                        crate::client::IsolationOutcome::BudgetExhausted,
                    );
                    let error = SharedFetchError::from_error(&error);
                    return requested
                        .into_iter()
                        .map(|identifier| {
                            resolved
                                .remove(&identifier)
                                .unwrap_or_else(|| Err(error.clone()))
                        })
                        .collect();
                }
                remaining_budget -= 1;
                match self.fetch_batch_with_retry(&subset).await {
                    Ok(results) => {
                        resolved.extend(subset.into_iter().zip(results.into_iter().map(Ok)));
                    }
                    Err(error) => {
                        last_error = SharedFetchError::from_error(&error).into_error();
                        failures.push_back((subset, error));
                    }
                }
            }
        }

        let fallback = SharedFetchError::from_error(&last_error);
        requested
            .into_iter()
            .map(|identifier| {
                resolved
                    .remove(&identifier)
                    .unwrap_or_else(|| Err(fallback.clone()))
            })
            .collect()
    }

    pub async fn add_and_fetch(
        &self,
        dids: Vec<String>,
    ) -> TurboResult<Vec<Option<Arc<BlueskyProfile>>>> {
        let mut profiles = Vec::with_capacity(dids.len());
        for chunk in dids.chunks(self.config.batch_size) {
            let registrations = self
                .coordination
                .register(chunk)
                .await
                .map_err(|error| TurboError::Internal(error.to_string()))?;

            while self.coordination.snapshot().pending_keys > 0 {
                let Some(claim) = self.coordination.claim() else {
                    tokio::time::sleep(Duration::from_millis(1)).await;
                    continue;
                };
                let identifiers = claim
                    .identifiers()
                    .iter()
                    .map(|identifier| identifier.to_string())
                    .collect::<Vec<_>>();
                self.record_claim(&identifiers, "Profile");
                let outcomes = self.fetch_claim(&identifiers).await;
                claim.finalize(outcomes);
            }

            for registration in registrations {
                profiles.push(
                    registration
                        .receive()
                        .await
                        .map_err(SharedFetchError::into_error)?,
                );
            }
        }
        Ok(profiles)
    }

    fn record_claim(&self, identifiers: &[String], kind: &'static str) {
        self.batches_total.fetch_add(1, Ordering::Relaxed);
        if identifiers.len() < self.config.batch_size {
            self.batches_partial.fetch_add(1, Ordering::Relaxed);
        }
        let pct = (identifiers.len() as f64 / self.config.batch_size as f64) * 100.0;
        info!(
            "{} batch capacity: {}/{} ({:.0}%)",
            kind,
            identifiers.len(),
            self.config.batch_size,
            pct
        );
    }

    pub fn log_partial_percentage(&self) {
        let total = self.batches_total.load(Ordering::Relaxed);
        if total > 0 && total % 10 == 0 {
            let partial = self.batches_partial.load(Ordering::Relaxed);
            let pct = (partial as f64 / total as f64) * 100.0;
            info!(
                "Profile batch partial rate: {:.1}% ({}/{})",
                pct, partial, total
            );
        }
    }

    /// Point-in-time snapshot of this collector's fetch counters.
    fn fetch_snapshot(&self) -> BlueskyFetchKindDiagnostics {
        BlueskyFetchKindDiagnostics {
            requests_total: self.batches_total.load(Ordering::Relaxed),
            items_total: self.fetch.items_total.load(Ordering::Relaxed),
            lock_duration_ns_total: self.fetch.lock_duration_ns_total.load(Ordering::Relaxed),
            lock_duration_count: self.fetch.lock_duration_count.load(Ordering::Relaxed),
            http_duration_ns_total: self.fetch.http_duration_ns_total.load(Ordering::Relaxed),
            http_duration_count: self.fetch.http_duration_count.load(Ordering::Relaxed),
            errors_rate_limited: self.fetch.errors_rate_limited.load(Ordering::Relaxed),
            rate_limiter_wait_ns_total: self
                .fetch
                .rate_limiter_wait_ns_total
                .load(Ordering::Relaxed),
            rate_limiter_wait_count: self.fetch.rate_limiter_wait_count.load(Ordering::Relaxed),
            assembly_duration_ns_total: self
                .fetch
                .assembly_duration_ns_total
                .load(Ordering::Relaxed),
            assembly_duration_count: self.fetch.assembly_duration_count.load(Ordering::Relaxed),
            errors_upstream: self.fetch.errors_upstream.load(Ordering::Relaxed),
        }
    }
}

impl PostBatchCollector {
    fn new(
        config: BatchConfig,
        deps: BatchCollectorDeps,
        limits: CoordinationLimits,
    ) -> TurboResult<Self> {
        let BatchCollectorDeps {
            http_client,
            session_strings,
            rate_limiter,
            api_base_url,
            retry_policy,
            containment_policy,
            isolation_recurrence,
            auth_client,
            refresh_jwt,
            expires_at,
        } = deps;
        let coordination = BoundedCoordinator::new(
            limits,
            config.batch_size,
            Duration::from_millis(config.wait_ms),
            SharedFetchError::claimant_cancelled(BlueskyOperation::Posts),
        )
        .map_err(|error| TurboError::Internal(error.to_string()))?;
        Ok(Self {
            config,
            http_client,
            session_strings,
            rate_limiter,
            api_base_url,
            retry_policy,
            containment_policy,
            isolation_recurrence,
            auth_client,
            refresh_jwt,
            expires_at,
            coordination,
            batches_total: AtomicU64::new(0),
            batches_partial: AtomicU64::new(0),
            fetch: FetchDiagnostics::default(),
        })
    }

    async fn get_session_string(&self) -> TurboResult<String> {
        let sessions = self.session_strings.read().await;
        if sessions.is_empty() {
            return Err(TurboError::PermissionDenied(
                "No valid session strings available".to_string(),
            ));
        }
        Ok(sessions[0].clone())
    }

    async fn refresh_session_with_fallback(&self) -> TurboResult<()> {
        if let Some(ref auth_client) = self.auth_client {
            let refresh_jwt = self.refresh_jwt.read().await.clone();
            if let Some(refresh_jwt) = refresh_jwt {
                match auth_client.refresh_session(&refresh_jwt).await {
                    Ok(auth_response) => {
                        let mut sessions = self.session_strings.write().await;
                        *sessions = vec![auth_response.access_jwt];
                        let mut jwt = self.refresh_jwt.write().await;
                        *jwt = Some(auth_response.refresh_jwt);
                        if let Some(expires_at) = auth_response.expires_at {
                            let mut exp = self.expires_at.write().await;
                            *exp = Some(expires_at);
                        }
                        info!("Session refreshed successfully");
                        return Ok(());
                    }
                    Err(TurboError::ExpiredToken(_)) => {
                        warn!("Refresh token expired, re-authenticating with credentials");
                    }
                    Err(e) => {
                        error!("Bluesky session refresh failed");
                        return Err(e);
                    }
                }
            }

            match auth_client.authenticate().await {
                Ok(auth_response) => {
                    let mut sessions = self.session_strings.write().await;
                    *sessions = vec![auth_response.access_jwt];
                    let mut jwt = self.refresh_jwt.write().await;
                    *jwt = Some(auth_response.refresh_jwt);
                    if let Some(expires_at) = auth_response.expires_at {
                        let mut exp = self.expires_at.write().await;
                        *exp = Some(expires_at);
                    }
                    info!("Re-authenticated successfully");
                    Ok(())
                }
                Err(e) => {
                    error!("Bluesky re-authentication failed");
                    Err(e)
                }
            }
        } else {
            Err(TurboError::ExpiredToken(
                "No auth client available for re-authentication".to_string(),
            ))
        }
    }

    fn convert_bulk_post_response(
        &self,
        response: crate::models::bluesky::GetPostsResponse,
    ) -> Arc<BlueskyPost> {
        Arc::new(BlueskyPost {
            uri: response.uri,
            cid: response.cid,
            author: response.author.into(),
            text: response
                .record
                .get("text")
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .to_string(),
            created_at: chrono::Utc::now(),
            embed: response.embed.and_then(|e| serde_json::from_value(e).ok()),
            reply: response.reply.and_then(|r| serde_json::from_value(r).ok()),
            facets: response
                .record
                .get("facets")
                .and_then(|v| serde_json::from_value(v.clone()).ok()),
            labels: response.labels,
            like_count: response.like_count,
            repost_count: response.repost_count,
            reply_count: response.reply_count,
        })
    }

    /// Times the full HTTP chain (incl. retries and isolation bisection) for
    /// one request. The counter pair feeds an average-latency metric.
    async fn fetch_batch_with_retry(
        &self,
        uris: &[String],
    ) -> TurboResult<Vec<Option<Arc<BlueskyPost>>>> {
        let start = Instant::now();
        let result = self.fetch_batch_with_retry_inner(uris).await;
        let elapsed_ns = start.elapsed().as_nanos() as u64;
        self.fetch
            .http_duration_ns_total
            .fetch_add(elapsed_ns, Ordering::Relaxed);
        self.fetch
            .http_duration_count
            .fetch_add(1, Ordering::Relaxed);
        if let Err(error) = &result {
            match classify_fetch_error(error) {
                Some(FetchErrorClass::RateLimited) => {
                    self.fetch
                        .errors_rate_limited
                        .fetch_add(1, Ordering::Relaxed);
                }
                Some(FetchErrorClass::Upstream) => {
                    self.fetch.errors_upstream.fetch_add(1, Ordering::Relaxed);
                }
                None => {}
            }
        }
        result
    }

    /// The actual HTTP fetch chain with retries (timed by the wrapper above).
    async fn fetch_batch_with_retry_inner(
        &self,
        uris: &[String],
    ) -> TurboResult<Vec<Option<Arc<BlueskyPost>>>> {
        let url = format!("{}/app.bsky.feed.getPosts", self.api_base_url);
        let mut session_string = self.get_session_string().await?;
        let mut attempts = 0u32;
        let operation = BlueskyOperation::Posts;
        let request_fingerprint = stable_identifier_fingerprint(uris);

        loop {
            attempts = attempts.saturating_add(1);
            let limiter_wait_start = Instant::now();
            self.rate_limiter.until_ready().await;
            self.fetch.rate_limiter_wait_ns_total.fetch_add(
                limiter_wait_start.elapsed().as_nanos() as u64,
                Ordering::Relaxed,
            );
            self.fetch
                .rate_limiter_wait_count
                .fetch_add(1, Ordering::Relaxed);

            let mut query_params: Vec<(&str, &str)> = Vec::new();
            for uri in uris {
                query_params.push(("uris", uri));
            }

            let response = self
                .http_client
                .get(&url)
                .header("Authorization", format!("Bearer {session_string}"))
                .query(&query_params)
                .send()
                .await;

            match response {
                Ok(resp) => match resp.status() {
                    StatusCode::OK => {
                        let assembly_start = Instant::now();
                        let body = resp.text().await?;
                        let posts_response: GetPostsBulkResponse = serde_json::from_str(&body)
                            .map_err(|e| {
                                error!(operation = operation.as_str(), error = %e, "Failed to decode Bluesky response");
                                TurboError::InvalidApiResponse(format!("Failed to decode: {e}"))
                            })?;

                        let mut results = vec![None; uris.len()];
                        for post_response in posts_response.posts {
                            if let Some(uri) = uris.iter().position(|u| u == &post_response.uri) {
                                results[uri] = Some(self.convert_bulk_post_response(post_response));
                            }
                        }

                        self.fetch.assembly_duration_ns_total.fetch_add(
                            assembly_start.elapsed().as_nanos() as u64,
                            Ordering::Relaxed,
                        );
                        self.fetch
                            .assembly_duration_count
                            .fetch_add(1, Ordering::Relaxed);
                        return Ok(results);
                    }
                    StatusCode::UNAUTHORIZED => {
                        if attempts > self.retry_policy.max_retries {
                            return Err(upstream_error(
                                operation,
                                uris,
                                Some(StatusCode::UNAUTHORIZED),
                                UpstreamFailureCategory::Authentication,
                                None,
                                attempts,
                                self.retry_policy.max_retries,
                            ));
                        }
                        if self.refresh_session_with_fallback().await.is_err() {
                            return Err(TurboError::ExpiredToken(
                                "Bluesky session recovery failed".to_string(),
                            ));
                        }
                        session_string = self.get_session_string().await?;
                        continue;
                    }
                    StatusCode::BAD_REQUEST => {
                        let error_text = resp.text().await.unwrap_or_default();
                        let is_expired = error_text.contains("ExpiredToken");
                        if is_expired {
                            if attempts > self.retry_policy.max_retries {
                                return Err(upstream_error(
                                    operation,
                                    uris,
                                    Some(StatusCode::BAD_REQUEST),
                                    UpstreamFailureCategory::Authentication,
                                    Some(&error_text),
                                    attempts,
                                    self.retry_policy.max_retries,
                                ));
                            }
                            if self.refresh_session_with_fallback().await.is_err() {
                                return Err(TurboError::ExpiredToken(
                                    "Bluesky session recovery failed".to_string(),
                                ));
                            }
                            session_string = self.get_session_string().await?;
                            continue;
                        }
                        return Err(upstream_error(
                            operation,
                            uris,
                            Some(StatusCode::BAD_REQUEST),
                            UpstreamFailureCategory::PermanentResponse,
                            Some(&error_text),
                            attempts,
                            self.retry_policy.max_retries,
                        ));
                    }
                    status => {
                        if let Some(category) = transient_category(status) {
                            if attempts <= self.retry_policy.max_retries {
                                let delay = retry_delay(
                                    Some(&resp),
                                    Some(status),
                                    attempts,
                                    self.retry_policy,
                                    &request_fingerprint,
                                );
                                record_request_retry(
                                    operation,
                                    category,
                                    attempts,
                                    self.retry_policy,
                                    delay,
                                );
                                tokio::time::sleep(delay).await;
                                continue;
                            }
                            let body = resp.text().await.unwrap_or_default();
                            let error = upstream_error(
                                operation,
                                uris,
                                Some(status),
                                category,
                                Some(&body),
                                attempts,
                                self.retry_policy.max_retries,
                            );
                            record_request_exhaustion(&error);
                            return Err(error);
                        }
                        let body = resp.text().await.unwrap_or_default();
                        return Err(upstream_error(
                            operation,
                            uris,
                            Some(status),
                            if status == StatusCode::FORBIDDEN {
                                UpstreamFailureCategory::Permission
                            } else {
                                UpstreamFailureCategory::PermanentResponse
                            },
                            Some(&body),
                            attempts,
                            self.retry_policy.max_retries,
                        ));
                    }
                },
                Err(error) => {
                    let category = if error.is_timeout() {
                        UpstreamFailureCategory::RequestTimeout
                    } else {
                        UpstreamFailureCategory::Transport
                    };
                    if attempts > self.retry_policy.max_retries {
                        let error = upstream_error(
                            operation,
                            uris,
                            None,
                            category,
                            None,
                            attempts,
                            self.retry_policy.max_retries,
                        );
                        record_request_exhaustion(&error);
                        return Err(error);
                    }
                    let delay = retry_delay(
                        None,
                        None,
                        attempts,
                        self.retry_policy,
                        &request_fingerprint,
                    );
                    record_request_retry(operation, category, attempts, self.retry_policy, delay);
                    tokio::time::sleep(delay).await;
                }
            }
        }
    }

    async fn fetch_claim(
        &self,
        uris: &[String],
    ) -> Vec<Result<Option<Arc<BlueskyPost>>, SharedFetchError>> {
        self.fetch
            .items_total
            .fetch_add(uris.len() as u64, Ordering::Relaxed);
        match self.fetch_batch_with_retry(uris).await {
            Ok(fetched) => fetched.into_iter().map(Ok).collect(),
            Err(error)
                if transient_upstream_category(&error).is_some()
                    && uris.len() > 1
                    && self.isolation_recurrence.load(Ordering::Acquire)
                        >= self.containment_policy.persistence_threshold =>
            {
                self.isolate_posts(uris.to_vec(), error).await
            }
            Err(error) => {
                let error = SharedFetchError::from_error(&error);
                vec![Err(error); uris.len()]
            }
        }
    }

    async fn isolate_posts(
        &self,
        mut identifiers: Vec<String>,
        root_error: TurboError,
    ) -> Vec<Result<Option<Arc<BlueskyPost>>, SharedFetchError>> {
        let requested = identifiers.clone();
        identifiers.sort_unstable();
        let mut remaining_budget = self.containment_policy.isolation_request_budget;
        let midpoint = identifiers.len() / 2;
        let first_halves = vec![
            identifiers[..midpoint].to_vec(),
            identifiers[midpoint..].to_vec(),
        ];
        let mut failures = VecDeque::new();
        let mut first_categories = Vec::new();
        let mut last_error = root_error;
        let mut resolved = HashMap::new();

        info!(
            operation = BlueskyOperation::Posts.as_str(),
            request_fingerprint = %stable_identifier_fingerprint(&identifiers),
            request_budget = remaining_budget,
            "Starting bounded Bluesky request isolation"
        );

        for half in first_halves {
            if remaining_budget == 0 {
                let error = with_isolation_outcome(
                    last_error,
                    crate::client::IsolationOutcome::BudgetExhausted,
                );
                let error = SharedFetchError::from_error(&error);
                return requested
                    .into_iter()
                    .map(|identifier| {
                        resolved
                            .remove(&identifier)
                            .unwrap_or_else(|| Err(error.clone()))
                    })
                    .collect();
            }
            remaining_budget -= 1;
            match self.fetch_batch_with_retry(&half).await {
                Ok(results) => {
                    resolved.extend(half.into_iter().zip(results.into_iter().map(Ok)));
                }
                Err(error) => {
                    first_categories.push(transient_upstream_category(&error));
                    last_error = SharedFetchError::from_error(&error).into_error();
                    failures.push_back((half, error));
                }
            }
        }

        if failures.len() == 2
            && first_categories[0].is_some()
            && first_categories[0] == first_categories[1]
        {
            let error = with_isolation_outcome(
                last_error,
                crate::client::IsolationOutcome::BroadOutage {
                    category: first_categories[0].expect("category checked"),
                },
            );
            let error = SharedFetchError::from_error(&error);
            return requested
                .into_iter()
                .map(|identifier| {
                    resolved
                        .remove(&identifier)
                        .unwrap_or_else(|| Err(error.clone()))
                })
                .collect();
        }

        while let Some((failing, failure_error)) = failures.pop_front() {
            if failing.len() == 1 {
                let error = with_isolation_outcome(
                    failure_error,
                    crate::client::IsolationOutcome::SingletonPoison {
                        request_fingerprint: stable_identifier_fingerprint(&failing),
                    },
                );
                resolved.insert(
                    failing[0].clone(),
                    Err(SharedFetchError::from_error(&error)),
                );
                continue;
            }
            let midpoint = failing.len() / 2;
            for subset in [failing[..midpoint].to_vec(), failing[midpoint..].to_vec()] {
                if remaining_budget == 0 {
                    let error = with_isolation_outcome(
                        last_error,
                        crate::client::IsolationOutcome::BudgetExhausted,
                    );
                    let error = SharedFetchError::from_error(&error);
                    return requested
                        .into_iter()
                        .map(|identifier| {
                            resolved
                                .remove(&identifier)
                                .unwrap_or_else(|| Err(error.clone()))
                        })
                        .collect();
                }
                remaining_budget -= 1;
                match self.fetch_batch_with_retry(&subset).await {
                    Ok(results) => {
                        resolved.extend(subset.into_iter().zip(results.into_iter().map(Ok)));
                    }
                    Err(error) => {
                        last_error = SharedFetchError::from_error(&error).into_error();
                        failures.push_back((subset, error));
                    }
                }
            }
        }

        let fallback = SharedFetchError::from_error(&last_error);
        requested
            .into_iter()
            .map(|identifier| {
                resolved
                    .remove(&identifier)
                    .unwrap_or_else(|| Err(fallback.clone()))
            })
            .collect()
    }

    pub async fn add_and_fetch(
        &self,
        uris: Vec<String>,
    ) -> Vec<Result<Option<Arc<BlueskyPost>>, SharedFetchError>> {
        let mut posts = Vec::with_capacity(uris.len());
        for chunk in uris.chunks(self.config.batch_size) {
            let registrations = match self.coordination.register(chunk).await {
                Ok(registrations) => registrations,
                Err(error) => {
                    posts.extend(
                        (0..chunk.len())
                            .map(|_| Err(SharedFetchError::Internal(error.to_string()))),
                    );
                    continue;
                }
            };

            while self.coordination.snapshot().pending_keys > 0 {
                let Some(claim) = self.coordination.claim() else {
                    tokio::time::sleep(Duration::from_millis(1)).await;
                    continue;
                };
                let identifiers = claim
                    .identifiers()
                    .iter()
                    .map(|identifier| identifier.to_string())
                    .collect::<Vec<_>>();
                self.record_claim(&identifiers);
                let outcomes = self.fetch_claim(&identifiers).await;
                claim.finalize(outcomes);
            }

            for registration in registrations {
                posts.push(registration.receive().await);
            }
        }
        posts
    }

    fn record_claim(&self, identifiers: &[String]) {
        self.batches_total.fetch_add(1, Ordering::Relaxed);
        if identifiers.len() < self.config.batch_size {
            self.batches_partial.fetch_add(1, Ordering::Relaxed);
        }
        let pct = (identifiers.len() as f64 / self.config.batch_size as f64) * 100.0;
        info!(
            "Post batch capacity: {}/{} ({:.0}%)",
            identifiers.len(),
            self.config.batch_size,
            pct
        );
    }

    pub fn log_partial_percentage(&self) {
        let total = self.batches_total.load(Ordering::Relaxed);
        if total > 0 && total % 10 == 0 {
            let partial = self.batches_partial.load(Ordering::Relaxed);
            let pct = (partial as f64 / total as f64) * 100.0;
            info!(
                "Post batch partial rate: {:.1}% ({}/{})",
                pct, partial, total
            );
        }
    }

    /// Point-in-time snapshot of this collector's fetch counters.
    fn fetch_snapshot(&self) -> BlueskyFetchKindDiagnostics {
        BlueskyFetchKindDiagnostics {
            requests_total: self.batches_total.load(Ordering::Relaxed),
            items_total: self.fetch.items_total.load(Ordering::Relaxed),
            lock_duration_ns_total: self.fetch.lock_duration_ns_total.load(Ordering::Relaxed),
            lock_duration_count: self.fetch.lock_duration_count.load(Ordering::Relaxed),
            http_duration_ns_total: self.fetch.http_duration_ns_total.load(Ordering::Relaxed),
            http_duration_count: self.fetch.http_duration_count.load(Ordering::Relaxed),
            errors_rate_limited: self.fetch.errors_rate_limited.load(Ordering::Relaxed),
            rate_limiter_wait_ns_total: self
                .fetch
                .rate_limiter_wait_ns_total
                .load(Ordering::Relaxed),
            rate_limiter_wait_count: self.fetch.rate_limiter_wait_count.load(Ordering::Relaxed),
            assembly_duration_ns_total: self
                .fetch
                .assembly_duration_ns_total
                .load(Ordering::Relaxed),
            assembly_duration_count: self.fetch.assembly_duration_count.load(Ordering::Relaxed),
            errors_upstream: self.fetch.errors_upstream.load(Ordering::Relaxed),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::hydration::{CacheMissResolver, HydrationExecutionMode, Hydrator, TurboCache};
    use crate::testing::fixtures::create_reply_message;
    use std::sync::Mutex as StdMutex;
    use wiremock::matchers::{method, path};
    use wiremock::{Mock, MockServer, Request, Respond, ResponseTemplate};

    #[derive(Clone)]
    struct SequenceResponder {
        calls: Arc<AtomicU32>,
        first: ResponseTemplate,
        later: ResponseTemplate,
    }

    #[derive(Clone)]
    enum PoisonMode {
        One(String),
        All,
        SplitCategories,
    }

    #[derive(Clone)]
    struct PoisonResponder {
        mode: PoisonMode,
    }

    #[derive(Clone)]
    struct DelayedEchoResponder {
        delay: Duration,
    }

    impl Respond for DelayedEchoResponder {
        fn respond(&self, request: &Request) -> ResponseTemplate {
            PoisonResponder {
                mode: PoisonMode::One("never-match".to_string()),
            }
            .respond(request)
            .set_delay(self.delay)
        }
    }

    impl Respond for PoisonResponder {
        fn respond(&self, request: &Request) -> ResponseTemplate {
            let identifiers = request
                .url
                .query_pairs()
                .filter(|(key, _)| key == "actors" || key == "uris")
                .map(|(_, value)| value.into_owned())
                .collect::<Vec<_>>();
            let status = match &self.mode {
                PoisonMode::All => Some(502),
                PoisonMode::One(bad) if identifiers.iter().any(|value| value == bad) => Some(502),
                PoisonMode::SplitCategories if identifiers.iter().any(|value| value == "a") => {
                    Some(502)
                }
                PoisonMode::SplitCategories => Some(429),
                PoisonMode::One(_) => None,
            };
            if let Some(status) = status {
                return ResponseTemplate::new(status);
            }
            if request.url.path().ends_with("getProfiles") {
                ResponseTemplate::new(200).set_body_json(serde_json::json!({
                    "profiles": identifiers
                        .iter()
                        .map(|did| serde_json::json!({"did": did, "handle": "ok.bsky.social"}))
                        .collect::<Vec<_>>()
                }))
            } else {
                ResponseTemplate::new(200).set_body_json(serde_json::json!({
                    "posts": identifiers
                        .iter()
                        .map(|uri| post_body(uri)["posts"][0].clone())
                        .collect::<Vec<_>>()
                }))
            }
        }
    }

    impl Respond for SequenceResponder {
        fn respond(&self, _request: &Request) -> ResponseTemplate {
            if self.calls.fetch_add(1, Ordering::SeqCst) == 0 {
                self.first.clone()
            } else {
                self.later.clone()
            }
        }
    }

    fn fast_policy(max_retries: u32) -> RequestRetryPolicy {
        RequestRetryPolicy {
            max_retries,
            base_delay: Duration::from_millis(1),
            max_delay: Duration::from_millis(2),
        }
    }

    async fn client_for_server(
        server: &MockServer,
        max_retries: u32,
        containment_policy: ContainmentPolicy,
    ) -> BlueskyClient {
        let client = BlueskyClient::new_with_policies(
            vec!["test-session".to_string()],
            None,
            25,
            25,
            0,
            0,
            fast_policy(max_retries),
            containment_policy,
        )
        .unwrap();
        client.profile_batch_collector.write().await.api_base_url = server.uri();
        client.post_batch_collector.write().await.api_base_url = server.uri();
        client
    }

    async fn bounded_client_for_server(
        server: &MockServer,
        batch_size: usize,
        key_capacity: usize,
        waiter_capacity: usize,
    ) -> BlueskyClient {
        let client = BlueskyClient::new_with_policies_and_coordination(
            vec!["test-session".to_string()],
            None,
            batch_size,
            batch_size,
            0,
            0,
            fast_policy(0),
            ContainmentPolicy::default(),
            key_capacity,
            waiter_capacity,
            key_capacity,
            waiter_capacity,
        )
        .unwrap();
        client.set_api_base_url_for_test(server.uri()).await;
        client
    }

    fn profile_body(did: &str) -> serde_json::Value {
        serde_json::json!({"profiles": [{"did": did, "handle": "test.bsky.social"}]})
    }

    fn post_body(uri: &str) -> serde_json::Value {
        serde_json::json!({
            "posts": [{
                "uri": uri,
                "cid": "cid",
                "author": {"did": "did:plc:author", "handle": "author.bsky.social"},
                "record": {"text": "hello"},
                "embed": null,
                "reply": null,
                "labels": null,
                "like_count": 0,
                "repost_count": 0,
                "reply_count": 0
            }]
        })
    }

    #[tokio::test]
    async fn profile_request_recovers_from_502_then_200() {
        let server = MockServer::start().await;
        let calls = Arc::new(AtomicU32::new(0));
        Mock::given(method("GET"))
            .and(path("/app.bsky.actor.getProfiles"))
            .respond_with(SequenceResponder {
                calls: Arc::clone(&calls),
                first: ResponseTemplate::new(502),
                later: ResponseTemplate::new(200).set_body_json(profile_body("did:plc:test")),
            })
            .mount(&server)
            .await;
        let client = client_for_server(&server, 1, ContainmentPolicy::default()).await;
        let result = client
            .bulk_fetch_profiles(&["did:plc:test".to_string()])
            .await
            .unwrap();
        assert!(result[0].is_some());
        assert_eq!(calls.load(Ordering::SeqCst), 2);
    }

    #[tokio::test]
    async fn post_request_recovers_from_502_then_200() {
        let server = MockServer::start().await;
        let calls = Arc::new(AtomicU32::new(0));
        let uri = "at://did:plc:test/app.bsky.feed.post/one";
        Mock::given(method("GET"))
            .and(path("/app.bsky.feed.getPosts"))
            .respond_with(SequenceResponder {
                calls: Arc::clone(&calls),
                first: ResponseTemplate::new(502),
                later: ResponseTemplate::new(200).set_body_json(post_body(uri)),
            })
            .mount(&server)
            .await;
        let client = client_for_server(&server, 1, ContainmentPolicy::default()).await;
        let result = client.bulk_fetch_posts(&[uri.to_string()]).await.unwrap();
        assert!(matches!(result[0], PostFetchOutcome::Found(_)));
        assert_eq!(calls.load(Ordering::SeqCst), 2);
    }

    #[tokio::test]
    async fn successful_post_response_preserves_order_and_marks_omissions_missing() {
        let server = MockServer::start().await;
        let found = "at://did:plc:a/app.bsky.feed.post/found";
        let missing = "at://did:plc:b/app.bsky.feed.post/missing";
        Mock::given(method("GET"))
            .and(path("/app.bsky.feed.getPosts"))
            .respond_with(ResponseTemplate::new(200).set_body_json(post_body(found)))
            .mount(&server)
            .await;
        let client = client_for_server(&server, 0, ContainmentPolicy::default()).await;

        let outcomes = client
            .bulk_fetch_posts(&[found.to_string(), missing.to_string()])
            .await
            .unwrap();

        assert!(matches!(&outcomes[0], PostFetchOutcome::Found(post) if post.uri == found));
        assert!(matches!(outcomes[1], PostFetchOutcome::Missing));
    }

    #[tokio::test]
    async fn singleton_502_becomes_temporarily_unavailable_after_retries() {
        let server = MockServer::start().await;
        let uri = "at://did:plc:a/app.bsky.feed.post/poison";
        Mock::given(method("GET"))
            .and(path("/app.bsky.feed.getPosts"))
            .respond_with(ResponseTemplate::new(502))
            .expect(2)
            .mount(&server)
            .await;
        let client = client_for_server(&server, 1, ContainmentPolicy::default()).await;

        let outcomes = client.bulk_fetch_posts(&[uri.to_string()]).await.unwrap();
        assert!(matches!(
            &outcomes[0],
            PostFetchOutcome::TemporarilyUnavailable(failure)
                if failure.category == UpstreamFailureCategory::ServerError && failure.attempts == 2
        ));
    }

    #[tokio::test]
    async fn post_isolation_keeps_successful_subset_and_bounds_budget_exhaustion() {
        let server = MockServer::start().await;
        let uris = [
            "at://did:plc:a/app.bsky.feed.post/one".to_string(),
            "at://did:plc:b/app.bsky.feed.post/two".to_string(),
            "at://did:plc:c/app.bsky.feed.post/three".to_string(),
            "at://did:plc:d/app.bsky.feed.post/poison".to_string(),
        ];
        Mock::given(method("GET"))
            .and(path("/app.bsky.feed.getPosts"))
            .respond_with(PoisonResponder {
                mode: PoisonMode::One(uris[3].clone()),
            })
            .mount(&server)
            .await;
        let containment = ContainmentPolicy {
            isolation_request_budget: 1,
            ..ContainmentPolicy::default()
        };
        let client = client_for_server(&server, 0, containment).await;
        client.set_failure_recurrence(3);

        let outcomes = client.bulk_fetch_posts(&uris).await.unwrap();
        assert_eq!(outcomes.len(), uris.len());
        assert!(outcomes
            .iter()
            .any(|outcome| matches!(outcome, PostFetchOutcome::Found(_))));
        assert!(outcomes.iter().any(|outcome| matches!(
            outcome,
            PostFetchOutcome::TemporarilyUnavailable(failure)
                if failure.isolation == Some(crate::client::IsolationOutcome::BudgetExhausted)
        )));
        assert_eq!(server.received_requests().await.unwrap().len(), 2);
    }

    #[tokio::test]
    async fn retryable_statuses_stop_at_max_retries_plus_one() {
        for (status, max_retries) in [(502, 2), (429, 2), (502, 0), (429, 0)] {
            let server = MockServer::start().await;
            Mock::given(method("GET"))
                .and(path("/app.bsky.actor.getProfiles"))
                .respond_with(ResponseTemplate::new(status))
                .expect((max_retries + 1) as u64)
                .mount(&server)
                .await;
            let client =
                client_for_server(&server, max_retries, ContainmentPolicy::default()).await;
            let error = client
                .bulk_fetch_profiles(&["did:plc:test".to_string()])
                .await
                .unwrap_err();
            let TurboError::BlueskyUpstream(error) = error else {
                panic!("expected typed upstream error");
            };
            assert_eq!(error.attempts, max_retries + 1);
        }
    }

    #[tokio::test]
    async fn permanent_4xx_and_malformed_success_are_not_retried() {
        for response in [
            ResponseTemplate::new(404).set_body_string("not found"),
            ResponseTemplate::new(200).set_body_string("not-json"),
        ] {
            let server = MockServer::start().await;
            Mock::given(method("GET"))
                .and(path("/app.bsky.actor.getProfiles"))
                .respond_with(response)
                .expect(1)
                .mount(&server)
                .await;
            let client = client_for_server(&server, 3, ContainmentPolicy::default()).await;
            assert!(client
                .bulk_fetch_profiles(&["did:plc:test".to_string()])
                .await
                .is_err());
        }
    }

    #[tokio::test]
    async fn profile_isolation_keeps_successful_subset_request_scoped() {
        let server = MockServer::start().await;
        let good = "did:plc:good".to_string();
        let bad = "did:plc:poison".to_string();
        Mock::given(method("GET"))
            .and(path("/app.bsky.actor.getProfiles"))
            .respond_with(PoisonResponder {
                mode: PoisonMode::One(bad.clone()),
            })
            .mount(&server)
            .await;
        let containment = ContainmentPolicy {
            persistence_threshold: 1,
            isolation_request_budget: 4,
            ..ContainmentPolicy::default()
        };
        let client = client_for_server(&server, 0, containment).await;
        client.set_failure_recurrence(1);
        let error = client
            .bulk_fetch_profiles(&[good.clone(), bad.clone()])
            .await
            .unwrap_err();
        let TurboError::BlueskyUpstream(error) = error else {
            panic!("expected typed upstream error");
        };
        assert!(matches!(
            error.isolation,
            Some(crate::client::IsolationOutcome::SingletonPoison { .. })
        ));

        let requests_before = server.received_requests().await.unwrap().len();
        assert!(client
            .bulk_fetch_profiles(&[good.clone(), bad.clone()])
            .await
            .is_err());
        let requests = server.received_requests().await.unwrap();
        assert_eq!(requests.len(), requests_before + 3);
        let last_query = requests.last().unwrap().url.query().unwrap_or_default();
        assert!(last_query.contains("did%3Aplc%3Apoison"));
        assert!(!last_query.contains("did%3Aplc%3Agood"));
        assert_eq!(
            client
                .coordination_diagnostics()
                .await
                .profiles
                .completed_result_owners,
            0
        );
    }

    #[tokio::test]
    async fn isolation_stops_early_for_broad_outage() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/app.bsky.feed.getPosts"))
            .respond_with(PoisonResponder {
                mode: PoisonMode::All,
            })
            .mount(&server)
            .await;
        let containment = ContainmentPolicy {
            persistence_threshold: 1,
            isolation_request_budget: 8,
            ..ContainmentPolicy::default()
        };
        let client = client_for_server(&server, 0, containment).await;
        client.set_failure_recurrence(1);
        let uris = [
            "at://did:plc:a/app.bsky.feed.post/one".to_string(),
            "at://did:plc:b/app.bsky.feed.post/two".to_string(),
        ];
        let outcomes = client.bulk_fetch_posts(&uris).await.unwrap();
        assert!(outcomes.iter().all(|outcome| matches!(
            outcome,
            PostFetchOutcome::TemporarilyUnavailable(failure)
                if matches!(failure.isolation, Some(crate::client::IsolationOutcome::BroadOutage { .. }))
        )));
        assert_eq!(server.received_requests().await.unwrap().len(), 3);
    }

    #[tokio::test]
    async fn isolation_honors_strict_probe_budget() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/app.bsky.actor.getProfiles"))
            .respond_with(PoisonResponder {
                mode: PoisonMode::SplitCategories,
            })
            .mount(&server)
            .await;
        let containment = ContainmentPolicy {
            persistence_threshold: 1,
            isolation_request_budget: 2,
            ..ContainmentPolicy::default()
        };
        let client = client_for_server(&server, 0, containment).await;
        client.set_failure_recurrence(1);
        let error = client
            .bulk_fetch_profiles(&[
                "a".to_string(),
                "b".to_string(),
                "y".to_string(),
                "z".to_string(),
            ])
            .await
            .unwrap_err();
        let TurboError::BlueskyUpstream(error) = error else {
            panic!("expected typed upstream error");
        };
        assert_eq!(
            error.isolation,
            Some(crate::client::IsolationOutcome::BudgetExhausted)
        );
        assert_eq!(server.received_requests().await.unwrap().len(), 3);
    }

    #[tokio::test]
    async fn post_outage_returns_ordered_unavailable_without_replay() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/app.bsky.feed.getPosts"))
            .respond_with(PoisonResponder {
                mode: PoisonMode::All,
            })
            .mount(&server)
            .await;
        let containment = ContainmentPolicy {
            min_delay: Duration::from_millis(1),
            max_delay: Duration::from_millis(8),
            persistence_threshold: 2,
            isolation_request_budget: 8,
        };
        let client = client_for_server(&server, 0, containment).await;
        client.set_failure_recurrence(2);
        let uris = [
            "at://did:plc:a/app.bsky.feed.post/one".to_string(),
            "at://did:plc:b/app.bsky.feed.post/two".to_string(),
        ];
        let outcomes = client.bulk_fetch_posts(&uris).await.unwrap();
        assert_eq!(outcomes.len(), uris.len());
        assert!(outcomes
            .iter()
            .all(|outcome| matches!(outcome, PostFetchOutcome::TemporarilyUnavailable(_))));
        assert_eq!(server.received_requests().await.unwrap().len(), 3);
    }

    #[tokio::test]
    async fn test_bluesky_client_creation() {
        let sessions = vec!["session1:::bsky.social".to_string()];
        let client = BlueskyClient::new(sessions, None, 25, 25, 150, 300).unwrap();
        assert_eq!(client.get_session_count().await, 1);
    }

    #[tokio::test]
    async fn test_refresh_sessions() {
        let client =
            BlueskyClient::new(vec!["old_session".to_string()], None, 25, 25, 150, 300).unwrap();
        assert_eq!(client.get_session_count().await, 1);

        client
            .refresh_sessions(
                vec![
                    "new_session1:::bsky.social".to_string(),
                    "new_session2:::bsky.social".to_string(),
                ],
                Some("new_refresh_jwt".to_string()),
                Some("2024-01-01T00:00:00.000Z".to_string()),
            )
            .await;

        assert_eq!(client.get_session_count().await, 2);
    }

    #[tokio::test]
    async fn test_refresh_session_with_fallback_reauthenticates_when_refresh_token_expired() {
        let mock_server = MockServer::start().await;

        Mock::given(method("POST"))
            .and(path("/com.atproto.server.refreshSession"))
            .respond_with(ResponseTemplate::new(401))
            .mount(&mock_server)
            .await;

        let auth_response = crate::client::auth::AuthResponse {
            access_jwt: "new_access_token".to_string(),
            refresh_jwt: "new_refresh_token".to_string(),
            handle: "test.bsky.social".to_string(),
            did: "did:plc:test".to_string(),
            email: None,
            email_confirmed: None,
            active: Some(true),
            expires_at: Some("2026-04-05T00:00:00.000Z".to_string()),
        };

        Mock::given(method("POST"))
            .and(path("/com.atproto.server.createSession"))
            .respond_with(ResponseTemplate::new(200).set_body_json(&auth_response))
            .mount(&mock_server)
            .await;

        let auth_client = Arc::new(
            BlueskyAuthClient::with_api_url(
                "test.bsky.social".to_string(),
                "app-password".to_string(),
                mock_server.uri(),
            )
            .expect("auth client should be created"),
        );

        let client = BlueskyClient::new(
            vec!["stale_access_token".to_string()],
            Some(auth_client),
            25,
            25,
            150,
            300,
        )
        .expect("client should be created");

        client
            .refresh_sessions(
                vec!["stale_access_token".to_string()],
                Some("expired_refresh_token".to_string()),
                Some("2026-04-04T00:00:00.000Z".to_string()),
            )
            .await;

        client
            .refresh_session_with_fallback()
            .await
            .expect("client should re-authenticate after refresh expiry");

        assert_eq!(
            client.get_refresh_jwt().await,
            Some("new_refresh_token".to_string())
        );

        let sessions = client.session_strings.read().await;
        assert_eq!(sessions.as_slice(), ["new_access_token"]);
    }

    #[tokio::test]
    async fn transient_failure_without_recurrence_skips_isolation() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/app.bsky.feed.getPosts"))
            .respond_with(ResponseTemplate::new(502))
            .mount(&server)
            .await;
        let client = client_for_server(&server, 0, ContainmentPolicy::default()).await;
        let uris = [
            "at://did:plc:a/app.bsky.feed.post/one".to_string(),
            "at://did:plc:b/app.bsky.feed.post/two".to_string(),
        ];
        let outcomes = client.bulk_fetch_posts(&uris).await.unwrap();
        assert_eq!(outcomes.len(), uris.len());
        assert!(outcomes
            .iter()
            .all(|o| matches!(o, PostFetchOutcome::TemporarilyUnavailable(_))));
        // No isolation bisection on a transient blip: exactly one request.
        assert_eq!(server.received_requests().await.unwrap().len(), 1);
    }

    #[tokio::test]
    async fn concurrent_batch_fetches_overlap_instead_of_serializing() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/app.bsky.actor.getProfiles"))
            .respond_with(
                ResponseTemplate::new(200)
                    .set_body_json(serde_json::json!({"profiles": []}))
                    .set_delay(Duration::from_millis(250)),
            )
            .mount(&server)
            .await;
        let client = client_for_server(&server, 0, ContainmentPolicy::default()).await;

        // Four requests worth of work across two independent calls. If fetches
        // serialized behind one lock this would take ~1000ms; the unlocked
        // fetch path overlaps them to ~500-750ms.
        let first: Vec<String> = (0..50).map(|i| format!("did:plc:a{i:02}")).collect();
        let second: Vec<String> = (0..50).map(|i| format!("did:plc:b{i:02}")).collect();
        let start = Instant::now();
        let (r1, r2) = tokio::join!(
            client.bulk_fetch_profiles(&first),
            client.bulk_fetch_profiles(&second),
        );
        let elapsed = start.elapsed();
        assert!(r1.is_ok() && r2.is_ok());
        assert!(
            elapsed < Duration::from_millis(1200),
            "concurrent fetches took {elapsed:?}; expected overlap"
        );
    }

    #[tokio::test]
    async fn collector_ownership_settles_after_cache_capacity_and_ttl_churn() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/app.bsky.actor.getProfiles"))
            .respond_with(PoisonResponder {
                mode: PoisonMode::One("never-a-profile".to_string()),
            })
            .mount(&server)
            .await;
        Mock::given(method("GET"))
            .and(path("/app.bsky.feed.getPosts"))
            .respond_with(PoisonResponder {
                mode: PoisonMode::One("never-a-post".to_string()),
            })
            .mount(&server)
            .await;

        let client = Arc::new(client_for_server(&server, 0, ContainmentPolicy::default()).await);
        let start = Instant::now();
        let now = Arc::new(StdMutex::new(start));
        let cache_clock = Arc::clone(&now);
        let cache = TurboCache::new_with_clock(
            2,
            2,
            2,
            Duration::from_secs(300),
            Arc::new(move || *cache_clock.lock().expect("cache clock poisoned")),
        );
        let resolver = CacheMissResolver::new(cache, Arc::clone(&client), Arc::clone(&client));
        let mut rss_samples = Vec::new();

        const CHURN_WAVES: usize = 8;
        for wave in 0..CHURN_WAVES {
            let profiles = (0..3)
                .map(|index| format!("did:plc:wave{wave}-profile{index}"))
                .collect::<Vec<_>>();
            let posts = (0..3)
                .map(|index| format!("at://did:plc:wave{wave}/app.bsky.feed.post/post{index}"))
                .collect::<Vec<_>>();

            resolver.resolve_profiles(&profiles).await.unwrap();
            resolver.resolve_posts(&posts).await.unwrap();
            let settled_wave = client.coordination_diagnostics().await;
            assert_eq!(settled_wave.profiles.pending_keys, 0);
            assert_eq!(settled_wave.profiles.in_flight_keys, 0);
            assert_eq!(settled_wave.profiles.waiters, 0);
            assert_eq!(settled_wave.profiles.retained_identifier_bytes, 0);
            assert!(settled_wave.profiles.key_high_watermark <= settled_wave.profiles.key_capacity);
            assert!(
                settled_wave.profiles.waiter_high_watermark
                    <= settled_wave.profiles.waiter_capacity
            );
            assert_eq!(settled_wave.posts.pending_keys, 0);
            assert_eq!(settled_wave.posts.in_flight_keys, 0);
            assert_eq!(settled_wave.posts.waiters, 0);
            assert_eq!(settled_wave.posts.retained_identifier_bytes, 0);
            assert!(settled_wave.posts.key_high_watermark <= settled_wave.posts.key_capacity);
            assert!(settled_wave.posts.waiter_high_watermark <= settled_wave.posts.waiter_capacity);
            rss_samples.push(
                crate::turbocharger::diagnostics::collect_process_memory_diagnostics().rss_bytes,
            );
            *now.lock().expect("cache clock poisoned") += Duration::from_secs(301);
        }

        eprintln!(
            "hydration_churn_diagnostics={}",
            serde_json::json!({
                "rss_bytes_by_wave": rss_samples,
                "profile_cache_capacity": 2,
                "post_cache_capacity": 2,
                "waves": CHURN_WAVES,
                "unique_identifiers_per_kind_per_wave": 3,
            })
        );

        let snapshot = client.collector_ownership_snapshot().await;
        assert_eq!(snapshot.profiles.pending_keys, 0);
        assert_eq!(snapshot.profiles.in_flight_keys, 0);
        assert_eq!(snapshot.profiles.waiters, 0);
        assert_eq!(snapshot.profiles.retained_identifier_bytes, 0);
        assert_eq!(snapshot.profiles.completed_result_owners, 0);
        assert_eq!(snapshot.posts.pending_keys, 0);
        assert_eq!(snapshot.posts.in_flight_keys, 0);
        assert_eq!(snapshot.posts.waiters, 0);
        assert_eq!(snapshot.posts.retained_identifier_bytes, 0);
        assert_eq!(snapshot.posts.completed_result_owners, 0);
    }

    #[tokio::test]
    async fn concurrent_load_reaches_configured_key_and_waiter_bounds_then_settles() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .respond_with(DelayedEchoResponder {
                delay: Duration::from_millis(100),
            })
            .mount(&server)
            .await;
        let client = Arc::new(bounded_client_for_server(&server, 2, 4, 8).await);

        let profile_groups = [
            vec![
                "did:plc:profile-a".to_string(),
                "did:plc:profile-b".to_string(),
            ],
            vec![
                "did:plc:profile-c".to_string(),
                "did:plc:profile-d".to_string(),
            ],
        ];
        let mut profile_tasks = Vec::new();
        for group in profile_groups {
            for _ in 0..2 {
                let client = Arc::clone(&client);
                let identifiers = group.clone();
                profile_tasks.push(tokio::spawn(async move {
                    client.bulk_fetch_profiles(&identifiers).await
                }));
            }
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
        let active_profiles = client.coordination_diagnostics().await.profiles;
        for task in profile_tasks {
            task.await.unwrap().unwrap();
        }

        let post_groups = [
            vec![
                "at://did:plc:a/app.bsky.feed.post/one".to_string(),
                "at://did:plc:b/app.bsky.feed.post/two".to_string(),
            ],
            vec![
                "at://did:plc:c/app.bsky.feed.post/three".to_string(),
                "at://did:plc:d/app.bsky.feed.post/four".to_string(),
            ],
        ];
        let mut post_tasks = Vec::new();
        for group in post_groups {
            for _ in 0..2 {
                let client = Arc::clone(&client);
                let identifiers = group.clone();
                post_tasks.push(tokio::spawn(async move {
                    client.bulk_fetch_posts(&identifiers).await
                }));
            }
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
        let active_posts = client.coordination_diagnostics().await.posts;
        for task in post_tasks {
            task.await.unwrap().unwrap();
        }

        let settled = client.coordination_diagnostics().await;
        let requests = server.received_requests().await.unwrap();
        let profile_requests = requests
            .iter()
            .filter(|request| request.url.path().ends_with("getProfiles"))
            .count();
        let post_requests = requests
            .iter()
            .filter(|request| request.url.path().ends_with("getPosts"))
            .count();
        assert_eq!(active_profiles.key_high_watermark, 4);
        assert_eq!(active_profiles.waiter_high_watermark, 8);
        assert_eq!(active_posts.key_high_watermark, 4);
        assert_eq!(active_posts.waiter_high_watermark, 8);
        assert_eq!(settled.profiles.pending_keys, 0);
        assert_eq!(settled.profiles.in_flight_keys, 0);
        assert_eq!(settled.profiles.waiters, 0);
        assert_eq!(settled.posts.pending_keys, 0);
        assert_eq!(settled.posts.in_flight_keys, 0);
        assert_eq!(settled.posts.waiters, 0);
        assert_eq!(profile_requests, 2);
        assert_eq!(post_requests, 2);
    }

    #[tokio::test]
    async fn faulted_waves_release_all_coordination_state() {
        let transient_server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/app.bsky.feed.getPosts"))
            .respond_with(ResponseTemplate::new(502))
            .mount(&transient_server)
            .await;
        let transient_client =
            client_for_server(&transient_server, 0, ContainmentPolicy::default()).await;
        let transient_uri = "at://did:plc:a/app.bsky.feed.post/transient".to_string();
        transient_client
            .bulk_fetch_posts(&[transient_uri])
            .await
            .unwrap();

        let permanent_server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/app.bsky.actor.getProfiles"))
            .respond_with(ResponseTemplate::new(404))
            .mount(&permanent_server)
            .await;
        let permanent_client =
            client_for_server(&permanent_server, 0, ContainmentPolicy::default()).await;
        assert!(permanent_client
            .bulk_fetch_profiles(&["did:plc:permanent".to_string()])
            .await
            .is_err());

        let missing_server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/app.bsky.feed.getPosts"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "posts": []
            })))
            .mount(&missing_server)
            .await;
        let missing_client =
            client_for_server(&missing_server, 0, ContainmentPolicy::default()).await;
        let missing_uri = "at://did:plc:b/app.bsky.feed.post/missing".to_string();
        assert!(matches!(
            missing_client
                .bulk_fetch_posts(&[missing_uri])
                .await
                .unwrap()[0],
            PostFetchOutcome::Missing
        ));

        let isolation_server = MockServer::start().await;
        let poison_uri = "at://did:plc:d/app.bsky.feed.post/poison".to_string();
        Mock::given(method("GET"))
            .and(path("/app.bsky.feed.getPosts"))
            .respond_with(PoisonResponder {
                mode: PoisonMode::One(poison_uri.clone()),
            })
            .mount(&isolation_server)
            .await;
        let isolation_client = client_for_server(
            &isolation_server,
            0,
            ContainmentPolicy {
                persistence_threshold: 1,
                isolation_request_budget: 4,
                ..ContainmentPolicy::default()
            },
        )
        .await;
        isolation_client.set_failure_recurrence(1);
        isolation_client
            .bulk_fetch_posts(&[
                "at://did:plc:c/app.bsky.feed.post/good".to_string(),
                poison_uri,
            ])
            .await
            .unwrap();

        for snapshot in [
            transient_client.coordination_diagnostics().await,
            permanent_client.coordination_diagnostics().await,
            missing_client.coordination_diagnostics().await,
            isolation_client.coordination_diagnostics().await,
        ] {
            assert_eq!(snapshot.profiles.pending_keys, 0);
            assert_eq!(snapshot.profiles.in_flight_keys, 0);
            assert_eq!(snapshot.profiles.waiters, 0);
            assert_eq!(snapshot.profiles.retained_identifier_bytes, 0);
            assert_eq!(snapshot.posts.pending_keys, 0);
            assert_eq!(snapshot.posts.in_flight_keys, 0);
            assert_eq!(snapshot.posts.waiters, 0);
            assert_eq!(snapshot.posts.retained_identifier_bytes, 0);
        }
    }

    #[tokio::test]
    async fn claimant_and_waiter_task_abort_release_every_registration() {
        let claimant_server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/app.bsky.actor.getProfiles"))
            .respond_with(DelayedEchoResponder {
                delay: Duration::from_millis(200),
            })
            .mount(&claimant_server)
            .await;
        let claimant_client =
            Arc::new(client_for_server(&claimant_server, 0, ContainmentPolicy::default()).await);
        let claimant_task = {
            let client = Arc::clone(&claimant_client);
            tokio::spawn(async move {
                client
                    .bulk_fetch_profiles(&["did:plc:claimant-abort".to_string()])
                    .await
            })
        };
        for _ in 0..50 {
            if claimant_client
                .coordination_diagnostics()
                .await
                .profiles
                .in_flight_keys
                == 1
            {
                break;
            }
            tokio::time::sleep(Duration::from_millis(2)).await;
        }
        let claimant_peer = {
            let client = Arc::clone(&claimant_client);
            tokio::spawn(async move {
                client
                    .bulk_fetch_profiles(&["did:plc:claimant-abort".to_string()])
                    .await
            })
        };
        for _ in 0..50 {
            if claimant_client
                .coordination_diagnostics()
                .await
                .profiles
                .waiters
                == 2
            {
                break;
            }
            tokio::time::sleep(Duration::from_millis(2)).await;
        }
        claimant_task.abort();
        assert!(claimant_task.await.unwrap_err().is_cancelled());
        let peer_error = claimant_peer.await.unwrap().unwrap_err();
        assert!(peer_error.is_retryable());
        let claimant_settled = claimant_client.coordination_diagnostics().await.profiles;
        assert_eq!(claimant_settled.in_flight_keys, 0);
        assert_eq!(claimant_settled.waiters, 0);
        assert_eq!(claimant_settled.retained_identifier_bytes, 0);
        assert!(claimant_settled.cancellations_total >= 1);

        let waiter_server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/app.bsky.actor.getProfiles"))
            .respond_with(DelayedEchoResponder {
                delay: Duration::from_millis(150),
            })
            .mount(&waiter_server)
            .await;
        let waiter_client =
            Arc::new(client_for_server(&waiter_server, 0, ContainmentPolicy::default()).await);
        let first = {
            let client = Arc::clone(&waiter_client);
            tokio::spawn(async move {
                client
                    .bulk_fetch_profiles(&["did:plc:waiter-abort".to_string()])
                    .await
            })
        };
        for _ in 0..50 {
            if waiter_client
                .coordination_diagnostics()
                .await
                .profiles
                .in_flight_keys
                == 1
            {
                break;
            }
            tokio::time::sleep(Duration::from_millis(2)).await;
        }
        let cancelled_waiter = {
            let client = Arc::clone(&waiter_client);
            tokio::spawn(async move {
                client
                    .bulk_fetch_profiles(&["did:plc:waiter-abort".to_string()])
                    .await
            })
        };
        for _ in 0..50 {
            if waiter_client
                .coordination_diagnostics()
                .await
                .profiles
                .waiters
                == 2
            {
                break;
            }
            tokio::time::sleep(Duration::from_millis(2)).await;
        }
        cancelled_waiter.abort();
        assert!(cancelled_waiter.await.unwrap_err().is_cancelled());
        first.await.unwrap().unwrap();

        let waiter_settled = waiter_client.coordination_diagnostics().await.profiles;
        assert_eq!(waiter_settled.pending_keys, 0);
        assert_eq!(waiter_settled.in_flight_keys, 0);
        assert_eq!(waiter_settled.waiters, 0);
        assert_eq!(waiter_settled.retained_identifier_bytes, 0);
        assert!(waiter_settled.cancellations_total >= 1);
    }

    async fn assert_collectors_settled(client: &BlueskyClient) {
        for _ in 0..200 {
            let diagnostics = client.coordination_diagnostics().await;
            let profiles = (
                diagnostics.profiles.pending_keys,
                diagnostics.profiles.in_flight_keys,
                diagnostics.profiles.waiters,
            );
            let posts = (
                diagnostics.posts.pending_keys,
                diagnostics.posts.in_flight_keys,
                diagnostics.posts.waiters,
            );
            if profiles == (0, 0, 0) && posts == (0, 0, 0) {
                return;
            }
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
        let diagnostics = client.coordination_diagnostics().await;
        panic!(
            "collectors did not settle: profiles {:?} posts {:?}",
            (
                diagnostics.profiles.pending_keys,
                diagnostics.profiles.in_flight_keys,
                diagnostics.profiles.waiters
            ),
            (
                diagnostics.posts.pending_keys,
                diagnostics.posts.in_flight_keys,
                diagnostics.posts.waiters
            )
        );
    }

    async fn parallel_hydrator_over(
        client: &Arc<BlueskyClient>,
    ) -> Hydrator<BlueskyClient, BlueskyClient> {
        Hydrator::new_with_mode(
            TurboCache::new(20, 20),
            Arc::clone(client),
            Arc::clone(client),
            HydrationExecutionMode::Parallel,
        )
    }

    #[tokio::test]
    async fn parallel_required_branch_failure_settles_both_collectors() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/app.bsky.actor.getProfiles"))
            .respond_with(ResponseTemplate::new(500))
            .mount(&server)
            .await;
        Mock::given(method("GET"))
            .and(path("/app.bsky.feed.getPosts"))
            .respond_with(DelayedEchoResponder {
                delay: Duration::from_millis(300),
            })
            .mount(&server)
            .await;
        let client = Arc::new(client_for_server(&server, 0, ContainmentPolicy::default()).await);
        let hydrator = parallel_hydrator_over(&client).await;

        // The post branch has an in-flight claim when the required profile
        // branch fails; both branches run to completion under join semantics
        // and both collectors must settle with the batch failing on the
        // profile error.
        let error = hydrator
            .hydrate_batch(vec![create_reply_message(1, "did:plc:parent", "settle")])
            .await
            .unwrap_err();
        assert!(matches!(error, TurboError::BlueskyUpstream(_)));

        assert_collectors_settled(&client).await;
    }

    #[tokio::test]
    async fn parallel_deadline_cancellation_settles_both_collectors() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/app.bsky.actor.getProfiles"))
            .respond_with(DelayedEchoResponder {
                delay: Duration::from_secs(10),
            })
            .mount(&server)
            .await;
        Mock::given(method("GET"))
            .and(path("/app.bsky.feed.getPosts"))
            .respond_with(DelayedEchoResponder {
                delay: Duration::from_secs(10),
            })
            .mount(&server)
            .await;
        let client = Arc::new(client_for_server(&server, 0, ContainmentPolicy::default()).await);
        let hydrator = parallel_hydrator_over(&client).await;

        // Whole-batch deadline expiry drops the combined future, cancelling
        // both branches simultaneously (and per-branch timeouts share this
        // drop path via the orchestrator's `timeout_at`).
        let result = tokio::time::timeout(
            Duration::from_millis(150),
            hydrator.hydrate_batch(vec![create_reply_message(1, "did:plc:parent", "cancel")]),
        )
        .await;
        assert!(result.is_err(), "batch should hit the deadline");

        assert_collectors_settled(&client).await;
    }

    #[tokio::test]
    async fn optional_post_branch_cancellation_settles_post_collector() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/app.bsky.feed.getPosts"))
            .respond_with(DelayedEchoResponder {
                delay: Duration::from_secs(5),
            })
            .mount(&server)
            .await;
        let client = Arc::new(client_for_server(&server, 0, ContainmentPolicy::default()).await);

        let task = {
            let client = Arc::clone(&client);
            tokio::spawn(async move {
                client
                    .bulk_fetch_posts(&[
                        "at://did:plc:parent/app.bsky.feed.post/aborted".to_string()
                    ])
                    .await
            })
        };
        for _ in 0..200 {
            if client.coordination_diagnostics().await.posts.in_flight_keys == 1 {
                break;
            }
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
        task.abort();
        assert!(task.await.unwrap_err().is_cancelled());

        assert_collectors_settled(&client).await;
    }

    #[tokio::test]
    async fn demand_aware_partial_claim_reports_fill_counters_correctly() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/app.bsky.actor.getProfiles"))
            .respond_with(PoisonResponder {
                mode: PoisonMode::One("never-match".to_string()),
            })
            .mount(&server)
            .await;
        // The window is far longer than the test run: only the demand-aware
        // immediate flush can serve the 5-key tail chunk of a 30-DID fetch.
        let client = BlueskyClient::new_with_policies(
            vec!["test-session".to_string()],
            None,
            25,
            25,
            600_000,
            600_000,
            fast_policy(0),
            ContainmentPolicy::default(),
        )
        .unwrap();
        client.set_api_base_url_for_test(server.uri()).await;

        let dids = (0..30)
            .map(|index| format!("did:plc:fill{index:02}"))
            .collect::<Vec<_>>();
        let profiles = client.bulk_fetch_profiles(&dids).await.unwrap();
        assert_eq!(profiles.len(), 30);
        assert!(profiles.iter().all(|profile| profile.is_some()));

        let diagnostics = client.fetch_diagnostics().await;
        assert_eq!(
            diagnostics.profiles.requests_total, 2,
            "one full 25-item claim plus the immediate 5-item tail claim"
        );
        assert_eq!(diagnostics.profiles.items_total, 30);
        assert_eq!(
            client
                .profile_batch_collector
                .read()
                .await
                .batches_partial
                .load(Ordering::Relaxed),
            1,
            "only the tail claim is partial"
        );

        assert_collectors_settled(&client).await;
    }

    #[tokio::test]
    async fn nine_permits_stay_below_the_shared_request_quota() {
        let server = MockServer::start().await;
        for endpoint in ["/app.bsky.actor.getProfiles", "/app.bsky.feed.getPosts"] {
            Mock::given(method("GET"))
                .and(path(endpoint))
                .respond_with(DelayedEchoResponder {
                    delay: Duration::from_millis(250),
                })
                .mount(&server)
                .await;
        }
        let client = Arc::new(client_for_server(&server, 0, ContainmentPolicy::default()).await);

        // Nine permits' worth of concurrent batches, each issuing one profile
        // and one post request through the shared 10 requests/second limiter.
        let started = std::time::Instant::now();
        let mut tasks = Vec::new();
        for index in 0..9 {
            let client = Arc::clone(&client);
            tasks.push(tokio::spawn(async move {
                client
                    .bulk_fetch_profiles(&[format!("did:plc:quota{index}")])
                    .await
                    .unwrap();
                client
                    .bulk_fetch_posts(&[format!("at://did:plc:quota{index}/app.bsky.feed.post/x")])
                    .await
                    .unwrap();
            }));
        }
        for task in tasks {
            task.await.unwrap();
        }
        let elapsed = started.elapsed();

        let diagnostics = client.fetch_diagnostics().await;
        let requests = diagnostics.profiles.requests_total + diagnostics.posts.requests_total;
        assert_eq!(requests, 18);
        assert_eq!(
            diagnostics.profiles.errors_rate_limited + diagnostics.posts.errors_rate_limited,
            0,
            "the limiter must pace requests instead of producing rate-limit errors"
        );

        // The shared limiter (10 requests/second, burst 1) spaces the 18
        // request starts at least 100ms apart; with the 250ms response delay
        // the run cannot finish before ~1.95s, so the observed combined rate
        // stays below the 10 requests/second quota with margin.
        let minimum_paced_seconds = 17.0 / 10.0 + 0.25;
        assert!(
            elapsed.as_secs_f64() >= minimum_paced_seconds,
            "rate limiter did not pace the shared quota: {elapsed:?}"
        );
        let request_rate = requests as f64 / elapsed.as_secs_f64();
        assert!(
            request_rate < 10.0,
            "combined upstream request rate {request_rate:.2}/s exceeds the quota"
        );

        assert_collectors_settled(&client).await;
    }

    #[tokio::test]
    async fn parallel_hydration_substage_timings_separate_branch_contributions() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/app.bsky.actor.getProfiles"))
            .respond_with(DelayedEchoResponder {
                delay: Duration::from_millis(120),
            })
            .mount(&server)
            .await;
        Mock::given(method("GET"))
            .and(path("/app.bsky.feed.getPosts"))
            .respond_with(DelayedEchoResponder {
                delay: Duration::from_millis(40),
            })
            .mount(&server)
            .await;
        let client = Arc::new(client_for_server(&server, 0, ContainmentPolicy::default()).await);
        let hydrator = parallel_hydrator_over(&client).await;

        hydrator
            .hydrate_batch(vec![create_reply_message(1, "did:plc:parent", "substage")])
            .await
            .unwrap();

        let diagnostics = client.fetch_diagnostics().await;
        let substage = |kind: &BlueskyFetchKindDiagnostics, wanted: HydrationSubstage| {
            kind.substage_timings()
                .into_iter()
                .find(|timing| timing.substage == wanted)
                .unwrap_or_else(|| panic!("missing {wanted:?} substage timing"))
        };
        let profile_http = substage(&diagnostics.profiles, HydrationSubstage::UpstreamHttp);
        let post_http = substage(&diagnostics.posts, HydrationSubstage::UpstreamHttp);
        assert!(profile_http.sample_count >= 1);
        assert!(
            profile_http.duration_ns_total >= 120_u64 * 1_000_000,
            "profile branch upstream contribution must be measured separately"
        );
        assert!(post_http.sample_count >= 1);
        assert!(
            post_http.duration_ns_total >= 40_u64 * 1_000_000,
            "post branch upstream contribution must be measured separately"
        );
    }
}
