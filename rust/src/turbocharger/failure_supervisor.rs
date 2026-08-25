use crate::client::{ContainmentPolicy, IsolationOutcome};
use crate::models::{
    errors::TurboError,
    recovery::{IngestionCheckpoint, IngressRange, SourceCursor},
};
use serde::Serialize;
use std::sync::Mutex;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum PipelineFailureSubtype {
    BatchOrdering,
    CheckpointDecoding,
    RangeCoordination,
    Storage,
    Publication,
    UnknownInvariant,
}

impl PipelineFailureSubtype {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::BatchOrdering => "batch_ordering",
            Self::CheckpointDecoding => "checkpoint_decoding",
            Self::RangeCoordination => "range_coordination",
            Self::Storage => "storage",
            Self::Publication => "publication",
            Self::UnknownInvariant => "unknown_invariant",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum PipelineFailureStage {
    Ingress,
    Checkpoint,
    Coordination,
    Storage,
    Publication,
    Unknown,
}

impl PipelineFailureStage {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Ingress => "ingress",
            Self::Checkpoint => "checkpoint",
            Self::Coordination => "coordination",
            Self::Storage => "storage",
            Self::Publication => "publication",
            Self::Unknown => "unknown",
        }
    }
}

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
    pub subtype: Option<PipelineFailureSubtype>,
    pub stage: Option<PipelineFailureStage>,
    pub boundary_present: bool,
    pub incident_start_checkpoint_ordinal: Option<u64>,
    pub total_occurrences: u64,
    pub recovered_incidents: u64,
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
        durable_checkpoint_ordinal: Option<u64>,
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
        let delay = recovery_delay(self.policy, recurrence, &descriptor.fingerprint);
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
            subtype: Some(descriptor.subtype),
            stage: Some(descriptor.stage),
            boundary_present: failed_range.is_some(),
            incident_start_checkpoint_ordinal: if same_incident {
                previous.incident_start_checkpoint_ordinal
            } else {
                durable_checkpoint_ordinal
            },
            total_occurrences: previous.total_occurrences.saturating_add(1),
            recovered_incidents: previous.recovered_incidents,
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

    /// Clears range-bound containment at its portable source boundary and
    /// boundaryless containment after new durable ordinal progress.
    pub fn observe_checkpoint(
        &self,
        checkpoint: &IngestionCheckpoint,
    ) -> Option<FailureContainmentSnapshot> {
        let mut state = self
            .state
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        if !state.snapshot.active {
            return None;
        }
        let recovered_by_progress = match state.failed_boundary.as_ref() {
            Some(boundary) => checkpoint_passes_boundary(&checkpoint.cursor, boundary),
            None => state
                .snapshot
                .incident_start_checkpoint_ordinal
                .is_none_or(|start| checkpoint.ingress_ordinal > start),
        };
        if !recovered_by_progress {
            return None;
        }
        let recovered = state.snapshot.clone();
        state.snapshot = FailureContainmentSnapshot {
            total_occurrences: recovered.total_occurrences,
            recovered_incidents: recovered.recovered_incidents.saturating_add(1),
            ..FailureContainmentSnapshot::default()
        };
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
    subtype: PipelineFailureSubtype,
    stage: PipelineFailureStage,
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
                subtype: PipelineFailureSubtype::UnknownInvariant,
                stage: PipelineFailureStage::Unknown,
            },
            other => {
                let category = error_category(other);
                Self {
                    fingerprint: format!("pipeline:{category}"),
                    operation: None,
                    category: category.to_string(),
                    retryable: other.is_retryable(),
                    isolation: None,
                    subtype: failure_subtype(other),
                    stage: failure_stage(other),
                }
            }
        }
    }
}

fn failure_subtype(error: &TurboError) -> PipelineFailureSubtype {
    match error {
        TurboError::InvalidMessage(message) if message.contains("ingress batch") => {
            PipelineFailureSubtype::BatchOrdering
        }
        TurboError::InvalidMessage(message) if message.contains("ingress range") => {
            PipelineFailureSubtype::RangeCoordination
        }
        TurboError::JsonDeserialization(_) => PipelineFailureSubtype::CheckpointDecoding,
        TurboError::Database(_) | TurboError::RotationFailed(_) => PipelineFailureSubtype::Storage,
        TurboError::RedisOperation(_) => PipelineFailureSubtype::Publication,
        _ => PipelineFailureSubtype::UnknownInvariant,
    }
}

