use crate::client::{ContainmentPolicy, IsolationOutcome};
use crate::models::errors::TurboError;
use serde::Serialize;
use std::sync::Mutex;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub struct FailureContainmentSnapshot {
    pub active: bool,
    pub persistent: bool,
    pub retryable: bool,
    pub fingerprint: Option<String>,
    pub operation: Option<String>,
    pub category: Option<String>,
    pub recurrence: u32,
    pub first_occurrence_unix_ms: Option<u64>,
    pub last_occurrence_unix_ms: Option<u64>,
    pub current_delay_ms: Option<u64>,
    pub blocked_checkpoint_ordinal: Option<u64>,
    pub isolation: Option<IsolationOutcome>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RecoveryDecision {
    pub delay: Duration,
    pub recurrence: u32,
    pub persistent: bool,
    pub retryable: bool,
    pub log_terminal: bool,
}

#[derive(Debug)]
struct FailureState {
    snapshot: FailureContainmentSnapshot,
}

#[derive(Debug)]
pub struct FailureSupervisor {
    policy: ContainmentPolicy,
    state: Mutex<FailureState>,
}

impl FailureSupervisor {
    pub fn new(policy: ContainmentPolicy) -> Self {
        Self {
            policy,
            state: Mutex::new(FailureState {
                snapshot: FailureContainmentSnapshot::default(),
            }),
        }
    }

    pub fn record_failure(
        &self,
        error: &TurboError,
        checkpoint_ordinal: Option<u64>,
    ) -> RecoveryDecision {
        let descriptor = FailureDescriptor::from_error(error);
        let now = unix_ms();
        let mut state = self
            .state
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let previous = &state.snapshot;
        let checkpoint_advanced = previous
            .blocked_checkpoint_ordinal
            .zip(checkpoint_ordinal)
            .is_some_and(|(blocked, current)| current > blocked);
        let same_incident = previous.active
            && !checkpoint_advanced
            && previous.fingerprint.as_deref() == Some(descriptor.fingerprint.as_str());
        let recurrence = if same_incident {
            previous.recurrence.saturating_add(1)
        } else {
            1
        };
        let delay = if descriptor.retryable {
            recovery_delay(self.policy, recurrence, &descriptor.fingerprint)
        } else {
            self.policy.max_delay
        };
        let persistent = recurrence >= self.policy.persistence_threshold;
        let reached_cap = delay >= self.policy.max_delay;
        let log_terminal = recurrence == 1
            || recurrence == self.policy.persistence_threshold
            || (reached_cap && recurrence % 10 == 0);

        state.snapshot = FailureContainmentSnapshot {
            active: true,
            persistent,
            retryable: descriptor.retryable,
            fingerprint: Some(descriptor.fingerprint),
            operation: descriptor.operation,
            category: Some(descriptor.category),
            recurrence,
            first_occurrence_unix_ms: if same_incident {
                previous.first_occurrence_unix_ms
            } else {
                Some(now)
            },
            last_occurrence_unix_ms: Some(now),
            current_delay_ms: Some(duration_ms(delay)),
            blocked_checkpoint_ordinal: Some(checkpoint_ordinal.unwrap_or(0)),
            isolation: descriptor.isolation,
        };

        RecoveryDecision {
            delay,
            recurrence,
            persistent,
            retryable: descriptor.retryable,
            log_terminal,
        }
    }

    /// Clears containment only after the durable completion frontier moves past
    /// the checkpoint recorded when the incident blocked the pipeline.
    pub fn observe_checkpoint(
        &self,
        checkpoint_ordinal: u64,
    ) -> Option<FailureContainmentSnapshot> {
        let mut state = self
            .state
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let blocked = state.snapshot.blocked_checkpoint_ordinal?;
        if !state.snapshot.active || checkpoint_ordinal <= blocked {
            return None;
        }
        let recovered = state.snapshot.clone();
        state.snapshot = FailureContainmentSnapshot::default();
        Some(recovered)
    }

    pub fn snapshot(&self) -> FailureContainmentSnapshot {
        self.state
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .snapshot
            .clone()
    }
}

#[derive(Debug)]
struct FailureDescriptor {
    fingerprint: String,
    operation: Option<String>,
    category: String,
    retryable: bool,
    isolation: Option<IsolationOutcome>,
}

impl FailureDescriptor {
    fn from_error(error: &TurboError) -> Self {
        match error {
            TurboError::BlueskyUpstream(upstream) => Self {
                fingerprint: upstream.failure_fingerprint(),
                operation: Some(upstream.operation.as_str().to_string()),
                category: upstream.category.as_str().to_string(),
                retryable: upstream.transient,
                isolation: upstream.isolation.clone(),
            },
            other => {
                let category = error_category(other);
                Self {
                    fingerprint: format!("pipeline:{category}"),
                    operation: None,
                    category: category.to_string(),
                    retryable: other.is_retryable(),
                    isolation: None,
                }
            }
        }
    }
}

fn error_category(error: &TurboError) -> &'static str {
    match error {
        TurboError::JetstreamConnection(_) | TurboError::WebSocketConnection(_) => "connection",
        TurboError::HttpRequest(_) => "http_transport",
        TurboError::RateLimitExceeded => "rate_limited",
        TurboError::InvalidApiResponse(_) => "invalid_api_response",
        TurboError::BlueskyUpstream(_) => "bluesky_upstream",
        TurboError::Configuration(_) | TurboError::MissingEnvVar(_) => "configuration",
        TurboError::Database(_) => "database",
        TurboError::RedisOperation(_) => "publication",
        TurboError::JsonSerialization(_) | TurboError::JsonDeserialization(_) => "serialization",
        TurboError::CacheOperation(_) => "cache",
        TurboError::InvalidMessage(_) => "invalid_message",
        TurboError::HydrationFailed(_) => "hydration",
        TurboError::RotationFailed(_) => "storage_rotation",
        TurboError::Io(_) => "io",
        TurboError::TaskJoin(_) => "task_join",
        TurboError::Timeout(_) | TurboError::BatchStageTimeout { .. } => "timeout",
        TurboError::Internal(_) => "internal",
        TurboError::NotFound(_) => "not_found",
        TurboError::PermissionDenied(_) => "permission",
        TurboError::ExpiredToken(_) => "authentication",
    }
}

