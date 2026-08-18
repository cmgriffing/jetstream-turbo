use crate::client::resilience::{
    bounded_body_excerpt, bounded_exponential_jitter, retry_after_delta,
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
    ) -> impl std::future::Future<Output = TurboResult<Vec<Option<BlueskyProfile>>>> + Send;
}

pub trait PostFetcher {
    fn bulk_fetch_posts(
        &self,
        uris: &[String],
    ) -> impl std::future::Future<Output = TurboResult<Vec<Option<BlueskyPost>>>> + Send;
}

const REQUESTS_PER_SECOND_MS: u64 = 1000 / 10;

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

struct ProfileBatchCollector {
    config: BatchConfig,
    pending: Vec<String>,
    last_flush: Instant,
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
    batches_total: AtomicU64,
    batches_partial: AtomicU64,
    isolation_cache: HashMap<String, Option<BlueskyProfile>>,
}

struct PostBatchCollector {
    config: BatchConfig,
    pending: Vec<String>,
    last_flush: Instant,
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
    batches_total: AtomicU64,
    batches_partial: AtomicU64,
    isolation_cache: HashMap<String, Option<BlueskyPost>>,
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
    transient: bool,
) -> TurboError {
    UpstreamHttpError {
        operation,
        status: status.map(|status| status.as_u16()),
        category,
        body_excerpt: body.map(bounded_body_excerpt),
        attempts,
        transient,
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

fn record_request_exhaustion(
    operation: BlueskyOperation,
    category: UpstreamFailureCategory,
    attempts: u32,
    request_fingerprint: &str,
) {
    metrics::counter!(
        "bluesky_request_exhaustions_total",
        "operation" => operation.as_str(),
        "category" => category.as_str(),
        "attempts" => attempts.to_string(),
    )
    .increment(1);
    error!(
        operation = operation.as_str(),
        category = category.as_str(),
        attempts,
        request_fingerprint,
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
        )));

        let post_batch_collector = Arc::new(RwLock::new(PostBatchCollector::new(
            BatchConfig {
                batch_size: post_batch_size,
                wait_ms: post_batch_wait_ms,
            },
            collector_deps,
        )));

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
}

impl ProfileFetcher for BlueskyClient {
    #[instrument(name = "bulk_fetch_profiles", skip(self, dids), fields(count))]
    async fn bulk_fetch_profiles(
        &self,
        dids: &[String],
    ) -> TurboResult<Vec<Option<BlueskyProfile>>> {
        tracing::Span::current().record("count", dids.len());

        if dids.is_empty() {
            return Ok(vec![]);
        }

        let mut collector = self.profile_batch_collector.write().await;
        let profiles = collector.add_and_fetch(dids.to_vec()).await?;
        collector.log_partial_percentage();

        Ok(profiles)
    }
}

impl PostFetcher for BlueskyClient {
    #[instrument(
        name = "bulk_fetch_posts",
        skip(self, uris),
        fields(count, valid_count)
    )]
    async fn bulk_fetch_posts(&self, uris: &[String]) -> TurboResult<Vec<Option<BlueskyPost>>> {
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

        let mut collector = self.post_batch_collector.write().await;
        let posts = collector.add_and_fetch(valid_uris).await?;
        collector.log_partial_percentage();

        Ok(posts)
    }
}

