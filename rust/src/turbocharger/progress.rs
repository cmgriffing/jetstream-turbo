use serde::Serialize;
use std::collections::HashMap;
use std::sync::Mutex;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum PipelineStage {
    Ingress,
    Hydration,
    Storage,
    Publication,
    Broadcast,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum PipelineReadinessState {
    Starting,
    Recovering,
    Healthy,
    Stale,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "snake_case")]
pub struct ActiveBatchSnapshot {
    pub batch_id: u64,
    pub stage: PipelineStage,
    pub age_seconds: u64,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "snake_case")]
pub struct PipelineProgressSnapshot {
    pub readiness_state: PipelineReadinessState,
    pub stale_stage: Option<PipelineStage>,
    pub readiness_reason: Option<String>,
    pub connected_endpoint: Option<String>,
    pub last_reconnect_reason: Option<String>,
    pub reconnect_count: u64,
    pub reconnect_reasons: HashMap<String, u64>,
    pub recovery_duration_ms: Option<u64>,
    pub ingress_messages: u64,
    pub completed_batches: u64,
    pub completed_records: u64,
    pub timed_out_batches: u64,
    pub input_drops: u64,
    pub input_backpressured: bool,
    pub input_occupancy: usize,
    pub input_capacity: usize,
    pub active_permits: usize,
    pub maximum_permits: usize,
    pub broadcast_receivers: usize,
    pub successful_broadcasts: u64,
    pub last_valid_ingress_unix_ms: Option<u64>,
    pub last_batch_completion_unix_ms: Option<u64>,
    pub last_store_unix_ms: Option<u64>,
    pub last_publication_unix_ms: Option<u64>,
    pub ingress_age_seconds: Option<u64>,
    pub completion_age_seconds: Option<u64>,
    pub oldest_active_batch_age_seconds: Option<u64>,
    pub active_batches: Vec<ActiveBatchSnapshot>,
}

#[derive(Debug, Clone, Copy)]
pub struct ProgressThresholds {
    pub startup_grace: Duration,
    pub ingress_idle: Duration,
    pub batch_execution: Duration,
    pub recovery_successes: u32,
}

#[derive(Debug)]
struct TimedEvent {
    monotonic: Instant,
    unix_ms: u64,
}

#[derive(Debug)]
struct ActiveBatch {
    stage: PipelineStage,
    started_at: Instant,
}

#[derive(Debug)]
struct ProgressState {
    started_at: Instant,
    connected_endpoint: Option<String>,
    last_reconnect_reason: Option<String>,
    reconnect_started_at: Option<Instant>,
    reconnect_count: u64,
    reconnect_reasons: HashMap<String, u64>,
    recovery_duration_ms: Option<u64>,
    last_ingress: Option<TimedEvent>,
    first_ingress_at: Option<Instant>,
    last_completion: Option<TimedEvent>,
    last_store: Option<TimedEvent>,
    last_publication: Option<TimedEvent>,
    ingress_messages: u64,
    completed_batches: u64,
    completed_records: u64,
    timed_out_batches: u64,
    input_drops: u64,
    input_backpressured: bool,
    input_occupancy: usize,
    input_capacity: usize,
    maximum_permits: usize,
    broadcast_receivers: usize,
    successful_broadcasts: u64,
    next_batch_id: u64,
    active_batches: HashMap<u64, ActiveBatch>,
    was_stale: bool,
    recovery_observations: u32,
    last_reported_state: Option<(PipelineReadinessState, Option<PipelineStage>)>,
}

#[derive(Debug)]
pub struct PipelineProgress {
    state: Mutex<ProgressState>,
}

impl PipelineProgress {
    pub fn new(maximum_permits: usize, input_capacity: usize) -> Self {
        Self::with_started_at(maximum_permits, input_capacity, Instant::now())
    }

    fn with_started_at(maximum_permits: usize, input_capacity: usize, started_at: Instant) -> Self {
        Self {
            state: Mutex::new(ProgressState {
                started_at,
                connected_endpoint: None,
                last_reconnect_reason: None,
                reconnect_started_at: None,
                reconnect_count: 0,
                reconnect_reasons: HashMap::new(),
                recovery_duration_ms: None,
                last_ingress: None,
                first_ingress_at: None,
                last_completion: None,
                last_store: None,
                last_publication: None,
                ingress_messages: 0,
                completed_batches: 0,
                completed_records: 0,
                timed_out_batches: 0,
                input_drops: 0,
                input_backpressured: false,
                input_occupancy: 0,
                input_capacity,
                maximum_permits,
                broadcast_receivers: 0,
                successful_broadcasts: 0,
                next_batch_id: 1,
                active_batches: HashMap::new(),
                was_stale: false,
                recovery_observations: 0,
                last_reported_state: None,
            }),
        }
    }

