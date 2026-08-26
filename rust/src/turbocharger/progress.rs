use crate::models::{jetstream::MessageKind, recovery::RecoveryPhase};
use serde::Serialize;
use std::collections::{HashMap, VecDeque};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum PipelineStage {
    Ingress,
    DuplicateDetection,
    Hydration,
    Storage,
    Publication,
    Broadcast,
    CheckpointPersistence,
    EndToEnd,
}

impl PipelineStage {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Ingress => "ingress",
            Self::DuplicateDetection => "duplicate_detection",
            Self::Hydration => "hydration",
            Self::Storage => "storage",
            Self::Publication => "publication",
            Self::Broadcast => "broadcast",
            Self::CheckpointPersistence => "checkpoint_persistence",
            Self::EndToEnd => "end_to_end",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum PipelineStageOutcome {
    Success,
    Error,
    Timeout,
    Cancellation,
}

impl PipelineStageOutcome {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Success => "success",
            Self::Error => "error",
            Self::Timeout => "timeout",
            Self::Cancellation => "cancellation",
        }
    }
}

#[derive(Debug, Clone, Serialize)]
pub struct StageTimingSnapshot {
    pub stage: PipelineStage,
    pub outcome: PipelineStageOutcome,
    pub count: u64,
    pub duration_milliseconds_total: u64,
    pub duration_milliseconds_max: u64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum PipelineReadinessState {
    Starting,
    Recovering,
    Healthy,
    Stale,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum CatchUpEtaState {
    Unavailable,
    Unstable,
    NonConverging,
    Converging,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "snake_case")]
pub struct ActiveBatchSnapshot {
    pub batch_id: u64,
    pub stage: PipelineStage,
    pub age_seconds: u64,
}

#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub struct RejectedIngressSnapshot {
    pub reason: String,
    pub message_kind: String,
    pub rejected_at_unix_ms: u64,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "snake_case")]
pub struct PipelineProgressSnapshot {
    pub recovery_phase: RecoveryPhase,
    pub readiness_state: PipelineReadinessState,
    pub stale_stage: Option<PipelineStage>,
    pub readiness_reason: Option<String>,
    pub connected_endpoint: Option<String>,
    pub last_reconnect_reason: Option<String>,
    pub reconnect_count: u64,
    pub reconnect_reasons: HashMap<String, u64>,
    pub recovery_duration_ms: Option<u64>,
    pub unrecoverable_gap: Option<String>,
    pub last_received_event_time_us: Option<u64>,
    pub last_committed_event_time_us: Option<u64>,
    pub received_lag_us: Option<u64>,
    pub committed_lag_us: Option<u64>,
    pub received_source_velocity: Option<f64>,
    pub committed_source_velocity: Option<f64>,
    pub net_convergence_rate: Option<f64>,
    pub catch_up_eta_state: CatchUpEtaState,
    pub catch_up_eta_seconds: Option<u64>,
    pub live_stability_observations: u32,
    pub endpoint_attempts: HashMap<String, u64>,
    pub endpoint_failures: HashMap<String, u64>,
    pub replayed_events: u64,
    pub duplicate_events: u64,
    pub blocked_send_duration_ms: u64,
    pub ingress_messages: u64,
    pub rejected_ingress: u64,
    pub rejected_ingress_reasons: HashMap<String, u64>,
    pub rejected_ingress_kinds: HashMap<String, u64>,
    pub last_rejected_ingress: Option<RejectedIngressSnapshot>,
    pub completed_batches: u64,
    pub completed_records: u64,
    pub failed_batches: u64,
    pub timed_out_batches: u64,
    pub aborted_batches: u64,
    pub input_drops: u64,
    pub input_backpressured: bool,
    pub input_occupancy: usize,
    pub input_capacity: usize,
    pub active_permits: usize,
    pub running_permit_holders: usize,
    pub queued_batches: usize,
    pub maximum_permits: usize,
    pub batch_completion_throughput_per_second: f64,
    pub record_completion_throughput_per_second: f64,
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
    pub stage_timings: Vec<StageTimingSnapshot>,
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
    stage_started_at: Instant,
    running: bool,
}

#[derive(Debug, Default)]
struct StageTimingAggregate {
    count: u64,
    duration_milliseconds_total: u64,
    duration_milliseconds_max: u64,
}