impl ProfileBatchCollector {
    fn new(config: BatchConfig, deps: BatchCollectorDeps) -> Self {
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
        Self {
            config,
            pending: Vec::new(),
            last_flush: Instant::now(),
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
            batches_total: AtomicU64::new(0),
            batches_partial: AtomicU64::new(0),
            isolation_cache: HashMap::new(),
        }
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

    async fn fetch_batch_with_retry(
        &self,
        dids: &[String],
    ) -> TurboResult<Vec<Option<BlueskyProfile>>> {
        let url = format!("{}/app.bsky.actor.getProfiles", self.api_base_url);
        let mut session_string = self.get_session_string().await?;
        let mut attempts = 0u32;
        let operation = BlueskyOperation::Profiles;
        let request_fingerprint = stable_identifier_fingerprint(dids);

        loop {
            attempts = attempts.saturating_add(1);
            self.rate_limiter.until_ready().await;

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
                        let body = resp.text().await?;
                        let profiles_response: GetProfilesResponse = serde_json::from_str(&body)
                            .map_err(|e| {
                                error!(operation = operation.as_str(), error = %e, "Failed to decode Bluesky response");
                                TurboError::InvalidApiResponse(format!("Failed to decode: {e}"))
                            })?;
                        let mut result = vec![None; dids.len()];
                        for (i, profile) in profiles_response.profiles.into_iter().enumerate() {
                            if i < result.len() {
                                result[i] = Some(profile.into());
                            }
                        }
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
                                false,
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
                                    false,
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
                            false,
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
                                true,
                            );
                            record_request_exhaustion(
                                operation,
                                category,
                                attempts,
                                &request_fingerprint,
                            );
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
                            false,
                        ));
                    }
                },
                Err(_error) => {
                    let category = UpstreamFailureCategory::Transport;
                    if attempts > self.retry_policy.max_retries {
                        record_request_exhaustion(
                            operation,
                            category,
                            attempts,
                            &request_fingerprint,
                        );
                        return Err(upstream_error(
                            operation, dids, None, category, None, attempts, true,
                        ));
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

    async fn fetch_batch(&mut self, dids: &[String]) -> TurboResult<Vec<Option<BlueskyProfile>>> {
        let unresolved = dids
            .iter()
            .filter(|did| !self.isolation_cache.contains_key(*did))
            .cloned()
            .collect::<Vec<_>>();
        if unresolved.is_empty() {
            return Ok(dids
                .iter()
                .map(|did| self.isolation_cache.get(did).cloned().unwrap_or(None))
                .collect());
        }

        match self.fetch_batch_with_retry(&unresolved).await {
            Ok(fetched) => {
                let by_identifier = unresolved
                    .into_iter()
                    .zip(fetched)
                    .collect::<HashMap<_, _>>();
                Ok(dids
                    .iter()
                    .map(|did| {
                        self.isolation_cache
                            .get(did)
                            .cloned()
                            .unwrap_or_else(|| by_identifier.get(did).cloned().unwrap_or(None))
                    })
                    .collect())
            }
            Err(error)
                if transient_upstream_category(&error).is_some()
                    && unresolved.len() > 1
                    && self.isolation_recurrence.load(Ordering::Acquire)
                        >= self.containment_policy.persistence_threshold =>
            {
                self.isolate_profiles(unresolved, error).await
            }
            Err(error) => Err(error),
        }
    }

    async fn isolate_profiles(
        &mut self,
        mut identifiers: Vec<String>,
        root_error: TurboError,
    ) -> TurboResult<Vec<Option<BlueskyProfile>>> {
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

        info!(
            operation = BlueskyOperation::Profiles.as_str(),
            request_fingerprint = %stable_identifier_fingerprint(&identifiers),
            request_budget = remaining_budget,
            "Starting bounded Bluesky request isolation"
        );

        for half in first_halves {
            if remaining_budget == 0 {
                return Err(with_isolation_outcome(
                    last_error,
                    crate::client::IsolationOutcome::BudgetExhausted,
                ));
            }
            remaining_budget -= 1;
            match self.fetch_batch_with_retry(&half).await {
                Ok(results) => {
                    self.isolation_cache.extend(half.into_iter().zip(results));
                }
                Err(error) => {
                    first_categories.push(transient_upstream_category(&error));
                    last_error = error;
                    failures.push_back(half);
                }
            }
        }

        if failures.len() == 2
            && first_categories[0].is_some()
            && first_categories[0] == first_categories[1]
        {
            return Err(with_isolation_outcome(
                last_error,
                crate::client::IsolationOutcome::BroadOutage {
                    category: first_categories[0].expect("category checked"),
                },
            ));
        }

        while let Some(failing) = failures.pop_front() {
            if failing.len() == 1 {
                return Err(with_isolation_outcome(
                    last_error,
                    crate::client::IsolationOutcome::SingletonPoison {
                        request_fingerprint: stable_identifier_fingerprint(&failing),
                    },
                ));
            }
            let midpoint = failing.len() / 2;
            for subset in [failing[..midpoint].to_vec(), failing[midpoint..].to_vec()] {
                if remaining_budget == 0 {
                    return Err(with_isolation_outcome(
                        last_error,
                        crate::client::IsolationOutcome::BudgetExhausted,
                    ));
                }
                remaining_budget -= 1;
                match self.fetch_batch_with_retry(&subset).await {
                    Ok(results) => self.isolation_cache.extend(subset.into_iter().zip(results)),
                    Err(error) => {
                        last_error = error;
                        failures.push_back(subset);
                    }
                }
            }
        }

        Ok(requested
            .iter()
            .map(|did| self.isolation_cache.get(did).cloned().unwrap_or(None))
            .collect())
    }

    pub async fn add_and_fetch(
        &mut self,
        dids: Vec<String>,
    ) -> TurboResult<Vec<Option<BlueskyProfile>>> {
        let mut results = Vec::new();
        let mut remaining: Vec<String> = dids.into_iter().collect();

        while !remaining.is_empty() {
            self.pending.append(&mut remaining);

            while self.pending.len() >= self.config.batch_size {
                let batch: Vec<String> = self.pending.drain(..self.config.batch_size).collect();
                self.batches_total.fetch_add(1, Ordering::Relaxed);
                let batch_len = batch.len();
                if batch_len < self.config.batch_size {
                    self.batches_partial.fetch_add(1, Ordering::Relaxed);
                }
                let pct = (batch_len as f64 / self.config.batch_size as f64) * 100.0;
                info!(
                    "Profile batch capacity: {}/{} ({:.0}%)",
                    batch_len, self.config.batch_size, pct
                );

                let batch_results = self.fetch_batch(&batch).await?;
                results.extend(batch_results);
                self.last_flush = Instant::now();
            }

            if !self.pending.is_empty()
                && self.last_flush.elapsed() >= Duration::from_millis(self.config.wait_ms)
            {
                let batch: Vec<String> = std::mem::take(&mut self.pending);
                self.batches_total.fetch_add(1, Ordering::Relaxed);
                let batch_len = batch.len();
                if batch_len < self.config.batch_size {
                    self.batches_partial.fetch_add(1, Ordering::Relaxed);
                }
                let pct = (batch_len as f64 / self.config.batch_size as f64) * 100.0;
                info!(
                    "Profile batch capacity: {}/{} ({:.0}%)",
                    batch_len, self.config.batch_size, pct
                );

                let batch_results = self.fetch_batch(&batch).await?;
                results.extend(batch_results);
                self.last_flush = Instant::now();
            }

            if self.pending.is_empty() {
                break;
            }

            tokio::time::sleep(Duration::from_millis(10)).await;
        }

        if !self.pending.is_empty() {
            let batch: Vec<String> = std::mem::take(&mut self.pending);
            self.batches_total.fetch_add(1, Ordering::Relaxed);
            let batch_len = batch.len();
            if batch_len < self.config.batch_size {
                self.batches_partial.fetch_add(1, Ordering::Relaxed);
            }
            let pct = (batch_len as f64 / self.config.batch_size as f64) * 100.0;
            info!(
                "Profile batch capacity: {}/{} ({:.0}%)",
                batch_len, self.config.batch_size, pct
            );

            let batch_results = self.fetch_batch(&batch).await?;
            results.extend(batch_results);
            self.last_flush = Instant::now();
        }

        Ok(results)
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
}

impl PostBatchCollector {
    fn new(config: BatchConfig, deps: BatchCollectorDeps) -> Self {
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
        Self {
            config,
            pending: Vec::new(),
            last_flush: Instant::now(),
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
            batches_total: AtomicU64::new(0),
            batches_partial: AtomicU64::new(0),
            isolation_cache: HashMap::new(),
        }
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
    ) -> BlueskyPost {
        BlueskyPost {
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
        }
    }

    async fn fetch_batch_with_retry(
        &self,
        uris: &[String],
    ) -> TurboResult<Vec<Option<BlueskyPost>>> {
        let url = format!("{}/app.bsky.feed.getPosts", self.api_base_url);
        let mut session_string = self.get_session_string().await?;
        let mut attempts = 0u32;
        let operation = BlueskyOperation::Posts;
        let request_fingerprint = stable_identifier_fingerprint(uris);

        loop {
            attempts = attempts.saturating_add(1);
            self.rate_limiter.until_ready().await;

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
                                false,
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
                                    false,
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
                            false,
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
                                true,
                            );
                            record_request_exhaustion(
                                operation,
                                category,
                                attempts,
                                &request_fingerprint,
                            );
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
                            false,
                        ));
                    }
                },
                Err(_error) => {
                    let category = UpstreamFailureCategory::Transport;
                    if attempts > self.retry_policy.max_retries {
                        record_request_exhaustion(
                            operation,
                            category,
                            attempts,
                            &request_fingerprint,
                        );
                        return Err(upstream_error(
                            operation, uris, None, category, None, attempts, true,
                        ));
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

    async fn fetch_batch(&mut self, uris: &[String]) -> TurboResult<Vec<Option<BlueskyPost>>> {
        let unresolved = uris
            .iter()
            .filter(|uri| !self.isolation_cache.contains_key(*uri))
            .cloned()
            .collect::<Vec<_>>();
        if unresolved.is_empty() {
            return Ok(uris
                .iter()
                .map(|uri| self.isolation_cache.get(uri).cloned().unwrap_or(None))
                .collect());
        }

        match self.fetch_batch_with_retry(&unresolved).await {
            Ok(fetched) => {
                let by_identifier = unresolved
                    .into_iter()
                    .zip(fetched)
                    .collect::<HashMap<_, _>>();
                Ok(uris
                    .iter()
                    .map(|uri| {
                        self.isolation_cache
                            .get(uri)
                            .cloned()
                            .unwrap_or_else(|| by_identifier.get(uri).cloned().unwrap_or(None))
                    })
                    .collect())
            }
            Err(error)
                if transient_upstream_category(&error).is_some()
                    && unresolved.len() > 1
                    && self.isolation_recurrence.load(Ordering::Acquire)
                        >= self.containment_policy.persistence_threshold =>
            {
                self.isolate_posts(unresolved, error).await
            }
            Err(error) => Err(error),
        }
    }

    async fn isolate_posts(
        &mut self,
        mut identifiers: Vec<String>,
        root_error: TurboError,
    ) -> TurboResult<Vec<Option<BlueskyPost>>> {
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

        info!(
            operation = BlueskyOperation::Posts.as_str(),
            request_fingerprint = %stable_identifier_fingerprint(&identifiers),
            request_budget = remaining_budget,
            "Starting bounded Bluesky request isolation"
        );

        for half in first_halves {
            if remaining_budget == 0 {
                return Err(with_isolation_outcome(
                    last_error,
                    crate::client::IsolationOutcome::BudgetExhausted,
                ));
            }
            remaining_budget -= 1;
            match self.fetch_batch_with_retry(&half).await {
                Ok(results) => self.isolation_cache.extend(half.into_iter().zip(results)),
                Err(error) => {
                    first_categories.push(transient_upstream_category(&error));
                    last_error = error;
                    failures.push_back(half);
                }
            }
        }

        if failures.len() == 2
            && first_categories[0].is_some()
            && first_categories[0] == first_categories[1]
        {
            return Err(with_isolation_outcome(
                last_error,
                crate::client::IsolationOutcome::BroadOutage {
                    category: first_categories[0].expect("category checked"),
                },
            ));
        }

        while let Some(failing) = failures.pop_front() {
            if failing.len() == 1 {
                return Err(with_isolation_outcome(
                    last_error,
                    crate::client::IsolationOutcome::SingletonPoison {
                        request_fingerprint: stable_identifier_fingerprint(&failing),
                    },
                ));
            }
            let midpoint = failing.len() / 2;
            for subset in [failing[..midpoint].to_vec(), failing[midpoint..].to_vec()] {
                if remaining_budget == 0 {
                    return Err(with_isolation_outcome(
                        last_error,
                        crate::client::IsolationOutcome::BudgetExhausted,
                    ));
                }
                remaining_budget -= 1;
                match self.fetch_batch_with_retry(&subset).await {
                    Ok(results) => self.isolation_cache.extend(subset.into_iter().zip(results)),
                    Err(error) => {
                        last_error = error;
                        failures.push_back(subset);
                    }
                }
            }
        }

        Ok(requested
            .iter()
            .map(|uri| self.isolation_cache.get(uri).cloned().unwrap_or(None))
            .collect())
    }

    pub async fn add_and_fetch(
        &mut self,
        uris: Vec<String>,
    ) -> TurboResult<Vec<Option<BlueskyPost>>> {
        let mut results = Vec::new();
        let mut remaining: Vec<String> = uris.into_iter().collect();

        while !remaining.is_empty() {
            self.pending.append(&mut remaining);

            while self.pending.len() >= self.config.batch_size {
                let batch: Vec<String> = self.pending.drain(..self.config.batch_size).collect();
                self.batches_total.fetch_add(1, Ordering::Relaxed);
                let batch_len = batch.len();
                if batch_len < self.config.batch_size {
                    self.batches_partial.fetch_add(1, Ordering::Relaxed);
                }
                let pct = (batch_len as f64 / self.config.batch_size as f64) * 100.0;
                info!(
                    "Post batch capacity: {}/{} ({:.0}%)",
                    batch_len, self.config.batch_size, pct
                );

                let batch_results = self.fetch_batch(&batch).await?;
                results.extend(batch_results);
                self.last_flush = Instant::now();
            }

            if !self.pending.is_empty()
                && self.last_flush.elapsed() >= Duration::from_millis(self.config.wait_ms)
            {
                let batch: Vec<String> = std::mem::take(&mut self.pending);
                self.batches_total.fetch_add(1, Ordering::Relaxed);
                let batch_len = batch.len();
                if batch_len < self.config.batch_size {
                    self.batches_partial.fetch_add(1, Ordering::Relaxed);
                }
                let pct = (batch_len as f64 / self.config.batch_size as f64) * 100.0;
                info!(
                    "Post batch capacity: {}/{} ({:.0}%)",
                    batch_len, self.config.batch_size, pct
                );

                let batch_results = self.fetch_batch(&batch).await?;
                results.extend(batch_results);
                self.last_flush = Instant::now();
            }

            if self.pending.is_empty() {
                break;
            }

            tokio::time::sleep(Duration::from_millis(10)).await;
        }

        if !self.pending.is_empty() {
            let batch: Vec<String> = std::mem::take(&mut self.pending);
            self.batches_total.fetch_add(1, Ordering::Relaxed);
            let batch_len = batch.len();
            if batch_len < self.config.batch_size {
                self.batches_partial.fetch_add(1, Ordering::Relaxed);
            }
            let pct = (batch_len as f64 / self.config.batch_size as f64) * 100.0;
            info!(
                "Post batch capacity: {}/{} ({:.0}%)",
                batch_len, self.config.batch_size, pct
            );

            let batch_results = self.fetch_batch(&batch).await?;
            results.extend(batch_results);
            self.last_flush = Instant::now();
        }

        Ok(results)
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
}

#[cfg(test)]
mod tests {
    use super::*;
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
        assert!(result[0].is_some());
        assert_eq!(calls.load(Ordering::SeqCst), 2);
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
    async fn profile_isolation_identifies_singleton_and_reuses_successful_half() {
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
        assert_eq!(requests.len(), requests_before + 1);
        let last_query = requests.last().unwrap().url.query().unwrap_or_default();
        assert!(last_query.contains("did%3Aplc%3Apoison"));
        assert!(!last_query.contains("did%3Aplc%3Agood"));
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
        let error = client.bulk_fetch_posts(&uris).await.unwrap_err();
        let TurboError::BlueskyUpstream(error) = error else {
            panic!("expected typed upstream error");
        };
        assert!(matches!(
            error.isolation,
            Some(crate::client::IsolationOutcome::BroadOutage { .. })
        ));
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
}