    fn state(&self) -> std::sync::MutexGuard<'_, ProgressState> {
        self.state
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    pub fn connection_established(&self, endpoint: impl Into<String>) {
        self.state().connected_endpoint = Some(endpoint.into());
    }

    pub fn disconnected(&self, reason: impl Into<String>) {
        let mut state = self.state();
        state.connected_endpoint = None;
        let reason = reason.into();
        state.last_reconnect_reason = Some(reason.clone());
        *state.reconnect_reasons.entry(reason).or_insert(0) += 1;
        state.reconnect_count += 1;
        state.reconnect_started_at = Some(Instant::now());
    }

    pub fn valid_ingress(&self) -> Option<u64> {
        self.valid_ingress_at(Instant::now(), SystemTime::now())
    }

    fn valid_ingress_at(&self, now: Instant, wall: SystemTime) -> Option<u64> {
        let mut state = self.state();
        state.ingress_messages += 1;
        state.first_ingress_at.get_or_insert(now);
        state.last_ingress = Some(TimedEvent {
            monotonic: now,
            unix_ms: unix_ms(wall),
        });
        let recovery = state
            .reconnect_started_at
            .take()
            .map(|started| duration_millis(now.saturating_duration_since(started)));
        if recovery.is_some() {
            state.recovery_duration_ms = recovery;
        }
        recovery
    }

    pub fn input_dropped(&self, occupancy: usize) {
        let mut state = self.state();
        state.input_drops += 1;
        state.input_backpressured = true;
        state.input_occupancy = occupancy;
    }

    pub fn input_recovered(&self, occupancy: usize) {
        let mut state = self.state();
        state.input_backpressured = false;
        state.input_occupancy = occupancy;
    }

    pub fn batch_started(&self) -> u64 {
        self.batch_started_at(Instant::now())
    }

    fn batch_started_at(&self, now: Instant) -> u64 {
        let mut state = self.state();
        let id = state.next_batch_id;
        state.next_batch_id += 1;
        state.active_batches.insert(
            id,
            ActiveBatch {
                stage: PipelineStage::Hydration,
                started_at: now,
            },
        );
        id
    }

    pub fn batch_stage(&self, batch_id: u64, stage: PipelineStage) {
        if let Some(batch) = self.state().active_batches.get_mut(&batch_id) {
            batch.stage = stage;
        }
    }

    pub fn store_succeeded(&self) {
        self.state().last_store = Some(event_now());
    }

    pub fn publication_succeeded(&self) {
        self.state().last_publication = Some(event_now());
    }

    pub fn batch_completed(&self, batch_id: u64, records: usize) {
        self.batch_completed_at(batch_id, records, Instant::now(), SystemTime::now());
    }

    fn batch_completed_at(&self, batch_id: u64, records: usize, now: Instant, wall: SystemTime) {
        let mut state = self.state();
        state.active_batches.remove(&batch_id);
        state.completed_batches += 1;
        state.completed_records += records as u64;
        state.last_completion = Some(TimedEvent {
            monotonic: now,
            unix_ms: unix_ms(wall),
        });
    }

    pub fn batch_timed_out(&self, batch_id: u64) {
        let mut state = self.state();
        state.active_batches.remove(&batch_id);
        state.timed_out_batches += 1;
    }

    pub fn batch_failed(&self, batch_id: u64) {
        self.state().active_batches.remove(&batch_id);
    }

    pub fn broadcast_state(&self, receivers: usize, successful_sends: usize) {
        let mut state = self.state();
        state.broadcast_receivers = receivers;
        state.successful_broadcasts += successful_sends as u64;
    }

    pub fn snapshot(&self, thresholds: ProgressThresholds) -> PipelineProgressSnapshot {
        self.snapshot_at(thresholds, Instant::now())
    }