#[derive(Debug)]
struct ProgressState {
    recovery_phase: RecoveryPhase,
    started_at: Instant,
    connected_endpoint: Option<String>,
    last_reconnect_reason: Option<String>,
    reconnect_started_at: Option<Instant>,
    reconnect_count: u64,
    reconnect_reasons: HashMap<String, u64>,
    recovery_duration_ms: Option<u64>,
    unrecoverable_gap: Option<String>,
    last_received_event_time_us: Option<u64>,
    last_committed_event_time_us: Option<u64>,
    received_lag_us: Option<u64>,
    committed_lag_us: Option<u64>,
    committed_samples: VecDeque<(Instant, u64)>,
    received_samples: VecDeque<(Instant, u64)>,
    live_stability_observations: u32,
    endpoint_attempts: HashMap<String, u64>,
    endpoint_failures: HashMap<String, u64>,
    replayed_events: u64,
    duplicate_events: u64,
    blocked_send_duration_ms: u64,
    last_ingress: Option<TimedEvent>,
    first_ingress_at: Option<Instant>,
    last_completion: Option<TimedEvent>,
    last_store: Option<TimedEvent>,
    last_publication: Option<TimedEvent>,
    ingress_messages: u64,
    rejected_ingress: u64,
    rejected_ingress_reasons: HashMap<String, u64>,
    rejected_ingress_kinds: HashMap<String, u64>,
    last_rejected_ingress: Option<RejectedIngressSnapshot>,
    completed_batches: u64,
    completed_records: u64,
    failed_batches: u64,
    timed_out_batches: u64,
    aborted_batches: u64,
    input_drops: u64,
    input_backpressured: bool,
    input_occupancy: usize,
    input_capacity: usize,
    maximum_permits: usize,
    broadcast_receivers: usize,
    successful_broadcasts: u64,
    next_batch_id: u64,
    active_batches: HashMap<u64, ActiveBatch>,
    batch_completion_samples: VecDeque<Instant>,
    record_completion_samples: VecDeque<(Instant, u64)>,
    stage_timings: HashMap<(PipelineStage, PipelineStageOutcome), StageTimingAggregate>,
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
                recovery_phase: RecoveryPhase::Connecting,
                started_at,
                connected_endpoint: None,
                last_reconnect_reason: None,
                reconnect_started_at: None,
                reconnect_count: 0,
                reconnect_reasons: HashMap::new(),
                recovery_duration_ms: None,
                unrecoverable_gap: None,
                last_received_event_time_us: None,
                last_committed_event_time_us: None,
                received_lag_us: None,
                committed_lag_us: None,
                committed_samples: VecDeque::new(),
                received_samples: VecDeque::new(),
                live_stability_observations: 0,
                endpoint_attempts: HashMap::new(),
                endpoint_failures: HashMap::new(),
                replayed_events: 0,
                duplicate_events: 0,
                blocked_send_duration_ms: 0,
                last_ingress: None,
                first_ingress_at: None,
                last_completion: None,
                last_store: None,
                last_publication: None,
                ingress_messages: 0,
                rejected_ingress: 0,
                rejected_ingress_reasons: HashMap::new(),
                rejected_ingress_kinds: HashMap::new(),
                last_rejected_ingress: None,
                completed_batches: 0,
                completed_records: 0,
                failed_batches: 0,
                timed_out_batches: 0,
                aborted_batches: 0,
                input_drops: 0,
                input_backpressured: false,
                input_occupancy: 0,
                input_capacity,
                maximum_permits,
                broadcast_receivers: 0,
                successful_broadcasts: 0,
                next_batch_id: 1,
                active_batches: HashMap::new(),
                batch_completion_samples: VecDeque::new(),
                record_completion_samples: VecDeque::new(),
                stage_timings: HashMap::new(),
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
        self.connection_established_with_replay(endpoint, false);
    }

    pub fn connecting(&self) {
        let mut state = self.state();
        state.recovery_phase = RecoveryPhase::Connecting;
        state.live_stability_observations = 0;
    }

    pub fn endpoint_attempted(&self, endpoint: &str) {
        *self
            .state()
            .endpoint_attempts
            .entry(endpoint.to_string())
            .or_insert(0) += 1;
    }

    pub fn endpoint_failed(&self, endpoint: &str) {
        *self
            .state()
            .endpoint_failures
            .entry(endpoint.to_string())
            .or_insert(0) += 1;
    }

    pub fn replayed_event(&self) {
        self.state().replayed_events += 1;
    }

    pub fn duplicate_events(&self, count: usize) {
        self.state().duplicate_events += count as u64;
    }

    pub fn connection_established_with_replay(
        &self,
        endpoint: impl Into<String>,
        replay_requested: bool,
    ) {
        let mut state = self.state();
        state.connected_endpoint = Some(endpoint.into());
        state.recovery_phase = if replay_requested {
            RecoveryPhase::Replaying
        } else {
            RecoveryPhase::CatchingUp
        };
        state.live_stability_observations = 0;
    }

    pub fn disconnected(&self, reason: impl Into<String>) {
        let mut state = self.state();
        state.connected_endpoint = None;
        let reason = reason.into();
        state.last_reconnect_reason = Some(reason.clone());
        *state.reconnect_reasons.entry(reason).or_insert(0) += 1;
        state.reconnect_count += 1;
        state.reconnect_started_at = Some(Instant::now());
        state.recovery_phase = RecoveryPhase::Connecting;
        state.live_stability_observations = 0;
    }

    pub fn mark_unrecoverable_gap(&self, reason: impl Into<String>) {
        let mut state = self.state();
        state.unrecoverable_gap = Some(reason.into());
        state.recovery_phase = RecoveryPhase::UnrecoverableGap;
        state.live_stability_observations = 0;
    }

    pub fn clear_unrecoverable_gap(&self) {
        self.state().unrecoverable_gap = None;
    }

    pub fn valid_ingress(&self) -> Option<u64> {
        self.valid_ingress_at(Instant::now(), SystemTime::now())
    }

    pub fn rejected_cursorless_ingress(&self, message_kind: MessageKind) -> u64 {
        let mut state = self.state();
        state.rejected_ingress = state.rejected_ingress.saturating_add(1);
        *state
            .rejected_ingress_reasons
            .entry("missing_time_us".to_string())
            .or_insert(0) += 1;
        *state
            .rejected_ingress_kinds
            .entry(message_kind.as_str().to_string())
            .or_insert(0) += 1;
        state.last_rejected_ingress = Some(RejectedIngressSnapshot {
            reason: "missing_time_us".to_string(),
            message_kind: message_kind.as_str().to_string(),
            rejected_at_unix_ms: unix_ms(SystemTime::now()),
        });
        state.rejected_ingress
    }

    pub fn valid_ingress_event(&self, event_time_us: u64) -> Option<u64> {
        self.valid_ingress_event_at(event_time_us, Instant::now(), SystemTime::now())
    }

    fn valid_ingress_event_at(
        &self,
        event_time_us: u64,
        now: Instant,
        wall: SystemTime,
    ) -> Option<u64> {
        let recovery = self.valid_ingress_at(now, wall);
        let mut state = self.state();
        state.last_received_event_time_us = Some(event_time_us);
        state.received_lag_us = Some(unix_us(wall).saturating_sub(event_time_us));
        state.received_samples.push_back((now, event_time_us));
        while state.received_samples.len() > 12 {
            state.received_samples.pop_front();
        }
        if state.recovery_phase != RecoveryPhase::UnrecoverableGap
            && state.recovery_phase != RecoveryPhase::Live
        {
            state.recovery_phase = RecoveryPhase::CatchingUp;
        }
        recovery
    }

    pub fn checkpoint_committed(
        &self,
        event_time_us: u64,
        live_lag_threshold: Duration,
        required_stability_observations: u32,
    ) -> bool {
        self.checkpoint_committed_at(
            event_time_us,
            live_lag_threshold,
            required_stability_observations,
            Instant::now(),
            SystemTime::now(),
        )
    }

    fn checkpoint_committed_at(
        &self,
        event_time_us: u64,
        live_lag_threshold: Duration,
        required_stability_observations: u32,
        now: Instant,
        wall: SystemTime,
    ) -> bool {
        let mut state = self.state();
        state.last_committed_event_time_us = Some(event_time_us);
        let committed_lag_us = unix_us(wall).saturating_sub(event_time_us);
        state.committed_lag_us = Some(committed_lag_us);
        state.committed_samples.push_back((now, event_time_us));
        while state.committed_samples.len() > 12 {
            state.committed_samples.pop_front();
        }
        if state.recovery_phase == RecoveryPhase::UnrecoverableGap {
            return false;
        }

        if committed_lag_us <= live_lag_threshold.as_micros().min(u64::MAX as u128) as u64 {
            state.live_stability_observations = state.live_stability_observations.saturating_add(1);
            if state.live_stability_observations >= required_stability_observations.max(1) {
                state.recovery_phase = RecoveryPhase::Live;
                if let Some(started) = state.reconnect_started_at.take() {
                    state.recovery_duration_ms =
                        Some(duration_millis(now.saturating_duration_since(started)));
                }
                return true;
            }
        } else {
            state.live_stability_observations = 0;
        }
        state.recovery_phase = RecoveryPhase::CatchingUp;
        false
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

    pub fn input_blocked(&self, occupancy: usize) {
        let mut state = self.state();
        state.input_backpressured = true;
        state.input_occupancy = occupancy;
    }

    pub fn input_recovered(&self, occupancy: usize) {
        let mut state = self.state();
        state.input_backpressured = false;
        state.input_occupancy = occupancy;
    }

    pub fn input_send_completed(&self, occupancy: usize, blocked_duration: Duration) {
        let mut state = self.state();
        state.input_occupancy = occupancy;
        state.blocked_send_duration_ms = state
            .blocked_send_duration_ms
            .saturating_add(duration_millis(blocked_duration));
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
                stage: PipelineStage::Ingress,
                started_at: now,
                stage_started_at: now,
                running: false,
            },
        );
        id
    }

    pub fn batch_running(&self, batch_id: u64) {
        if let Some(batch) = self.state().active_batches.get_mut(&batch_id) {
            batch.running = true;
            batch.stage = PipelineStage::DuplicateDetection;
            batch.stage_started_at = Instant::now();
        }
    }

    pub fn batch_stage(&self, batch_id: u64, stage: PipelineStage) {
        self.batch_stage_at(batch_id, stage, Instant::now());
    }

    fn batch_stage_at(&self, batch_id: u64, stage: PipelineStage, now: Instant) {
        let mut state = self.state();
        let timing = state.active_batches.get(&batch_id).map(|batch| {
            (
                batch.stage,
                now.saturating_duration_since(batch.stage_started_at),
            )
        });
        if let Some((previous, duration)) = timing {
            record_stage_timing(
                &mut state,
                previous,
                PipelineStageOutcome::Success,
                duration,
            );
        }
        if let Some(batch) = state.active_batches.get_mut(&batch_id) {
            batch.stage = stage;
            batch.stage_started_at = now;
        }
    }

    pub fn record_stage_duration(
        &self,
        stage: PipelineStage,
        outcome: PipelineStageOutcome,
        duration: Duration,
    ) {
        record_stage_timing(&mut self.state(), stage, outcome, duration);
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
        let Some(batch) = state.active_batches.remove(&batch_id) else {
            return;
        };
        record_stage_timing(
            &mut state,
            batch.stage,
            PipelineStageOutcome::Success,
            now.saturating_duration_since(batch.stage_started_at),
        );
        record_stage_timing(
            &mut state,
            PipelineStage::EndToEnd,
            PipelineStageOutcome::Success,
            now.saturating_duration_since(batch.started_at),
        );
        state.completed_batches += 1;
        state.completed_records += records as u64;
        state.last_completion = Some(TimedEvent {
            monotonic: now,
            unix_ms: unix_ms(wall),
        });
        state.batch_completion_samples.push_back(now);
        let completed_records = state.completed_records;
        state
            .record_completion_samples
            .push_back((now, completed_records));
        while state.batch_completion_samples.len() > 128 {
            state.batch_completion_samples.pop_front();
        }
        while state.record_completion_samples.len() > 128 {
            state.record_completion_samples.pop_front();
        }
    }

    pub fn batch_timed_out(&self, batch_id: u64) {
        let mut state = self.state();
        if let Some(batch) = state.active_batches.remove(&batch_id) {
            let now = Instant::now();
            record_terminal_timings(&mut state, &batch, PipelineStageOutcome::Timeout, now);
            state.timed_out_batches += 1;
        }
    }

    pub fn batch_failed(&self, batch_id: u64) {
        let mut state = self.state();
        if let Some(batch) = state.active_batches.remove(&batch_id) {
            let now = Instant::now();
            record_terminal_timings(&mut state, &batch, PipelineStageOutcome::Error, now);
            state.failed_batches += 1;
        }
    }

    pub fn batch_aborted(&self, batch_id: u64) {
        let mut state = self.state();
        if let Some(batch) = state.active_batches.remove(&batch_id) {
            let now = Instant::now();
            record_terminal_timings(&mut state, &batch, PipelineStageOutcome::Cancellation, now);
            state.aborted_batches += 1;
        }
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
        while state
            .batch_completion_samples
            .front()
            .is_some_and(|sample| now.saturating_duration_since(*sample) > Duration::from_secs(60))
        {
            state.batch_completion_samples.pop_front();
        }
        while state
            .record_completion_samples
            .front()
            .is_some_and(|(sample, _)| {
                now.saturating_duration_since(*sample) > Duration::from_secs(60)
            })
        {
            state.record_completion_samples.pop_front();
        }
        let running_permit_holders = state
            .active_batches
            .values()
            .filter(|batch| batch.running)
            .count();
        let queued_batches = state
            .active_batches
            .len()
            .saturating_sub(running_permit_holders);
        let completion_throughput = completion_throughput(&state.batch_completion_samples, now);
        let record_completion_throughput = record_throughput(&state.record_completion_samples);
        let received_source_velocity = source_velocity(&state.received_samples);
        let (
            committed_source_velocity,
            net_convergence_rate,
            catch_up_eta_state,
            catch_up_eta_seconds,
        ) = convergence(&state.committed_samples, state.committed_lag_us);

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

        let mut stage_timings = state
            .stage_timings
            .iter()
            .map(|(&(stage, outcome), timing)| StageTimingSnapshot {
                stage,
                outcome,
                count: timing.count,
                duration_milliseconds_total: timing.duration_milliseconds_total,
                duration_milliseconds_max: timing.duration_milliseconds_max,
            })
            .collect::<Vec<_>>();
        stage_timings.sort_by_key(|timing| (timing.stage as u8, timing.outcome as u8));

        PipelineProgressSnapshot {
            recovery_phase: state.recovery_phase,
            readiness_state,
            stale_stage,
            readiness_reason,
            connected_endpoint: state.connected_endpoint.clone(),
            last_reconnect_reason: state.last_reconnect_reason.clone(),
            reconnect_count: state.reconnect_count,
            reconnect_reasons: state.reconnect_reasons.clone(),
            recovery_duration_ms: state.recovery_duration_ms,
            unrecoverable_gap: state.unrecoverable_gap.clone(),
            last_received_event_time_us: state.last_received_event_time_us,
            last_committed_event_time_us: state.last_committed_event_time_us,
            received_lag_us: state.received_lag_us,
            committed_lag_us: state.committed_lag_us,
            received_source_velocity,
            committed_source_velocity,
            net_convergence_rate,
            catch_up_eta_state,
            catch_up_eta_seconds,
            live_stability_observations: state.live_stability_observations,
            endpoint_attempts: state.endpoint_attempts.clone(),
            endpoint_failures: state.endpoint_failures.clone(),
            replayed_events: state.replayed_events,
            duplicate_events: state.duplicate_events,
            blocked_send_duration_ms: state.blocked_send_duration_ms,
            ingress_messages: state.ingress_messages,
            rejected_ingress: state.rejected_ingress,
            rejected_ingress_reasons: state.rejected_ingress_reasons.clone(),
            rejected_ingress_kinds: state.rejected_ingress_kinds.clone(),
            last_rejected_ingress: state.last_rejected_ingress.clone(),
            completed_batches: state.completed_batches,
            completed_records: state.completed_records,
            failed_batches: state.failed_batches,
            timed_out_batches: state.timed_out_batches,
            aborted_batches: state.aborted_batches,
            input_drops: state.input_drops,
            input_backpressured: state.input_backpressured,
            input_occupancy: state.input_occupancy,
            input_capacity: state.input_capacity,
            active_permits: running_permit_holders,
            running_permit_holders,
            queued_batches,
            maximum_permits: state.maximum_permits,
            batch_completion_throughput_per_second: completion_throughput,
            record_completion_throughput_per_second: record_completion_throughput,
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
            stage_timings,
        }
    }
}