fn failure_stage(error: &TurboError) -> PipelineFailureStage {
    match failure_subtype(error) {
        PipelineFailureSubtype::BatchOrdering => PipelineFailureStage::Ingress,
        PipelineFailureSubtype::CheckpointDecoding => PipelineFailureStage::Checkpoint,
        PipelineFailureSubtype::RangeCoordination => PipelineFailureStage::Coordination,
        PipelineFailureSubtype::Storage => PipelineFailureStage::Storage,
        PipelineFailureSubtype::Publication => PipelineFailureStage::Publication,
        PipelineFailureSubtype::UnknownInvariant => PipelineFailureStage::Unknown,
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
        TurboError::SchemaMaintenanceRequired { .. } => "schema_maintenance",
        TurboError::RedisOperation(_) => "publication",
        TurboError::JsonSerialization(_) | TurboError::JsonDeserialization(_) => "serialization",
        TurboError::CacheOperation(_) => "cache",
        TurboError::InvalidMessage(_) => "invalid_message",
        TurboError::HydrationFailed(_) => "hydration",
        TurboError::RotationFailed(_) => "storage_rotation",
        TurboError::Io(_) => "io",
        TurboError::TaskJoin(_) => "task_join",
        TurboError::Timeout(_) | TurboError::BatchStageTimeout { .. } => "timeout",
        TurboError::ControlledMemoryExit => "memory_emergency",
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
        let first = supervisor.record_failure(&failure("same"), Some(&failed_range), None);
        let second = supervisor.record_failure(&failure("same"), Some(&failed_range), None);
        let third = supervisor.record_failure(&failure("same"), Some(&failed_range), None);
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
        supervisor.record_failure(&failure("one"), Some(&failed_range), None);
        let decision = supervisor.record_failure(&failure("two"), Some(&failed_range), None);
        assert_eq!(decision.recurrence, 1);
    }

    #[test]
    fn portable_checkpoint_progress_clears_containment() {
        let supervisor = FailureSupervisor::new(policy());
        let failed_range = range(9, 10);
        supervisor.record_failure(&failure("same"), Some(&failed_range), None);
        assert!(supervisor
            .observe_checkpoint(&checkpoint(99, 9_999, "earlier"))
            .is_none());
        assert!(supervisor.snapshot().active);
        assert!(supervisor
            .observe_checkpoint(&checkpoint(100, 10_000, "event-10"))
            .is_some());
        assert!(!supervisor.snapshot().active);
        let decision = supervisor.record_failure(&failure("same"), Some(&failed_range), None);
        assert_eq!(decision.recurrence, 1);
    }

    #[test]
    fn higher_replay_ordinal_does_not_clear_before_failed_source_boundary() {
        let supervisor = FailureSupervisor::new(policy());
        let failed_range = range(9, 10);
        supervisor.record_failure(&failure("same"), Some(&failed_range), None);
        assert!(supervisor
            .observe_checkpoint(&checkpoint(1_000, 9_000, "replayed-earlier"))
            .is_none());
        assert!(supervisor.snapshot().active);
    }

    #[test]
    fn strictly_later_source_time_clears_with_different_event_identity() {
        let supervisor = FailureSupervisor::new(policy());
        let failed_range = range(9, 10);
        supervisor.record_failure(&failure("same"), Some(&failed_range), None);
        assert!(supervisor
            .observe_checkpoint(&checkpoint(1, 10_001, "different-event"))
            .is_some());
    }

    #[test]
    fn boundaryless_failure_survives_without_progress_and_resets_after_progress() {
        let supervisor = FailureSupervisor::new(policy());
        let first = supervisor.record_failure(&failure("same"), None, Some(10));
        assert!(supervisor
            .observe_checkpoint(&checkpoint(10, u64::MAX, "same-ordinal"))
            .is_none());
        assert!(supervisor.snapshot().active);
        let recurrence = supervisor.record_failure(&failure("same"), None, Some(10));
        assert_eq!(recurrence.recurrence, 2);
        assert!(recurrence.delay > first.delay);
        let recovered = supervisor
            .observe_checkpoint(&checkpoint(11, 1, "regressed-time"))
            .expect("greater durable ordinal clears a boundaryless incident");
        assert_eq!(recovered.recurrence, 2);
        assert_eq!(supervisor.snapshot().recovered_incidents, 1);
        let next = supervisor.record_failure(&failure("same"), None, Some(11));
        assert_eq!(next.recurrence, 1);
    }

    #[test]
    fn first_checkpoint_clears_startup_boundaryless_incident() {
        let supervisor = FailureSupervisor::new(policy());
        supervisor.record_failure(&failure("same"), None, None);

        assert!(supervisor
            .observe_checkpoint(&checkpoint(1, 1, "first"))
            .is_some());
    }

    #[test]
    fn internal_invariant_uses_minimum_first_delay_and_bounded_identity() {
        let supervisor = FailureSupervisor::new(policy());
        let error = TurboError::InvalidMessage(
            "ingress batch must be non-empty and ordered; private payload omitted".to_string(),
        );

        let decision = supervisor.record_failure(&error, None, Some(4));
        let snapshot = supervisor.snapshot();

        assert_eq!(decision.delay, policy().min_delay);
        assert_eq!(
            snapshot.subtype,
            Some(PipelineFailureSubtype::BatchOrdering)
        );
        assert_eq!(snapshot.stage, Some(PipelineFailureStage::Ingress));
        assert!(!snapshot.boundary_present);
        assert_eq!(snapshot.incident_start_checkpoint_ordinal, Some(4));
        assert!(!serde_json::to_string(&snapshot)
            .unwrap()
            .contains("private payload"));
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
