use crate::client::{ContainmentPolicy, IsolationOutcome};
use crate::models::{
    errors::TurboError,
    recovery::{IngestionCheckpoint, IngressRange, SourceCursor},
};
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
    failed_boundary: Option<SourceCursor>,
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
                failed_boundary: None,
            }),
        }
    }

    pub fn record_failure(
        &self,
        error: &TurboError,
        failed_range: Option<&IngressRange>,
    ) -> RecoveryDecision {
        let descriptor = FailureDescriptor::from_error(error);
        let now = unix_ms();
        let mut state = self
            .state
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let previous = state.snapshot.clone();
        let same_incident = previous.active
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

        let failed_boundary = if same_incident {
            state
                .failed_boundary
                .clone()
                .or_else(|| failed_range.map(|range| range.end_cursor.clone()))
        } else {
            failed_range.map(|range| range.end_cursor.clone())
        };
        state.failed_boundary = failed_boundary;
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
            blocked_checkpoint_ordinal: failed_range.map(|range| range.end_ordinal),
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

    /// Clears containment only after durable source progress reaches or passes
    /// the failed portable boundary. Process-local ordinals are not considered.
    pub fn observe_checkpoint(
        &self,
        checkpoint: &IngestionCheckpoint,
    ) -> Option<FailureContainmentSnapshot> {
        let mut state = self
            .state
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let boundary = state.failed_boundary.as_ref()?;
        if !state.snapshot.active || !checkpoint_passes_boundary(&checkpoint.cursor, boundary) {
            return None;
        }
        let recovered = state.snapshot.clone();
        state.snapshot = FailureContainmentSnapshot::default();
        state.failed_boundary = None;
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

fn checkpoint_passes_boundary(checkpoint: &SourceCursor, boundary: &SourceCursor) -> bool {
    checkpoint.source_event_id == boundary.source_event_id || checkpoint.time_us > boundary.time_us
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
            diagnostic_summary: None,
            attempts: 2,
            retry_limit: 1,
            request_cardinality: 2,
            transient: true,
            request_fingerprint: hash.to_string(),
            isolation: None,
        }
        .into()
    }

    #[test]
    fn identical_failures_grow_and_cap_without_connection_reset() {
        let supervisor = FailureSupervisor::new(policy());
        let failed_range = range(9, 10);
        let first = supervisor.record_failure(&failure("same"), Some(&failed_range));
        let second = supervisor.record_failure(&failure("same"), Some(&failed_range));
        let third = supervisor.record_failure(&failure("same"), Some(&failed_range));
        assert_eq!(first.recurrence, 1);
        assert_eq!(second.recurrence, 2);
        assert!(second.delay > first.delay);
        assert_eq!(third.delay, Duration::from_secs(4));
        assert!(second.persistent);
    }

    #[test]
    fn distinct_fingerprint_starts_a_new_sequence() {
        let supervisor = FailureSupervisor::new(policy());
        let failed_range = range(9, 10);
        supervisor.record_failure(&failure("one"), Some(&failed_range));
        let decision = supervisor.record_failure(&failure("two"), Some(&failed_range));
        assert_eq!(decision.recurrence, 1);
    }

    #[test]
    fn portable_checkpoint_progress_clears_containment() {
        let supervisor = FailureSupervisor::new(policy());
        let failed_range = range(9, 10);
        supervisor.record_failure(&failure("same"), Some(&failed_range));
        assert!(supervisor
            .observe_checkpoint(&checkpoint(99, 9_999, "earlier"))
            .is_none());
        assert!(supervisor.snapshot().active);
        assert!(supervisor
            .observe_checkpoint(&checkpoint(100, 10_000, "event-10"))
            .is_some());
        assert!(!supervisor.snapshot().active);
        let decision = supervisor.record_failure(&failure("same"), Some(&failed_range));
        assert_eq!(decision.recurrence, 1);
    }

    #[test]
    fn higher_replay_ordinal_does_not_clear_before_failed_source_boundary() {
        let supervisor = FailureSupervisor::new(policy());
        let failed_range = range(9, 10);
        supervisor.record_failure(&failure("same"), Some(&failed_range));
        assert!(supervisor
            .observe_checkpoint(&checkpoint(1_000, 9_000, "replayed-earlier"))
            .is_none());
        assert!(supervisor.snapshot().active);
    }

    #[test]
    fn strictly_later_source_time_clears_with_different_event_identity() {
        let supervisor = FailureSupervisor::new(policy());
        let failed_range = range(9, 10);
        supervisor.record_failure(&failure("same"), Some(&failed_range));
        assert!(supervisor
            .observe_checkpoint(&checkpoint(1, 10_001, "different-event"))
            .is_some());
    }

    #[test]
    fn boundaryless_failure_is_not_cleared_by_checkpoint_movement() {
        let supervisor = FailureSupervisor::new(policy());
        supervisor.record_failure(&failure("same"), None);
        assert!(supervisor
            .observe_checkpoint(&checkpoint(1_000, u64::MAX, "later"))
            .is_none());
        assert!(supervisor.snapshot().active);
    }

    fn range(start: u64, end: u64) -> IngressRange {
        IngressRange {
            start_ordinal: start,
            end_ordinal: end,
            start_cursor: cursor(start * 1_000, &format!("event-{start}")),
            end_cursor: cursor(end * 1_000, &format!("event-{end}")),
        }
    }

    fn checkpoint(ordinal: u64, time_us: u64, event_id: &str) -> IngestionCheckpoint {
        IngestionCheckpoint {
            ingress_ordinal: ordinal,
            cursor: cursor(time_us, event_id),
            updated_at: chrono::Utc::now(),
        }
    }

    fn cursor(time_us: u64, event_id: &str) -> SourceCursor {
        SourceCursor {
            time_us,
            source_seq: None,
            source_event_id: event_id.to_string().into(),
        }
    }
}
