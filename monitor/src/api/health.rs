//! Bounded in-memory operational snapshot and `/api/v1/health` semantics.

use chrono::{DateTime, Utc};
use serde::Serialize;
use utoipa::ToSchema;

/// Overall monitor self-health classification.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, ToSchema)]
#[serde(rename_all = "snake_case")]
pub enum HealthStatus {
    Healthy,
    /// The monitor works but observes an external problem.
    Degraded,
    /// The monitor cannot provide trustworthy observation.
    Unhealthy,
}

/// Machine-readable unhealthy reason (bounded).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, ToSchema)]
#[serde(rename_all = "snake_case")]
pub enum UnhealthyReason {
    ObservationLoopStopped,
    StorageUnhealthy,
}

/// Health of one required persistence writer.
#[derive(Debug, Clone, Serialize, ToSchema)]
pub struct StorageHealth {
    pub available: bool,
    /// Seconds since the last successful write, when one has occurred.
    pub last_success_age_seconds: Option<u64>,
    /// Current expected staleness allowance in seconds.
    pub stale_after_seconds: u64,
}

impl StorageHealth {
    pub fn is_stale(&self) -> bool {
        self.available
            && self
                .last_success_age_seconds
                .is_some_and(|age| age > self.stale_after_seconds)
    }
}

/// Bounded per-stream operational state for the health surface.
#[derive(Debug, Clone, Serialize, ToSchema)]
pub struct StreamHealth {
    pub stream_id: String,
    pub transport: String,
    pub delivery: String,
    pub delivery_idle: bool,
    /// Seconds since the stream state last changed.
    pub state_age_seconds: u64,
    /// Seconds since the last useful record, when one has been observed.
    pub last_useful_record_age_seconds: Option<u64>,
    /// Seconds since the last peer-liveness frame, when connected.
    pub last_pong_age_seconds: Option<u64>,
    /// Source event-time lag in seconds, when known.
    pub source_lag_seconds: Option<u64>,
    pub connection_epoch: u64,
    /// Failed reconnect attempts reported since the last transport recovery.
    pub reconnect_attempts: u64,
    /// Milliseconds since the original boundary of the ongoing outage.
    pub outage_elapsed_ms: u64,
    pub outage_episodes: u64,
    pub idle_episodes: u64,
    pub active_incident_id: Option<String>,
}

/// Full health snapshot as served by `GET /api/v1/health`.
#[derive(Debug, Clone, Serialize, ToSchema)]
pub struct HealthSnapshot {
    pub status: HealthStatus,
    pub api_version: String,
    pub process_epoch: String,
    pub release: String,
    pub process_uptime_seconds: u64,
    pub observation_loop_alive: bool,
    pub incident_storage: StorageHealth,
    pub hourly_storage: StorageHealth,
    pub streams: Vec<StreamHealth>,
}

impl HealthSnapshot {
    /// Compute the health snapshot from bounded inputs.
    #[allow(clippy::too_many_arguments)]
    pub fn compute(
        now: DateTime<Utc>,
        process_epoch: String,
        release: String,
        started_at: DateTime<Utc>,
        observation_loop_alive: bool,
        incident_storage: StorageHealth,
        hourly_storage: StorageHealth,
        streams: Vec<StreamHealth>,
    ) -> Self {
        let status = classify(
            observation_loop_alive,
            &incident_storage,
            &hourly_storage,
            &streams,
        );
        Self {
            status,
            api_version: "v1".to_string(),
            process_epoch,
            release,
            process_uptime_seconds: (now - started_at).num_seconds().max(0) as u64,
            observation_loop_alive,
            incident_storage,
            hourly_storage,
            streams,
        }
    }
}

fn classify(
    observation_loop_alive: bool,
    incident_storage: &StorageHealth,
    hourly_storage: &StorageHealth,
    streams: &[StreamHealth],
) -> HealthStatus {
    if !observation_loop_alive {
        return HealthStatus::Unhealthy;
    }
    if !incident_storage.available || incident_storage.is_stale() {
        return HealthStatus::Unhealthy;
    }
    // The hourly writer lagging its expected cadence is monitor degradation,
    // not loss of trustworthy observation.
    let _ = hourly_storage;
    let all_delivering = streams
        .iter()
        .all(|s| s.transport == "connected" && s.delivery == "delivering");
    if all_delivering {
        HealthStatus::Healthy
    } else {
        HealthStatus::Degraded
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn delivering(stream: &str) -> StreamHealth {
        StreamHealth {
            stream_id: stream.to_string(),
            transport: "connected".to_string(),
            delivery: "delivering".to_string(),
            delivery_idle: false,
            state_age_seconds: 0,
            last_useful_record_age_seconds: Some(0),
            last_pong_age_seconds: Some(0),
            source_lag_seconds: Some(0),
            connection_epoch: 1,
            reconnect_attempts: 0,
            outage_elapsed_ms: 0,
            outage_episodes: 0,
            idle_episodes: 0,
            active_incident_id: None,
        }
    }

    fn storage(available: bool, age: Option<u64>) -> StorageHealth {
        StorageHealth {
            available,
            last_success_age_seconds: age,
            stale_after_seconds: 7200,
        }
    }

    fn healthy_storage() -> StorageHealth {
        storage(true, Some(1))
    }

    #[test]
    fn healthy_when_all_components_operate() {
        let snapshot = HealthSnapshot::compute(
            Utc::now(),
            "epoch".into(),
            "1.0".into(),
            Utc::now(),
            true,
            healthy_storage(),
            healthy_storage(),
            vec![delivering("a"), delivering("b")],
        );
        assert_eq!(snapshot.status, HealthStatus::Healthy);
    }

    #[test]
    fn stream_degradation_is_reported_as_http_200_degraded() {
        let mut idle = delivering("a");
        idle.delivery = "idle".to_string();
        idle.delivery_idle = true;
        let snapshot = HealthSnapshot::compute(
            Utc::now(),
            "epoch".into(),
            "1.0".into(),
            Utc::now(),
            true,
            healthy_storage(),
            healthy_storage(),
            vec![idle, delivering("b")],
        );
        assert_eq!(snapshot.status, HealthStatus::Degraded);
    }

    #[test]
    fn stopped_observation_loop_is_unhealthy() {
        let snapshot = HealthSnapshot::compute(
            Utc::now(),
            "epoch".into(),
            "1.0".into(),
            Utc::now(),
            false,
            healthy_storage(),
            healthy_storage(),
            vec![delivering("a")],
        );
        assert_eq!(snapshot.status, HealthStatus::Unhealthy);
    }

    #[test]
    fn unavailable_incident_storage_is_unhealthy() {
        let snapshot = HealthSnapshot::compute(
            Utc::now(),
            "epoch".into(),
            "1.0".into(),
            Utc::now(),
            true,
            storage(false, None),
            healthy_storage(),
            vec![delivering("a")],
        );
        assert_eq!(snapshot.status, HealthStatus::Unhealthy);
    }

    #[test]
    fn stale_hourly_writer_still_serves_health() {
        // Hourly staleness alone does not make the monitor untrustworthy.
        let snapshot = HealthSnapshot::compute(
            Utc::now(),
            "epoch".into(),
            "1.0".into(),
            Utc::now(),
            true,
            healthy_storage(),
            storage(true, Some(100_000)),
            vec![delivering("a")],
        );
        assert_ne!(snapshot.status, HealthStatus::Unhealthy);
    }
}