fn record_terminal_timings(
    state: &mut ProgressState,
    batch: &ActiveBatch,
    outcome: PipelineStageOutcome,
    now: Instant,
) {
    record_stage_timing(
        state,
        batch.stage,
        outcome,
        now.saturating_duration_since(batch.stage_started_at),
    );
    record_stage_timing(
        state,
        PipelineStage::EndToEnd,
        outcome,
        now.saturating_duration_since(batch.started_at),
    );
}

fn record_stage_timing(
    state: &mut ProgressState,
    stage: PipelineStage,
    outcome: PipelineStageOutcome,
    duration: Duration,
) {
    let milliseconds = duration_millis(duration);
    let timing = state.stage_timings.entry((stage, outcome)).or_default();
    timing.count = timing.count.saturating_add(1);
    timing.duration_milliseconds_total = timing
        .duration_milliseconds_total
        .saturating_add(milliseconds);
    timing.duration_milliseconds_max = timing.duration_milliseconds_max.max(milliseconds);
}

fn convergence(
    samples: &VecDeque<(Instant, u64)>,
    committed_lag_us: Option<u64>,
) -> (Option<f64>, Option<f64>, CatchUpEtaState, Option<u64>) {
    let (Some((first_at, _)), Some((last_at, _))) = (samples.front(), samples.back()) else {
        return (None, None, CatchUpEtaState::Unavailable, None);
    };
    let wall_seconds = last_at.saturating_duration_since(*first_at).as_secs_f64();
    if samples.len() < 3 || wall_seconds <= f64::EPSILON {
        return (None, None, CatchUpEtaState::Unavailable, None);
    }
    let Some(velocity) = source_velocity(samples) else {
        return (None, None, CatchUpEtaState::Unstable, None);
    };
    let net = velocity - 1.0;
    if net <= 0.0 {
        return (
            Some(velocity),
            Some(net),
            CatchUpEtaState::NonConverging,
            None,
        );
    }
    let eta = committed_lag_us.map(|lag| (lag as f64 / 1_000_000.0 / net).ceil() as u64);
    (Some(velocity), Some(net), CatchUpEtaState::Converging, eta)
}