    fn snapshot_at(
        &self,
        thresholds: ProgressThresholds,
        now: Instant,
    ) -> PipelineProgressSnapshot {
        let mut state = self.state();
        let ingress_age = state
            .last_ingress
            .as_ref()
            .map(|event| now.saturating_duration_since(event.monotonic));
        let completion_age = state
            .last_completion
            .as_ref()
            .map(|event| now.saturating_duration_since(event.monotonic));
        let mut active_batches: Vec<_> = state
            .active_batches
            .iter()
            .map(|(&batch_id, batch)| ActiveBatchSnapshot {
                batch_id,
                stage: batch.stage,
                age_seconds: now.saturating_duration_since(batch.started_at).as_secs(),
            })
            .collect();
        active_batches.sort_by_key(|batch| batch.batch_id);
        let oldest = state
            .active_batches
            .values()
            .map(|batch| now.saturating_duration_since(batch.started_at))
            .max();

        let (mut readiness_state, stale_stage, mut readiness_reason) =
            if state.last_ingress.is_none()
                && now.saturating_duration_since(state.started_at) <= thresholds.startup_grace
            {
                (
                    PipelineReadinessState::Starting,
                    None,
                    Some("startup_grace".to_string()),
                )
            } else if state.last_ingress.is_none()
                || ingress_age.is_some_and(|age| age > thresholds.ingress_idle)
            {
                (
                    PipelineReadinessState::Stale,
                    Some(PipelineStage::Ingress),
                    Some("ingress_stale".to_string()),
                )
            } else if oldest.is_some_and(|age| age > thresholds.batch_execution) {
                let stage = state
                    .active_batches
                    .values()
                    .max_by_key(|batch| now.saturating_duration_since(batch.started_at))
                    .map(|batch| batch.stage)
                    .unwrap_or(PipelineStage::Hydration);
                (
                    PipelineReadinessState::Stale,
                    Some(stage),
                    Some("batch_stalled".to_string()),
                )
            } else if state.first_ingress_at.is_some_and(|first| {
                state
                    .last_completion
                    .as_ref()
                    .map(|event| {
                        now.saturating_duration_since(event.monotonic) > thresholds.batch_execution
                    })
                    .unwrap_or_else(|| {
                        now.saturating_duration_since(first) > thresholds.batch_execution
                    })
            }) {
                (
                    PipelineReadinessState::Stale,
                    Some(PipelineStage::Publication),
                    Some("output_stale".to_string()),
                )
            } else {
                (PipelineReadinessState::Healthy, None, None)
            };

        if readiness_state == PipelineReadinessState::Stale {
            state.was_stale = true;
            state.recovery_observations = 0;
        } else if readiness_state == PipelineReadinessState::Healthy && state.was_stale {
            state.recovery_observations += 1;
            if state.recovery_observations < thresholds.recovery_successes.max(1) {
                readiness_state = PipelineReadinessState::Recovering;
                readiness_reason = Some("recovery_threshold".to_string());
            } else {
                state.was_stale = false;
                state.recovery_observations = 0;
            }
        }

        let transition = (readiness_state, stale_stage);
        if state.last_reported_state != Some(transition) {
            match readiness_state {
                PipelineReadinessState::Stale => tracing::warn!(
                    ?stale_stage,
                    ?readiness_reason,
                    "Pipeline progress became stale"
                ),
                PipelineReadinessState::Healthy if state.last_reported_state.is_some() => {
                    tracing::info!("Pipeline progress recovered")
                }
                _ => {}
            }
            state.last_reported_state = Some(transition);
        }

        PipelineProgressSnapshot {
            readiness_state,
            stale_stage,
            readiness_reason,
            connected_endpoint: state.connected_endpoint.clone(),
            last_reconnect_reason: state.last_reconnect_reason.clone(),
            reconnect_count: state.reconnect_count,
            reconnect_reasons: state.reconnect_reasons.clone(),
            recovery_duration_ms: state.recovery_duration_ms,
            ingress_messages: state.ingress_messages,
            completed_batches: state.completed_batches,
            completed_records: state.completed_records,
            timed_out_batches: state.timed_out_batches,
            input_drops: state.input_drops,
            input_backpressured: state.input_backpressured,
            input_occupancy: state.input_occupancy,
            input_capacity: state.input_capacity,
            active_permits: state.active_batches.len(),
            maximum_permits: state.maximum_permits,
            broadcast_receivers: state.broadcast_receivers,
            successful_broadcasts: state.successful_broadcasts,
            last_valid_ingress_unix_ms: state.last_ingress.as_ref().map(|event| event.unix_ms),
            last_batch_completion_unix_ms: state
                .last_completion
                .as_ref()
                .map(|event| event.unix_ms),
            last_store_unix_ms: state.last_store.as_ref().map(|event| event.unix_ms),
            last_publication_unix_ms: state.last_publication.as_ref().map(|event| event.unix_ms),
            ingress_age_seconds: ingress_age.map(|age| age.as_secs()),
            completion_age_seconds: completion_age.map(|age| age.as_secs()),
            oldest_active_batch_age_seconds: oldest.map(|age| age.as_secs()),
            active_batches,
        }
    }
}