fn recovery_delay(policy: ContainmentPolicy, recurrence: u32, fingerprint: &str) -> Duration {
    if recurrence <= 1 {
        return policy.min_delay;
    }
    let exponent = recurrence.saturating_sub(1).min(31);
    let exponential = policy
        .min_delay
        .saturating_mul(1u32 << exponent)
        .min(policy.max_delay);
    let entropy = fingerprint.bytes().fold(recurrence as u64, |state, byte| {
        state.wrapping_mul(1099511628211) ^ byte as u64
    });
    let millis = exponential.as_millis().min(u64::MAX as u128) as u64;
    let jitter_permille = 1_000 + entropy % 251;
    Duration::from_millis(millis.saturating_mul(jitter_permille) / 1_000)
        .clamp(policy.min_delay, policy.max_delay)
}

fn duration_ms(duration: Duration) -> u64 {
    duration.as_millis().min(u64::MAX as u128) as u64
}

fn unix_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis()
        .min(u64::MAX as u128) as u64
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::client::{BlueskyOperation, UpstreamFailureCategory, UpstreamHttpError};

    fn policy() -> ContainmentPolicy {
        ContainmentPolicy {
            min_delay: Duration::from_secs(1),
            max_delay: Duration::from_secs(4),
            persistence_threshold: 2,
            isolation_request_budget: 4,
        }
    }

    fn failure(hash: &str) -> TurboError {
        UpstreamHttpError {
            operation: BlueskyOperation::Profiles,
            status: Some(502),
            category: UpstreamFailureCategory::ServerError,
            body_excerpt: None,
            attempts: 2,
            transient: true,
            request_fingerprint: hash.to_string(),
            isolation: None,
        }
        .into()
    }

    #[test]
    fn identical_failures_grow_and_cap_without_connection_reset() {
        let supervisor = FailureSupervisor::new(policy());
        let first = supervisor.record_failure(&failure("same"), Some(10));
        let second = supervisor.record_failure(&failure("same"), Some(10));
        let third = supervisor.record_failure(&failure("same"), Some(10));
        assert_eq!(first.recurrence, 1);
        assert_eq!(second.recurrence, 2);
        assert!(second.delay > first.delay);
        assert_eq!(third.delay, Duration::from_secs(4));
        assert!(second.persistent);
    }

    #[test]
    fn distinct_fingerprint_starts_a_new_sequence() {
        let supervisor = FailureSupervisor::new(policy());
        supervisor.record_failure(&failure("one"), Some(10));
        let decision = supervisor.record_failure(&failure("two"), Some(10));
        assert_eq!(decision.recurrence, 1);
    }

    #[test]
    fn only_checkpoint_progress_clears_containment() {
        let supervisor = FailureSupervisor::new(policy());
        supervisor.record_failure(&failure("same"), Some(10));
        assert!(supervisor.observe_checkpoint(10).is_none());
        assert!(supervisor.snapshot().active);
        assert!(supervisor.observe_checkpoint(11).is_some());
        assert!(!supervisor.snapshot().active);
        let decision = supervisor.record_failure(&failure("same"), Some(11));
        assert_eq!(decision.recurrence, 1);
    }
}