fn source_velocity(samples: &VecDeque<(Instant, u64)>) -> Option<f64> {
    let (Some((first_at, first_source)), Some((last_at, last_source))) =
        (samples.front(), samples.back())
    else {
        return None;
    };
    if samples.len() < 3 || last_source < first_source {
        return None;
    }
    let wall_seconds = last_at.saturating_duration_since(*first_at).as_secs_f64();
    (wall_seconds > f64::EPSILON)
        .then_some((*last_source - *first_source) as f64 / 1_000_000.0 / wall_seconds)
}

fn record_throughput(samples: &VecDeque<(Instant, u64)>) -> f64 {
    let (Some((first_at, first_count)), Some((last_at, last_count))) =
        (samples.front(), samples.back())
    else {
        return 0.0;
    };
    let elapsed = last_at.saturating_duration_since(*first_at).as_secs_f64();
    if elapsed <= f64::EPSILON {
        return 0.0;
    }
    last_count.saturating_sub(*first_count) as f64 / elapsed
}

fn completion_throughput(samples: &VecDeque<Instant>, now: Instant) -> f64 {
    let Some(first) = samples.front() else {
        return 0.0;
    };
    let elapsed = now.saturating_duration_since(*first).as_secs_f64().max(1.0);
    samples.len() as f64 / elapsed
}

