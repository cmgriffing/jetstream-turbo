//! Incident ledger types: bounded states, triggers, events, and identifiers.

pub mod store;

pub use store::IncidentStore;

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use std::fmt;

/// Sortable, opaque incident identifier (ULID, lexicographically time-sortable).
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct IncidentId(String);

impl IncidentId {
    /// Generate a new time-sortable opaque identifier.
    pub fn generate() -> Self {
        Self(ulid::Ulid::new().to_string())
    }

    /// Wrap an existing identifier string after validation.
    pub fn from_string(value: impl Into<String>) -> Option<Self> {
        let value = value.into();
        if value.len() == 26 && value.bytes().all(|b| b.is_ascii_alphanumeric()) {
            Some(Self(value))
        } else {
            None
        }
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for IncidentId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

/// Lifecycle state of a delivery-disruption incident.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum IncidentState {
    Open,
    Resolved,
    /// An incident whose later observation is missing (e.g. monitor restart).
    Incomplete,
}

impl IncidentState {
    pub fn is_terminal(self) -> bool {
        matches!(self, IncidentState::Resolved | IncidentState::Incomplete)
    }
}

/// What initially triggered the delivery disruption.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum IncidentTrigger {
    /// Previously delivering stream crossed the delivery-idle threshold.
    DeliveryIdle,
    /// Transport was lost while delivery had been available.
    TransportLoss,
}

/// Bounded, sanitized event types recorded in the incident ledger.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum IncidentEventType {
    /// The idle threshold was crossed while the socket remained alive.
    DeliveryIdleDetected,
    /// Transport loss observed (socket error, peer close, or liveness deadline).
    TransportLost,
    /// A reconnect attempt failed while the incident was open.
    ReconnectAttemptFailed,
    /// Handshake succeeded after transport loss.
    TransportRecovered,
    /// Useful delivery resumed and resolved the incident.
    DeliveryRecovered,
    /// An observation gap was recorded at startup reconciliation.
    ObservationGap,
}

/// A bounded, sanitized event in an incident's ordered history.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct IncidentEvent {
    /// Incident-local, monotonically increasing sequence number starting at 1.
    pub sequence: i64,
    pub event_type: IncidentEventType,
    pub occurred_at: DateTime<Utc>,
    /// Bounded reason (e.g. `socket_error`, `peer_close`, `liveness_deadline`).
    pub reason: Option<String>,
    /// 1-based attempt ordinal for reconnect-attempt events.
    pub attempt_ordinal: Option<u64>,
    /// Selected backoff delay in milliseconds, when a delay was scheduled.
    pub scheduled_delay_ms: Option<u64>,
}

/// Bounded connection-transport loss reason used in transitions and incidents.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TransportLossReason {
    SocketError,
    SocketWrite,
    PeerClose,
    /// No peer frame (Pong or otherwise) arrived before the liveness deadline.
    LivenessDeadline,
}

impl TransportLossReason {
    pub fn as_str(self) -> &'static str {
        match self {
            TransportLossReason::SocketError => "socket_error",
            TransportLossReason::SocketWrite => "socket_write",
            TransportLossReason::PeerClose => "peer_close",
            TransportLossReason::LivenessDeadline => "liveness_deadline",
        }
    }
}

/// Bounded handshake failure reason used for reconnect-attempt events.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum HandshakeFailureReason {
    ConnectError,
    ConnectTimeout,
}

impl HandshakeFailureReason {
    pub fn as_str(self) -> &'static str {
        match self {
            HandshakeFailureReason::ConnectError => "connect_error",
            HandshakeFailureReason::ConnectTimeout => "connect_timeout",
        }
    }
}

/// Identity of the monitor process that observed an incident.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MonitorIdentity {
    /// Process-unique identity regenerated at every monitor start.
    pub process_epoch: String,
    /// Deployed release identifier (semver or build identity).
    pub release: String,
}

/// Full incident summary as persisted and exposed through the API.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct IncidentSummary {
    pub id: IncidentId,
    /// Stable, bounded stream identifier (a, b, baseline1, baseline2).
    pub stream_id: String,
    pub state: IncidentState,
    pub trigger: IncidentTrigger,
    /// Last time a useful record was observed before the disruption.
    pub last_useful_record_at: Option<DateTime<Utc>>,
    /// When the disruption was detected (later than the gap start for idles).
    pub detected_at: DateTime<Utc>,
    /// When the incident reached a terminal state.
    pub resolved_at: Option<DateTime<Utc>>,
    /// When the transport next recovered (handshake success), if observed.
    pub transport_recovered_at: Option<DateTime<Utc>>,
    /// Total silence duration in milliseconds from last useful record to resolution.
    pub total_silence_ms: Option<u64>,
    /// Detected recovery duration in milliseconds from detection to resolution.
    pub detected_recovery_ms: Option<u64>,
    /// Number of failed reconnect attempts recorded during the incident.
    pub reconnect_attempts: u64,
    /// Connection epoch during which the incident was observed.
    pub connection_epoch: u64,
    /// Whether observation covered the entire incident.
    pub observation_complete: bool,
    pub monitor_process_epoch: String,
    pub monitor_release: String,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
}
/// In-memory health of the incident (and hourly) writers: last-success ages
/// and bounded failure counts, surfaced through health and metrics.
#[derive(Debug, Default)]
pub struct LedgerHealth {
    last_success_epoch: std::sync::Mutex<Option<DateTime<Utc>>>,
    last_failure_epoch: std::sync::Mutex<Option<DateTime<Utc>>>,
    consecutive_failures: std::sync::atomic::AtomicU64,
}

impl LedgerHealth {
    pub fn record_success(&self, at: DateTime<Utc>) {
        *self
            .last_success_epoch
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner()) = Some(at);
        self.consecutive_failures
            .store(0, std::sync::atomic::Ordering::Relaxed);
    }

    pub fn record_failure(&self, at: DateTime<Utc>) {
        *self
            .last_failure_epoch
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner()) = Some(at);
        self.consecutive_failures
            .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    }

    /// True when no consecutive failures have occurred.
    pub fn healthy(&self) -> bool {
        self.consecutive_failures
            .load(std::sync::atomic::Ordering::Relaxed)
            == 0
    }

    /// Seconds since the last successful write; None before the first write.
    pub fn last_success_age_seconds(&self, now: DateTime<Utc>) -> Option<u64> {
        self.last_success_epoch
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .map(|at| (now - at).num_seconds().max(0) as u64)
    }

    pub fn last_failure(&self) -> Option<DateTime<Utc>> {
        *self
            .last_failure_epoch
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }
}

#[cfg(test)]
mod health_tests {
    use super::*;

    #[test]
    fn ledger_health_tracks_success_and_failures() {
        let health = LedgerHealth::default();
        assert!(health.healthy());
        assert_eq!(health.last_success_age_seconds(Utc::now()), None);

        health.record_success(Utc::now() - chrono::Duration::seconds(10));
        assert_eq!(health.last_success_age_seconds(Utc::now()), Some(10));

        health.record_failure(Utc::now());
        assert!(!health.healthy());

        health.record_success(Utc::now());
        assert!(health.healthy());
    }
}