fn event_now() -> TimedEvent {
    TimedEvent {
        monotonic: Instant::now(),
        unix_ms: unix_ms(SystemTime::now()),
    }
}

fn unix_ms(time: SystemTime) -> u64 {
    duration_millis(time.duration_since(UNIX_EPOCH).unwrap_or_default())
}

fn duration_millis(duration: Duration) -> u64 {
    duration.as_millis().min(u64::MAX as u128) as u64
}

#[cfg(test)]
mod tests {
    use super::*;

    fn thresholds() -> ProgressThresholds {
        ProgressThresholds {
            startup_grace: Duration::from_secs(30),
            ingress_idle: Duration::from_secs(10),
            batch_execution: Duration::from_secs(20),
            recovery_successes: 1,
        }
    }

    #[test]
    fn snapshots_cover_starting_healthy_stale_stalled_and_recovered_states() {
        let start = Instant::now();
        let progress = PipelineProgress::with_started_at(4, 100, start);

        let starting = progress.snapshot_at(thresholds(), start + Duration::from_secs(5));
        assert_eq!(starting.readiness_state, PipelineReadinessState::Starting);

        progress.valid_ingress_at(
            start + Duration::from_secs(6),
            UNIX_EPOCH + Duration::from_secs(100),
        );
        let healthy = progress.snapshot_at(thresholds(), start + Duration::from_secs(8));
        assert_eq!(healthy.readiness_state, PipelineReadinessState::Healthy);
        assert_eq!(healthy.ingress_messages, 1);

        let stale = progress.snapshot_at(thresholds(), start + Duration::from_secs(17));
        assert_eq!(stale.stale_stage, Some(PipelineStage::Ingress));
        assert_eq!(stale.readiness_reason.as_deref(), Some("ingress_stale"));

        progress.valid_ingress_at(
            start + Duration::from_secs(18),
            UNIX_EPOCH + Duration::from_secs(101),
        );
        let batch_id = progress.batch_started_at(start + Duration::from_secs(18));
        progress.batch_stage(batch_id, PipelineStage::Storage);
        progress.valid_ingress_at(
            start + Duration::from_secs(38),
            UNIX_EPOCH + Duration::from_secs(102),
        );
        let stalled = progress.snapshot_at(thresholds(), start + Duration::from_secs(39));
        assert_eq!(stalled.stale_stage, Some(PipelineStage::Storage));
        assert_eq!(stalled.readiness_reason.as_deref(), Some("batch_stalled"));

        progress.batch_completed_at(
            batch_id,
            7,
            start + Duration::from_secs(40),
            UNIX_EPOCH + Duration::from_secs(102),
        );
        progress.valid_ingress_at(
            start + Duration::from_secs(40),
            UNIX_EPOCH + Duration::from_secs(102),
        );
        let recovered = progress.snapshot_at(thresholds(), start + Duration::from_secs(41));
        assert_eq!(recovered.readiness_state, PipelineReadinessState::Healthy);
        assert_eq!(recovered.completed_batches, 1);
        assert_eq!(recovered.completed_records, 7);
        assert_eq!(recovered.active_permits, 0);
    }

    #[test]
    fn advancing_ingress_with_stale_output_requires_recovery_streak() {
        let start = Instant::now();
        let progress = PipelineProgress::with_started_at(2, 100, start);
        let thresholds = ProgressThresholds {
            startup_grace: Duration::from_secs(5),
            ingress_idle: Duration::from_secs(10),
            batch_execution: Duration::from_secs(5),
            recovery_successes: 2,
        };
        progress.valid_ingress_at(start + Duration::from_secs(1), UNIX_EPOCH);
        progress.valid_ingress_at(start + Duration::from_secs(7), UNIX_EPOCH);
        let stale = progress.snapshot_at(thresholds, start + Duration::from_secs(7));
        assert_eq!(stale.stale_stage, Some(PipelineStage::Publication));
        assert_eq!(stale.readiness_reason.as_deref(), Some("output_stale"));

        let id = progress.batch_started_at(start + Duration::from_secs(7));
        progress.batch_completed_at(id, 1, start + Duration::from_secs(8), UNIX_EPOCH);
        let recovering = progress.snapshot_at(thresholds, start + Duration::from_secs(8));
        assert_eq!(
            recovering.readiness_state,
            PipelineReadinessState::Recovering
        );
        let healthy = progress.snapshot_at(thresholds, start + Duration::from_secs(9));
        assert_eq!(healthy.readiness_state, PipelineReadinessState::Healthy);
        assert_eq!(
            healthy.broadcast_receivers, 0,
            "no subscribers must not affect readiness"
        );
    }
}