pub struct BatchLifecycle {
    progress: Arc<PipelineProgress>,
    batch_id: u64,
    finalized: bool,
}

impl BatchLifecycle {
    pub fn new(progress: Arc<PipelineProgress>, batch_id: u64) -> Self {
        Self {
            progress,
            batch_id,
            finalized: false,
        }
    }

    pub fn completed(mut self, records: usize) {
        self.progress.batch_completed(self.batch_id, records);
        self.finalized = true;
    }

    pub fn failed(mut self) {
        self.progress.batch_failed(self.batch_id);
        self.finalized = true;
    }

    pub fn timed_out(mut self) {
        self.progress.batch_timed_out(self.batch_id);
        self.finalized = true;
    }
}

impl Drop for BatchLifecycle {
    fn drop(&mut self) {
        if !self.finalized {
            self.progress.batch_aborted(self.batch_id);
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

fn unix_us(time: SystemTime) -> u64 {
    time.duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_micros()
        .min(u64::MAX as u128) as u64
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
    fn rejected_cursorless_ingress_is_bounded_and_privacy_safe() {
        let progress = PipelineProgress::new(2, 10);

        progress.rejected_cursorless_ingress(MessageKind::Commit);
        progress.rejected_cursorless_ingress(MessageKind::Commit);
        let snapshot = progress.snapshot(thresholds());

        assert_eq!(snapshot.rejected_ingress, 2);
        assert_eq!(snapshot.rejected_ingress_reasons.len(), 1);
        assert_eq!(snapshot.rejected_ingress_kinds.len(), 1);
        assert_eq!(
            snapshot.last_rejected_ingress,
            Some(RejectedIngressSnapshot {
                reason: "missing_time_us".to_string(),
                message_kind: "commit".to_string(),
                rejected_at_unix_ms: snapshot
                    .last_rejected_ingress
                    .as_ref()
                    .expect("rejection summary")
                    .rejected_at_unix_ms,
            })
        );
    }

    #[test]
    fn batch_terminal_transitions_are_idempotent() {
        let progress = PipelineProgress::new(2, 10);
        let batch_id = progress.batch_started();

        progress.batch_completed(batch_id, 3);
        progress.batch_failed(batch_id);
        progress.batch_timed_out(batch_id);
        progress.batch_aborted(batch_id);
        let snapshot = progress.snapshot(thresholds());

        assert_eq!(snapshot.completed_batches, 1);
        assert_eq!(snapshot.completed_records, 3);
        assert_eq!(snapshot.failed_batches, 0);
        assert_eq!(snapshot.timed_out_batches, 0);
        assert_eq!(snapshot.aborted_batches, 0);
        assert_eq!(snapshot.active_permits, 0);
        assert_eq!(snapshot.oldest_active_batch_age_seconds, None);
    }

    #[test]
    fn dropping_batch_lifecycle_finalizes_it_as_aborted() {
        let progress = Arc::new(PipelineProgress::new(2, 10));
        let batch_id = progress.batch_started();

        drop(BatchLifecycle::new(Arc::clone(&progress), batch_id));
        let snapshot = progress.snapshot(thresholds());

        assert_eq!(snapshot.aborted_batches, 1);
        assert_eq!(snapshot.completed_records, 0);
        assert_eq!(snapshot.active_permits, 0);
        assert_eq!(snapshot.oldest_active_batch_age_seconds, None);
    }

    #[tokio::test]
    async fn task_panic_drops_lifecycle_and_clears_active_batch() {
        let progress = Arc::new(PipelineProgress::new(2, 10));
        let batch_id = progress.batch_started();
        let lifecycle = BatchLifecycle::new(Arc::clone(&progress), batch_id);

        let join_error = tokio::spawn(async move {
            let _lifecycle = lifecycle;
            panic!("simulated batch panic");
        })
        .await
        .expect_err("task should panic");
        assert!(join_error.is_panic());

        let snapshot = progress.snapshot(thresholds());
        assert_eq!(snapshot.aborted_batches, 1);
        assert_eq!(snapshot.active_permits, 0);
        assert_eq!(snapshot.oldest_active_batch_age_seconds, None);
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

    #[test]
    fn committed_lag_requires_stable_observations_before_live() {
        let start = Instant::now();
        let wall = UNIX_EPOCH + Duration::from_secs(100);
        let event_time_us = 95_000_000;
        let progress = PipelineProgress::with_started_at(2, 100, start);
        progress.disconnected("test_recovery");
        progress.connection_established_with_replay("endpoint-a", true);
        progress.valid_ingress_event_at(event_time_us, start, wall);

        assert!(!progress.checkpoint_committed_at(
            event_time_us,
            Duration::from_secs(10),
            3,
            start,
            wall,
        ));
        assert!(!progress.checkpoint_committed_at(
            event_time_us,
            Duration::from_secs(10),
            3,
            start + Duration::from_secs(1),
            wall,
        ));
        assert!(progress.checkpoint_committed_at(
            event_time_us,
            Duration::from_secs(10),
            3,
            start + Duration::from_secs(2),
            wall,
        ));

        let snapshot = progress.snapshot_at(thresholds(), start + Duration::from_secs(2));
        assert_eq!(snapshot.recovery_phase, RecoveryPhase::Live);
        assert_eq!(snapshot.committed_lag_us, Some(5_000_000));
        assert_eq!(snapshot.live_stability_observations, 3);
    }

    #[test]
    fn non_converging_committed_lag_remains_catching_up() {
        let start = Instant::now();
        let wall = UNIX_EPOCH + Duration::from_secs(100);
        let progress = PipelineProgress::with_started_at(2, 100, start);
        progress.connection_established_with_replay("endpoint-a", true);
        progress.valid_ingress_event_at(99_000_000, start, wall);

        for observation in 0..5 {
            assert!(!progress.checkpoint_committed_at(
                50_000_000,
                Duration::from_secs(30),
                3,
                start + Duration::from_secs(observation),
                wall,
            ));
        }

        let snapshot = progress.snapshot_at(thresholds(), start + Duration::from_secs(5));
        assert_eq!(snapshot.recovery_phase, RecoveryPhase::CatchingUp);
        assert_eq!(snapshot.committed_lag_us, Some(50_000_000));
        assert_eq!(snapshot.live_stability_observations, 0);
        assert_eq!(snapshot.catch_up_eta_state, CatchUpEtaState::NonConverging);
        assert_eq!(snapshot.catch_up_eta_seconds, None);
    }

    #[test]
    fn stable_committed_velocity_reports_net_convergence_and_eta() {
        let start = Instant::now();
        let progress = PipelineProgress::with_started_at(2, 100, start);
        progress.connection_established_with_replay("endpoint-a", true);

        for (seconds, source_seconds) in [(0, 50), (10, 70), (20, 90)] {
            progress.checkpoint_committed_at(
                source_seconds * 1_000_000,
                Duration::from_secs(5),
                3,
                start + Duration::from_secs(seconds),
                UNIX_EPOCH + Duration::from_secs(100 + seconds),
            );
        }

        let snapshot = progress.snapshot_at(thresholds(), start + Duration::from_secs(20));
        assert_eq!(snapshot.committed_source_velocity, Some(2.0));
        assert_eq!(snapshot.net_convergence_rate, Some(1.0));
        assert_eq!(snapshot.catch_up_eta_state, CatchUpEtaState::Converging);
        assert_eq!(snapshot.catch_up_eta_seconds, Some(30));
    }

    #[test]
    fn too_few_committed_samples_leave_catch_up_eta_unavailable() {
        let start = Instant::now();
        let wall = UNIX_EPOCH + Duration::from_secs(100);
        let progress = PipelineProgress::with_started_at(2, 100, start);

        for seconds in 0..2 {
            progress.checkpoint_committed_at(
                (50 + seconds * 10) * 1_000_000,
                Duration::from_secs(5),
                3,
                start + Duration::from_secs(seconds),
                wall,
            );
        }

        let snapshot = progress.snapshot_at(thresholds(), start + Duration::from_secs(2));
        assert_eq!(snapshot.catch_up_eta_state, CatchUpEtaState::Unavailable);
        assert_eq!(snapshot.committed_source_velocity, None);
        assert_eq!(snapshot.net_convergence_rate, None);
        assert_eq!(snapshot.catch_up_eta_seconds, None);
    }

    #[test]
    fn non_monotonic_source_timestamps_leave_catch_up_eta_unstable() {
        let start = Instant::now();
        let progress = PipelineProgress::with_started_at(2, 100, start);

        // Three observations whose source timestamps regress (e.g. replayed or
        // reordered commits): enough samples for an estimate, but not stable.
        for (seconds, source_seconds) in [(0, 50), (10, 70), (20, 40)] {
            progress.checkpoint_committed_at(
                source_seconds * 1_000_000,
                Duration::from_secs(5),
                3,
                start + Duration::from_secs(seconds),
                UNIX_EPOCH + Duration::from_secs(100 + seconds),
            );
        }

        let snapshot = progress.snapshot_at(thresholds(), start + Duration::from_secs(20));
        assert_eq!(snapshot.catch_up_eta_state, CatchUpEtaState::Unstable);
        assert_eq!(snapshot.committed_source_velocity, None);
        assert_eq!(snapshot.catch_up_eta_seconds, None);
    }

    #[test]
    fn stage_timings_finalize_with_the_bounded_outcome_for_each_terminal_path() {
        let progress = Arc::new(PipelineProgress::new(4, 10));

        // Success path: hydration stage closed, end-to-end success recorded.
        let success = progress.batch_started();
        progress.batch_running(success);
        progress.batch_stage(success, PipelineStage::Hydration);
        progress.batch_completed(success, 2);

        // Error path: storage stage closed with the error outcome.
        let failed = progress.batch_started();
        progress.batch_running(failed);
        progress.batch_stage(failed, PipelineStage::Storage);
        progress.batch_failed(failed);

        // Timeout path.
        let timed_out = progress.batch_started();
        progress.batch_running(timed_out);
        progress.batch_stage(timed_out, PipelineStage::Publication);
        progress.batch_timed_out(timed_out);

        // Cancellation path (dropped lifecycle, e.g. task panic or shutdown).
        let cancelled = progress.batch_started();
        progress.batch_running(cancelled);
        drop(BatchLifecycle::new(Arc::clone(&progress), cancelled));

        let snapshot = progress.snapshot(thresholds());
        let find = |stage: PipelineStage, outcome: PipelineStageOutcome| {
            snapshot
                .stage_timings
                .iter()
                .find(|timing| timing.stage == stage && timing.outcome == outcome)
        };

        assert_eq!(
            find(PipelineStage::Hydration, PipelineStageOutcome::Success).map(|t| t.count),
            Some(1)
        );
        assert_eq!(
            find(PipelineStage::EndToEnd, PipelineStageOutcome::Success).map(|t| t.count),
            Some(1)
        );
        assert_eq!(
            find(PipelineStage::Storage, PipelineStageOutcome::Error).map(|t| t.count),
            Some(1)
        );
        assert_eq!(
            find(PipelineStage::EndToEnd, PipelineStageOutcome::Error).map(|t| t.count),
            Some(1)
        );
        assert_eq!(
            find(PipelineStage::Publication, PipelineStageOutcome::Timeout).map(|t| t.count),
            Some(1)
        );
        assert_eq!(
            find(PipelineStage::EndToEnd, PipelineStageOutcome::Timeout).map(|t| t.count),
            Some(1)
        );
        assert_eq!(
            find(
                PipelineStage::DuplicateDetection,
                PipelineStageOutcome::Cancellation
            )
            .map(|t| t.count),
            Some(1)
        );
        assert_eq!(
            find(PipelineStage::EndToEnd, PipelineStageOutcome::Cancellation).map(|t| t.count),
            Some(1)
        );

        for timing in &snapshot.stage_timings {
            let serialized = serde_json::to_string(&(
                serde_json::to_value(timing.stage).unwrap(),
                serde_json::to_value(timing.outcome).unwrap(),
            ))
            .unwrap();
            assert!(
                !serialized.contains("did:")
                    && !serialized.contains("at://")
                    && !serialized.chars().any(|c| c == '?'),
                "stage timing labels must stay bounded: {serialized}"
            );
        }
    }

    #[test]
    fn queued_batches_are_not_reported_as_running_permit_holders() {
        let start = Instant::now();
        let progress = PipelineProgress::with_started_at(1, 10, start);
        let batch_id = progress.batch_started_at(start);
        let queued = progress.snapshot_at(thresholds(), start);
        assert_eq!(queued.queued_batches, 1);
        assert_eq!(queued.running_permit_holders, 0);
        assert_eq!(queued.active_permits, 0);

        progress.batch_running(batch_id);
        let running = progress.snapshot_at(thresholds(), start + Duration::from_secs(1));
        assert_eq!(running.queued_batches, 0);
        assert_eq!(running.running_permit_holders, 1);
        assert!(running.running_permit_holders <= running.maximum_permits);
    }

    #[test]
    fn upstream_downtime_and_replay_backlog_stays_recovering_until_commit_converges() {
        let start = Instant::now();
        let wall = UNIX_EPOCH + Duration::from_secs(100);
        let progress = PipelineProgress::with_started_at(2, 100, start);
        progress.disconnected("upstream_downtime");
        assert_eq!(progress.state().recovery_phase, RecoveryPhase::Connecting);

        progress.connection_established_with_replay("healthy-alternate", true);
        assert_eq!(progress.state().recovery_phase, RecoveryPhase::Replaying);
        progress.valid_ingress_event_at(60_000_000, start, wall);
        assert!(!progress.checkpoint_committed_at(
            60_000_000,
            Duration::from_secs(30),
            2,
            start,
            wall,
        ));
        assert_eq!(progress.state().recovery_phase, RecoveryPhase::CatchingUp);

        progress.valid_ingress_event_at(95_000_000, start + Duration::from_secs(1), wall);
        assert!(!progress.checkpoint_committed_at(
            80_000_000,
            Duration::from_secs(30),
            2,
            start + Duration::from_secs(1),
            wall,
        ));
        assert_eq!(progress.state().recovery_phase, RecoveryPhase::CatchingUp);

        assert!(progress.checkpoint_committed_at(
            95_000_000,
            Duration::from_secs(30),
            2,
            start + Duration::from_secs(2),
            wall,
        ));
        assert_eq!(progress.state().recovery_phase, RecoveryPhase::Live);
    }
}
