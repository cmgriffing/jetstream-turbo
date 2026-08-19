use crate::stats::comparison::{
    ComparisonEngine, ComparisonStreamState, ObservationConfig, PairwiseComparisons,
};
use crate::stream::{ConnectionStatus, ReconnectReason, StreamId, StreamMessage};
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::collections::VecDeque;
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::sync::broadcast;

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum LiveLatencyMetric {
    ConnectionLatency,
}

#[derive(Debug, Clone, Copy, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum DeliveryMode {
    Live,
    CatchingUp,
    #[default]
    Unknown,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum ComparisonIneligibilityReason {
    CatchingUp,
    UnknownMode,
    MissingEventTimeCoverage,
    WatermarkSkew,
    Disconnected,
    IdleDelivery,
    MissingSharedCoverage,
    IncompleteIdentityCoverage,
    SettlementPending,
    LegacyUnknown,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
pub struct StreamEventTimeSnapshot {
    pub source_watermark_us: Option<u64>,
    pub source_lag_us: Option<u64>,
    pub delivery_mode: DeliveryMode,
    pub event_time_coverage: bool,
    pub clock_skew_us: u64,
}

impl Default for StreamEventTimeSnapshot {
    fn default() -> Self {
        Self {
            source_watermark_us: None,
            source_lag_us: None,
            delivery_mode: DeliveryMode::Unknown,
            event_time_coverage: false,
            clock_skew_us: 0,
        }
    }
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
pub struct ComparisonEligibility {
    pub eligible: bool,
    pub reason: Option<ComparisonIneligibilityReason>,
    pub watermark_skew_us: Option<u64>,
}

impl Default for ComparisonEligibility {
    fn default() -> Self {
        Self {
            eligible: false,
            reason: Some(ComparisonIneligibilityReason::UnknownMode),
            watermark_skew_us: None,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StreamStats {
    pub stream_a: u64,
    pub stream_b: u64,
    pub counting_started_at: DateTime<Utc>,
    pub delta: i64,
    pub rate_a: f64,
    pub rate_b: f64,
    pub stream_a_name: String,
    pub stream_b_name: String,
    pub timestamp: DateTime<Utc>,
    pub uptime_a: f64,
    pub uptime_b: f64,
    pub uptime_a_all_time: f64,
    pub uptime_b_all_time: f64,
    pub downtime_a: f64,
    pub downtime_b: f64,
    pub connected_a: bool,
    pub connected_b: bool,
    pub connect_time_a_ms: u64,
    pub connect_time_b_ms: u64,
    pub live_latency_metric: LiveLatencyMetric,
    pub live_latency_a_ms: f64,
    pub live_latency_b_ms: f64,
    pub delivery_latency_a_ms: f64,
    pub delivery_latency_b_ms: f64,
    pub mttr_a_ms: u64,
    pub mttr_b_ms: u64,
    pub current_streak_a: f64,
    pub current_streak_b: f64,
    pub baseline_1_name: String,
    pub baseline_2_name: String,
    pub baseline_1: u64,
    pub baseline_2: u64,
    pub rate_baseline_1: f64,
    pub rate_baseline_2: f64,
    pub connected_baseline_1: bool,
    pub connected_baseline_2: bool,
    pub uptime_baseline_1_all_time: f64,
    pub uptime_baseline_2_all_time: f64,
    pub current_streak_baseline_1: f64,
    pub current_streak_baseline_2: f64,
    pub delivery_available_a: bool,
    pub delivery_available_b: bool,
    pub delivery_available_baseline_1: bool,
    pub delivery_available_baseline_2: bool,
    pub transport_uptime_a_all_time: f64,
    pub transport_uptime_b_all_time: f64,
    pub transport_uptime_baseline_1_all_time: f64,
    pub transport_uptime_baseline_2_all_time: f64,
    pub delivery_uptime_a_all_time: f64,
    pub delivery_uptime_b_all_time: f64,
    pub delivery_uptime_baseline_1_all_time: f64,
    pub delivery_uptime_baseline_2_all_time: f64,
    pub reconnect_reason_a: Option<ReconnectReason>,
    pub reconnect_reason_b: Option<ReconnectReason>,
    pub reconnect_reason_baseline_1: Option<ReconnectReason>,
    pub reconnect_reason_baseline_2: Option<ReconnectReason>,
    pub data_idle_reconnects_a: u64,
    pub data_idle_reconnects_b: u64,
    pub data_idle_reconnects_baseline_1: u64,
    pub data_idle_reconnects_baseline_2: u64,
    pub client_recovery_a_ms: u64,
    pub client_recovery_b_ms: u64,
    pub client_recovery_baseline_1_ms: u64,
    pub client_recovery_baseline_2_ms: u64,
    pub event_time_a: StreamEventTimeSnapshot,
    pub event_time_b: StreamEventTimeSnapshot,
    pub event_time_baseline_1: StreamEventTimeSnapshot,
    pub event_time_baseline_2: StreamEventTimeSnapshot,
    pub comparison: ComparisonEligibility,
    pub comparisons: PairwiseComparisons,
    pub watermark_skew_threshold_us: u64,
}

pub struct StatsAggregator {
    tx: broadcast::Sender<StreamStats>,
    stream_a_name: String,
    stream_b_name: String,
    baseline_1_name: String,
    baseline_2_name: String,
}

impl StatsAggregator {
    pub fn new(
        stream_a_name: String,
        stream_b_name: String,
        baseline_1_name: String,
        baseline_2_name: String,
    ) -> Self {
        let (tx, _) = broadcast::channel(16);
        Self {
            tx,
            stream_a_name,
            stream_b_name,
            baseline_1_name,
            baseline_2_name,
        }
    }

    pub fn subscribe(&self) -> broadcast::Receiver<StreamStats> {
        self.tx.subscribe()
    }

    pub fn sender(&self) -> broadcast::Sender<StreamStats> {
        self.tx.clone()
    }

    pub fn process(
        &self,
        stats: &Arc<std::sync::RwLock<StreamStatsInternal>>,
        uptime: &Arc<std::sync::RwLock<UptimeTracker>>,
    ) {
        let tx = self.tx.clone();
        let stats = Arc::clone(stats);
        let uptime = Arc::clone(uptime);
        let stream_a_name = self.stream_a_name.clone();
        let stream_b_name = self.stream_b_name.clone();
        let baseline_1_name = self.baseline_1_name.clone();
        let baseline_2_name = self.baseline_2_name.clone();
        let counting_started_at = Utc::now();

        tokio::spawn(async move {
            let mut interval = tokio::time::interval(std::time::Duration::from_millis(100));

            loop {
                interval.tick().await;

                let internal = stats.read().unwrap();

                let (
                    rate_a,
                    rate_b,
                    connected_a,
                    connected_b,
                    connect_time_a,
                    connect_time_b,
                    live_latency_a,
                    live_latency_b,
                    delivery_latency_a,
                    delivery_latency_b,
                    mttr_a,
                    mttr_b,
                ) = {
                    let up = uptime.read().unwrap();
                    let (rate_a, rate_b) = up.get_average_rates();
                    (
                        rate_a,
                        rate_b,
                        up.connected_a,
                        up.connected_b,
                        up.get_avg_connect_time_a(),
                        up.get_avg_connect_time_b(),
                        up.get_connection_latency_a_ms(),
                        up.get_connection_latency_b_ms(),
                        up.get_delivery_latency_a_ms(),
                        up.get_delivery_latency_b_ms(),
                        up.get_mttr_a_ms(),
                        up.get_mttr_b_ms(),
                    )
                };

                let (
                    event_time_a,
                    event_time_b,
                    event_time_baseline_1,
                    event_time_baseline_2,
                    comparison,
                    comparisons,
                ) = {
                    let up = uptime.read().unwrap();
                    let event_time_a = up.event_time_snapshot(StreamId::A);
                    let event_time_b = up.event_time_snapshot(StreamId::B);
                    (
                        event_time_a,
                        event_time_b,
                        up.event_time_snapshot(StreamId::Baseline1),
                        up.event_time_snapshot(StreamId::Baseline2),
                        comparison_eligibility(
                            &event_time_a,
                            &event_time_b,
                            up.watermark_skew_threshold,
                        ),
                        up.pairwise_comparisons(),
                    )
                };

                let (
                    uptime_a,
                    uptime_b,
                    streak_a,
                    streak_b,
                    (uptime_a_all_time, uptime_b_all_time),
                    (downtime_a, downtime_b),
                ) = {
                    let up = uptime.read().unwrap();
                    let (a, b) = up.get_current_uptime_percentage();
                    (
                        a,
                        b,
                        up.get_current_streak_a(),
                        up.get_current_streak_b(),
                        up.get_all_time_uptime_from_downtime(),
                        up.get_all_time_downtime_seconds(),
                    )
                };

                let (
                    baseline_1_total,
                    baseline_2_total,
                    rate_baseline_1,
                    rate_baseline_2,
                    connected_baseline_1,
                    connected_baseline_2,
                    uptime_baseline_1_all_time,
                    uptime_baseline_2_all_time,
                    streak_baseline_1,
                    streak_baseline_2,
                ) = {
                    let up = uptime.read().unwrap();
                    let (rate_b1, rate_b2) = up.get_baseline_rates();
                    let (uptime_b1, uptime_b2) = up.get_baseline_uptime_percentages();
                    (
                        up.baseline_1.total_messages,
                        up.baseline_2.total_messages,
                        rate_b1,
                        rate_b2,
                        up.baseline_1.connected,
                        up.baseline_2.connected,
                        uptime_b1,
                        uptime_b2,
                        up.get_baseline_1_streak(),
                        up.get_baseline_2_streak(),
                    )
                };

                let (
                    availability_a,
                    availability_b,
                    availability_baseline_1,
                    availability_baseline_2,
                ) = {
                    let up = uptime.read().unwrap();
                    (
                        up.availability_snapshot(StreamId::A),
                        up.availability_snapshot(StreamId::B),
                        up.availability_snapshot(StreamId::Baseline1),
                        up.availability_snapshot(StreamId::Baseline2),
                    )
                };

                let stats_snapshot = StreamStats {
                    stream_a: internal.total_a,
                    stream_b: internal.total_b,
                    counting_started_at,
                    delta: internal.total_a as i64 - internal.total_b as i64,
                    rate_a,
                    rate_b,
                    stream_a_name: stream_a_name.clone(),
                    stream_b_name: stream_b_name.clone(),
                    timestamp: Utc::now(),
                    uptime_a,
                    uptime_b,
                    uptime_a_all_time,
                    uptime_b_all_time,
                    downtime_a: downtime_a as f64,
                    downtime_b: downtime_b as f64,
                    connected_a,
                    connected_b,
                    connect_time_a_ms: connect_time_a,
                    connect_time_b_ms: connect_time_b,
                    live_latency_metric: LiveLatencyMetric::ConnectionLatency,
                    live_latency_a_ms: live_latency_a,
                    live_latency_b_ms: live_latency_b,
                    delivery_latency_a_ms: delivery_latency_a,
                    delivery_latency_b_ms: delivery_latency_b,
                    mttr_a_ms: mttr_a,
                    mttr_b_ms: mttr_b,
                    current_streak_a: streak_a,
                    current_streak_b: streak_b,
                    baseline_1_name: baseline_1_name.clone(),
                    baseline_2_name: baseline_2_name.clone(),
                    baseline_1: baseline_1_total,
                    baseline_2: baseline_2_total,
                    rate_baseline_1,
                    rate_baseline_2,
                    connected_baseline_1,
                    connected_baseline_2,
                    uptime_baseline_1_all_time,
                    uptime_baseline_2_all_time,
                    current_streak_baseline_1: streak_baseline_1,
                    current_streak_baseline_2: streak_baseline_2,
                    delivery_available_a: availability_a.delivery_available,
                    delivery_available_b: availability_b.delivery_available,
                    delivery_available_baseline_1: availability_baseline_1.delivery_available,
                    delivery_available_baseline_2: availability_baseline_2.delivery_available,
                    transport_uptime_a_all_time: availability_a.transport_uptime_percent(),
                    transport_uptime_b_all_time: availability_b.transport_uptime_percent(),
                    transport_uptime_baseline_1_all_time: availability_baseline_1
                        .transport_uptime_percent(),
                    transport_uptime_baseline_2_all_time: availability_baseline_2
                        .transport_uptime_percent(),
                    delivery_uptime_a_all_time: availability_a.delivery_uptime_percent(),
                    delivery_uptime_b_all_time: availability_b.delivery_uptime_percent(),
                    delivery_uptime_baseline_1_all_time: availability_baseline_1
                        .delivery_uptime_percent(),
                    delivery_uptime_baseline_2_all_time: availability_baseline_2
                        .delivery_uptime_percent(),
                    reconnect_reason_a: availability_a.last_reason,
                    reconnect_reason_b: availability_b.last_reason,
                    reconnect_reason_baseline_1: availability_baseline_1.last_reason,
                    reconnect_reason_baseline_2: availability_baseline_2.last_reason,
                    data_idle_reconnects_a: availability_a.data_idle_reconnects(),
                    data_idle_reconnects_b: availability_b.data_idle_reconnects(),
                    data_idle_reconnects_baseline_1: availability_baseline_1.data_idle_reconnects(),
                    data_idle_reconnects_baseline_2: availability_baseline_2.data_idle_reconnects(),
                    client_recovery_a_ms: availability_a.client_recovery_ms,
                    client_recovery_b_ms: availability_b.client_recovery_ms,
                    client_recovery_baseline_1_ms: availability_baseline_1.client_recovery_ms,
                    client_recovery_baseline_2_ms: availability_baseline_2.client_recovery_ms,
                    event_time_a,
                    event_time_b,
                    event_time_baseline_1,
                    event_time_baseline_2,
                    comparison,
                    comparisons,
                    watermark_skew_threshold_us: duration_us(
                        uptime.read().unwrap().watermark_skew_threshold,
                    ),
                };

                let _ = tx.send(stats_snapshot);
            }
        });
    }
}

#[derive(Debug, Default)]
pub struct StreamStatsInternal {
    pub total_a: u64,
    pub total_b: u64,
}

impl StreamStatsInternal {
    pub fn update(&mut self, msg: StreamMessage) {
        match msg.stream_id {
            StreamId::A => self.total_a = msg.count,
            StreamId::B => self.total_b = msg.count,
            StreamId::Baseline1 | StreamId::Baseline2 => {}
        }
    }

    pub fn load_totals(&mut self, total_a: u64, total_b: u64) {
        self.total_a = total_a;
        self.total_b = total_b;
    }
}

#[derive(Debug, Default)]
struct AvailabilityState {
    transport_connected: bool,
    delivery_available: bool,
    transport_up_started: Option<Instant>,
    transport_down_started: Option<Instant>,
    transport_up_seconds: u64,
    transport_down_seconds: u64,
    delivery_up_started: Option<Instant>,
    delivery_down_started: Option<Instant>,
    delivery_up_seconds: u64,
    delivery_down_seconds: u64,
    client_recovery_started: Option<Instant>,
    client_recovery_ms: u64,
    last_reason: Option<ReconnectReason>,
    reason_counts: HashMap<String, u64>,
}

#[derive(Debug, Clone)]
pub struct AvailabilitySnapshot {
    pub transport_connected: bool,
    pub delivery_available: bool,
    pub transport_up_seconds: u64,
    pub transport_down_seconds: u64,
    pub delivery_up_seconds: u64,
    pub delivery_down_seconds: u64,
    pub client_recovery_ms: u64,
    pub last_reason: Option<ReconnectReason>,
    pub reason_counts: HashMap<String, u64>,
}

impl AvailabilitySnapshot {
    pub fn transport_uptime_percent(&self) -> f64 {
        percent(self.transport_up_seconds, self.transport_down_seconds)
    }
    pub fn delivery_uptime_percent(&self) -> f64 {
        percent(self.delivery_up_seconds, self.delivery_down_seconds)
    }
    pub fn data_idle_reconnects(&self) -> u64 {
        self.reason_counts
            .get("dataidletimeout")
            .copied()
            .unwrap_or(0)
    }
}

fn percent(up: u64, down: u64) -> f64 {
    let observed = up.saturating_add(down);
    if observed == 0 {
        0.0
    } else {
        (up as f64 / observed as f64) * 100.0
    }
}

fn duration_us(duration: Duration) -> u64 {
    duration.as_micros().min(u64::MAX as u128) as u64
}

pub fn comparison_eligibility(
    left: &StreamEventTimeSnapshot,
    right: &StreamEventTimeSnapshot,
    watermark_skew_threshold: Duration,
) -> ComparisonEligibility {
    if left.delivery_mode == DeliveryMode::CatchingUp
        || right.delivery_mode == DeliveryMode::CatchingUp
    {
        return ComparisonEligibility {
            eligible: false,
            reason: Some(ComparisonIneligibilityReason::CatchingUp),
            watermark_skew_us: None,
        };
    }
    if left.delivery_mode == DeliveryMode::Unknown || right.delivery_mode == DeliveryMode::Unknown {
        return ComparisonEligibility {
            eligible: false,
            reason: Some(ComparisonIneligibilityReason::UnknownMode),
            watermark_skew_us: None,
        };
    }
    if !left.event_time_coverage || !right.event_time_coverage {
        return ComparisonEligibility {
            eligible: false,
            reason: Some(ComparisonIneligibilityReason::MissingEventTimeCoverage),
            watermark_skew_us: None,
        };
    }
    let watermark_skew_us = left
        .source_watermark_us
        .zip(right.source_watermark_us)
        .map(|(left, right)| left.abs_diff(right));
    if watermark_skew_us.is_none_or(|skew| skew > duration_us(watermark_skew_threshold)) {
        return ComparisonEligibility {
            eligible: false,
            reason: Some(ComparisonIneligibilityReason::WatermarkSkew),
            watermark_skew_us,
        };
    }
    ComparisonEligibility {
        eligible: true,
        reason: None,
        watermark_skew_us,
    }
}

impl AvailabilityState {
    fn apply(&mut self, status: &ConnectionStatus, now: Instant) {
        if let Some(reason) = status.reconnect_reason {
            self.last_reason = Some(reason);
            *self
                .reason_counts
                .entry(format!("{reason:?}").to_lowercase())
                .or_insert(0) += 1;
        }

        if status.connected {
            if !self.transport_connected {
                if let Some(start) = self.client_recovery_started.take() {
                    self.client_recovery_ms = self
                        .client_recovery_ms
                        .saturating_add(now.duration_since(start).as_millis() as u64);
                } else if let Some(start) = self.transport_down_started.take() {
                    self.transport_down_seconds = self
                        .transport_down_seconds
                        .saturating_add(now.duration_since(start).as_secs());
                }
                self.transport_up_started = Some(now);
            }
            self.transport_connected = true;
        } else {
            if self.transport_connected {
                if let Some(start) = self.transport_up_started.take() {
                    self.transport_up_seconds = self
                        .transport_up_seconds
                        .saturating_add(now.duration_since(start).as_secs());
                }
            }
            self.transport_connected = false;
            if status.client_recovery {
                self.client_recovery_started.get_or_insert(now);
                self.transport_down_started = None;
            } else {
                self.transport_down_started.get_or_insert(now);
            }
        }

        if status.delivery_available {
            self.mark_delivery(now);
        } else if self.delivery_available {
            if let Some(start) = self.delivery_up_started.take() {
                self.delivery_up_seconds = self
                    .delivery_up_seconds
                    .saturating_add(now.duration_since(start).as_secs());
            }
            self.delivery_available = false;
            self.delivery_down_started.get_or_insert(now);
        } else {
            self.delivery_down_started.get_or_insert(now);
        }
    }

    fn mark_delivery(&mut self, now: Instant) {
        if !self.delivery_available {
            if let Some(start) = self.delivery_down_started.take() {
                self.delivery_down_seconds = self
                    .delivery_down_seconds
                    .saturating_add(now.duration_since(start).as_secs());
            }
            self.delivery_up_started = Some(now);
        }
        self.delivery_available = true;
    }

    fn snapshot(&self, now: Instant) -> AvailabilitySnapshot {
        let transport_up = self.transport_up_seconds.saturating_add(
            self.transport_up_started
                .map(|start| now.duration_since(start).as_secs())
                .unwrap_or(0),
        );
        let transport_down = self.transport_down_seconds.saturating_add(
            self.transport_down_started
                .map(|start| now.duration_since(start).as_secs())
                .unwrap_or(0),
        );
        let delivery_up = self.delivery_up_seconds.saturating_add(
            self.delivery_up_started
                .map(|start| now.duration_since(start).as_secs())
                .unwrap_or(0),
        );
        let delivery_down = self.delivery_down_seconds.saturating_add(
            self.delivery_down_started
                .map(|start| now.duration_since(start).as_secs())
                .unwrap_or(0),
        );
        let client_recovery_ms = self.client_recovery_ms.saturating_add(
            self.client_recovery_started
                .map(|start| now.duration_since(start).as_millis() as u64)
                .unwrap_or(0),
        );
        AvailabilitySnapshot {
            transport_connected: self.transport_connected,
            delivery_available: self.delivery_available,
            transport_up_seconds: transport_up,
            transport_down_seconds: transport_down,
            delivery_up_seconds: delivery_up,
            delivery_down_seconds: delivery_down,
            client_recovery_ms,
            last_reason: self.last_reason,
            reason_counts: self.reason_counts.clone(),
        }
    }
}

#[derive(Debug, Default)]
pub struct BaselineStream {
    pub connected: bool,
    pub connected_at: Option<Instant>,
    pub session_start: Option<Instant>,
    pub disconnected_at: Option<Instant>,
    pub session_start_disconnected: Option<Instant>,
    pub connected_seconds: u64,
    pub disconnected_seconds: u64,
    pub total_messages: u64,
    pub message_samples: VecDeque<(Instant, u64)>,
    availability: AvailabilityState,
}

#[derive(Debug)]
pub struct UptimeTracker {
    availability_a: AvailabilityState,
    availability_b: AvailabilityState,
    pub connected_a: bool,
    pub connected_b: bool,
    pub connected_at_a: Option<Instant>,
    pub connected_at_b: Option<Instant>,
    pub disconnect_count_a: u64,
    pub disconnect_count_b: u64,
    pub connect_time_sum_a_ms: u64,
    pub connect_time_sum_b_ms: u64,
    pub connect_time_count_a: u64,
    pub connect_time_count_b: u64,
    pub last_connect_time_a_ms: Option<u64>,
    pub last_connect_time_b_ms: Option<u64>,
    pub total_messages_a: u64,
    pub total_messages_b: u64,
    message_samples_a: VecDeque<(Instant, u64)>,
    message_samples_b: VecDeque<(Instant, u64)>,
    session_start_a: Option<Instant>,
    session_start_b: Option<Instant>,
    connected_seconds_a: u64,
    connected_seconds_b: u64,
    disconnected_seconds_a: u64,
    disconnected_seconds_b: u64,
    disconnected_at_a: Option<Instant>,
    disconnected_at_b: Option<Instant>,
    session_start_disconnected_a: Option<Instant>,
    session_start_disconnected_b: Option<Instant>,
    server_start_time: Instant,
    delivery_latency_samples_a: VecDeque<(Instant, u64)>,
    delivery_latency_samples_b: VecDeque<(Instant, u64)>,
    total_recovery_time_a_ms: u64,
    total_recovery_time_b_ms: u64,
    recovery_count_a: u64,
    recovery_count_b: u64,
    pub baseline_1: BaselineStream,
    pub baseline_2: BaselineStream,
    event_time_a: EventTimeState,
    event_time_b: EventTimeState,
    event_time_baseline_1: EventTimeState,
    event_time_baseline_2: EventTimeState,
    live_lag_threshold: Duration,
    watermark_skew_threshold: Duration,
    event_idle_threshold: Duration,
    comparison_engine: ComparisonEngine,
}

#[derive(Debug, Default)]
struct EventTimeState {
    source_watermark_us: Option<u64>,
    last_timestamped_at: Option<Instant>,
    coverage_samples: VecDeque<(Instant, bool)>,
    clock_skew_us: u64,
}

#[derive(Debug, Clone, Copy)]
pub struct UptimeMetricsSnapshot {
    pub uptime_a_seconds: u64,
    pub uptime_b_seconds: u64,
    pub downtime_a_seconds: u64,
    pub downtime_b_seconds: u64,
    pub disconnect_count_a: u64,
    pub disconnect_count_b: u64,
    pub connect_time_sum_a_ms: u64,
    pub connect_time_sum_b_ms: u64,
    pub connect_time_count_a: u64,
    pub connect_time_count_b: u64,
    pub total_messages_a: u64,
    pub total_messages_b: u64,
    pub total_recovery_time_a_ms: u64,
    pub total_recovery_time_b_ms: u64,
    pub recovery_count_a: u64,
    pub recovery_count_b: u64,
    pub delivery_latency_a_ms: f64,
    pub delivery_latency_b_ms: f64,
    pub baseline_1_uptime_seconds: u64,
    pub baseline_2_uptime_seconds: u64,
    pub baseline_1_downtime_seconds: u64,
    pub baseline_2_downtime_seconds: u64,
    pub baseline_1_total_messages: u64,
    pub baseline_2_total_messages: u64,
}

impl Default for UptimeTracker {
    fn default() -> Self {
        Self {
            availability_a: AvailabilityState::default(),
            availability_b: AvailabilityState::default(),
            connected_a: false,
            connected_b: false,
            connected_at_a: None,
            connected_at_b: None,
            disconnect_count_a: 0,
            disconnect_count_b: 0,
            connect_time_sum_a_ms: 0,
            connect_time_sum_b_ms: 0,
            connect_time_count_a: 0,
            connect_time_count_b: 0,
            last_connect_time_a_ms: None,
            last_connect_time_b_ms: None,
            total_messages_a: 0,
            total_messages_b: 0,
            message_samples_a: VecDeque::new(),
            message_samples_b: VecDeque::new(),
            session_start_a: None,
            session_start_b: None,
            connected_seconds_a: 0,
            connected_seconds_b: 0,
            disconnected_seconds_a: 0,
            disconnected_seconds_b: 0,
            disconnected_at_a: None,
            disconnected_at_b: None,
            session_start_disconnected_a: None,
            session_start_disconnected_b: None,
            server_start_time: Instant::now(),
            delivery_latency_samples_a: VecDeque::new(),
            delivery_latency_samples_b: VecDeque::new(),
            total_recovery_time_a_ms: 0,
            total_recovery_time_b_ms: 0,
            recovery_count_a: 0,
            recovery_count_b: 0,
            baseline_1: BaselineStream::default(),
            baseline_2: BaselineStream::default(),
            event_time_a: EventTimeState::default(),
            event_time_b: EventTimeState::default(),
            event_time_baseline_1: EventTimeState::default(),
            event_time_baseline_2: EventTimeState::default(),
            live_lag_threshold: Duration::from_secs(30),
            watermark_skew_threshold: Duration::from_secs(30),
            event_idle_threshold: Duration::from_secs(30),
            comparison_engine: ComparisonEngine::new(ObservationConfig::default()),
        }
    }
}

impl UptimeTracker {
    const RATE_WINDOW: Duration = Duration::from_secs(10);

    pub fn new() -> Self {
        Self::default()
    }

    pub fn with_event_time_thresholds(
        live_lag_threshold: Duration,
        watermark_skew_threshold: Duration,
        event_idle_threshold: Duration,
    ) -> Self {
        Self {
            live_lag_threshold,
            watermark_skew_threshold,
            event_idle_threshold,
            ..Self::default()
        }
    }

    pub fn with_comparison_config(mut self, config: ObservationConfig) -> Self {
        self.comparison_engine = ComparisonEngine::new(config);
        self
    }

    pub fn handle_connection_status(&mut self, status: ConnectionStatus) {
        let now = Instant::now();

        match status.stream_id {
            StreamId::A => self.availability_a.apply(&status, now),
            StreamId::B => self.availability_b.apply(&status, now),
            StreamId::Baseline1 => self.baseline_1.availability.apply(&status, now),
            StreamId::Baseline2 => self.baseline_2.availability.apply(&status, now),
        }

        match status.stream_id {
            StreamId::A => {
                if status.connected {
                    if !self.connected_a {
                        if let Some(session_start) = self.session_start_disconnected_a.take() {
                            let elapsed = now.duration_since(session_start).as_secs();
                            self.disconnected_seconds_a =
                                self.disconnected_seconds_a.saturating_add(elapsed);
                        }
                        if let Some(disc_at) = self.disconnected_at_a {
                            let recovery_ms = now.duration_since(disc_at).as_millis() as u64;
                            self.total_recovery_time_a_ms += recovery_ms;
                            self.recovery_count_a += 1;
                        }
                    }
                    self.session_start_a = Some(now);
                    self.connected_a = true;
                    self.connected_at_a = Some(now);
                    self.disconnected_at_a = None;
                    if let Some(ct) = status.connect_time_ms {
                        self.connect_time_sum_a_ms += ct;
                        self.connect_time_count_a += 1;
                        self.last_connect_time_a_ms = Some(ct);
                    }
                } else {
                    if let Some(session_start) = self.session_start_a.take() {
                        let elapsed = now.duration_since(session_start).as_secs();
                        self.connected_seconds_a = self.connected_seconds_a.saturating_add(elapsed);
                    }
                    self.connected_a = false;
                    self.disconnected_at_a = Some(now);
                    self.session_start_disconnected_a = Some(now);
                    self.disconnect_count_a += 1;
                }
            }
            StreamId::B => {
                if status.connected {
                    if !self.connected_b {
                        if let Some(session_start) = self.session_start_disconnected_b.take() {
                            let elapsed = now.duration_since(session_start).as_secs();
                            self.disconnected_seconds_b =
                                self.disconnected_seconds_b.saturating_add(elapsed);
                        }
                        if let Some(disc_at) = self.disconnected_at_b {
                            let recovery_ms = now.duration_since(disc_at).as_millis() as u64;
                            self.total_recovery_time_b_ms += recovery_ms;
                            self.recovery_count_b += 1;
                        }
                    }
                    self.session_start_b = Some(now);
                    self.connected_b = true;
                    self.connected_at_b = Some(now);
                    self.disconnected_at_b = None;
                    if let Some(ct) = status.connect_time_ms {
                        self.connect_time_sum_b_ms += ct;
                        self.connect_time_count_b += 1;
                        self.last_connect_time_b_ms = Some(ct);
                    }
                } else {
                    if let Some(session_start) = self.session_start_b.take() {
                        let elapsed = now.duration_since(session_start).as_secs();
                        self.connected_seconds_b = self.connected_seconds_b.saturating_add(elapsed);
                    }
                    self.connected_b = false;
                    self.disconnected_at_b = Some(now);
                    self.session_start_disconnected_b = Some(now);
                    self.disconnect_count_b += 1;
                }
            }
            StreamId::Baseline1 => Self::apply_baseline_status(&mut self.baseline_1, status, now),
            StreamId::Baseline2 => Self::apply_baseline_status(&mut self.baseline_2, status, now),
        }
        self.refresh_comparison_epochs();
    }

    fn apply_baseline_status(
        baseline: &mut BaselineStream,
        status: ConnectionStatus,
        now: Instant,
    ) {
        if status.connected {
            if !baseline.connected {
                if let Some(session_start) = baseline.session_start_disconnected.take() {
                    let elapsed = now.duration_since(session_start).as_secs();
                    baseline.disconnected_seconds =
                        baseline.disconnected_seconds.saturating_add(elapsed);
                }
            }
            baseline.session_start = Some(now);
            baseline.connected = true;
            baseline.connected_at = Some(now);
            baseline.disconnected_at = None;
        } else {
            if let Some(session_start) = baseline.session_start.take() {
                let elapsed = now.duration_since(session_start).as_secs();
                baseline.connected_seconds = baseline.connected_seconds.saturating_add(elapsed);
            }
            baseline.connected = false;
            baseline.disconnected_at = Some(now);
            baseline.session_start_disconnected = Some(now);
        }
    }

    pub fn record_message(&mut self, stream_id: StreamId) {
        match stream_id {
            StreamId::A => self.total_messages_a += 1,
            StreamId::B => self.total_messages_b += 1,
            StreamId::Baseline1 => self.baseline_1.total_messages += 1,
            StreamId::Baseline2 => self.baseline_2.total_messages += 1,
        }
    }

    pub fn record_total_count(&mut self, stream_id: StreamId, total_count: u64) {
        let now = Instant::now();

        match stream_id {
            StreamId::A => {
                self.availability_a.mark_delivery(now);
                self.total_messages_a = total_count;
                Self::record_sample(&mut self.message_samples_a, now, total_count);
            }
            StreamId::B => {
                self.availability_b.mark_delivery(now);
                self.total_messages_b = total_count;
                Self::record_sample(&mut self.message_samples_b, now, total_count);
            }
            StreamId::Baseline1 => {
                self.baseline_1.availability.mark_delivery(now);
                self.baseline_1.total_messages = total_count;
                Self::record_sample(&mut self.baseline_1.message_samples, now, total_count);
            }
            StreamId::Baseline2 => {
                self.baseline_2.availability.mark_delivery(now);
                self.baseline_2.total_messages = total_count;
                Self::record_sample(&mut self.baseline_2.message_samples, now, total_count);
            }
        }
    }

    pub fn record_stream_message(&mut self, message: &StreamMessage) {
        self.record_total_count(message.stream_id, message.count);
        let now = Instant::now();
        let state = match message.stream_id {
            StreamId::A => &mut self.event_time_a,
            StreamId::B => &mut self.event_time_b,
            StreamId::Baseline1 => &mut self.event_time_baseline_1,
            StreamId::Baseline2 => &mut self.event_time_baseline_2,
        };
        state
            .coverage_samples
            .push_back((now, message.source_event.is_some()));
        while state
            .coverage_samples
            .front()
            .is_some_and(|(observed_at, _)| now.duration_since(*observed_at) > Self::RATE_WINDOW)
        {
            state.coverage_samples.pop_front();
        }
        if let Some(observation) = &message.source_event {
            state.source_watermark_us = Some(
                state
                    .source_watermark_us
                    .map_or(observation.source_time_us, |watermark| {
                        watermark.max(observation.source_time_us)
                    }),
            );
            state.last_timestamped_at = Some(now);
            state.clock_skew_us = observation.clock_skew_us;
            self.comparison_engine
                .record(message.stream_id, observation);
        }
        self.refresh_comparison_epochs();
    }

    pub fn pairwise_comparisons(&self) -> PairwiseComparisons {
        self.comparison_engine.snapshots()
    }

    fn refresh_comparison_epochs(&mut self) {
        let states = [
            StreamId::A,
            StreamId::B,
            StreamId::Baseline1,
            StreamId::Baseline2,
        ]
        .into_iter()
        .map(|stream_id| {
            let event_time = self.event_time_snapshot(stream_id);
            let availability = self.availability_snapshot(stream_id);
            (
                stream_id,
                ComparisonStreamState {
                    connected: availability.transport_connected,
                    delivery_available: availability.delivery_available,
                    mode: event_time.delivery_mode,
                    source_watermark_us: event_time.source_watermark_us,
                },
            )
        })
        .collect();
        self.comparison_engine
            .refresh(&states, self.watermark_skew_threshold);
    }

    pub fn event_time_snapshot(&self, stream_id: StreamId) -> StreamEventTimeSnapshot {
        let now = Instant::now();
        let wall_us = Utc::now().timestamp_micros().max(0) as u64;
        self.event_time_snapshot_at(stream_id, now, wall_us)
    }

    pub fn watermark_skew_threshold(&self) -> Duration {
        self.watermark_skew_threshold
    }

    fn event_time_snapshot_at(
        &self,
        stream_id: StreamId,
        now: Instant,
        wall_us: u64,
    ) -> StreamEventTimeSnapshot {
        let (state, availability) = match stream_id {
            StreamId::A => (&self.event_time_a, &self.availability_a),
            StreamId::B => (&self.event_time_b, &self.availability_b),
            StreamId::Baseline1 => (&self.event_time_baseline_1, &self.baseline_1.availability),
            StreamId::Baseline2 => (&self.event_time_baseline_2, &self.baseline_2.availability),
        };
        let event_time_coverage = state.coverage_samples.iter().any(|(observed_at, covered)| {
            *covered && now.duration_since(*observed_at) <= Self::RATE_WINDOW
        });
        let source_lag_us = state
            .source_watermark_us
            .map(|watermark| wall_us.saturating_sub(watermark));
        let recent_timestamped_delivery = state.last_timestamped_at.is_some_and(|observed_at| {
            now.duration_since(observed_at) <= self.event_idle_threshold
        });
        let delivery_mode = if !availability.transport_connected
            || !availability.delivery_available
            || !recent_timestamped_delivery
        {
            DeliveryMode::Unknown
        } else if source_lag_us.is_some_and(|lag| lag <= duration_us(self.live_lag_threshold)) {
            DeliveryMode::Live
        } else {
            DeliveryMode::CatchingUp
        };
        StreamEventTimeSnapshot {
            source_watermark_us: state.source_watermark_us,
            source_lag_us,
            delivery_mode,
            event_time_coverage,
            clock_skew_us: state.clock_skew_us,
        }
    }

    pub fn availability_snapshot(&self, stream_id: StreamId) -> AvailabilitySnapshot {
        let now = Instant::now();
        match stream_id {
            StreamId::A => self.availability_a.snapshot(now),
            StreamId::B => self.availability_b.snapshot(now),
            StreamId::Baseline1 => self.baseline_1.availability.snapshot(now),
            StreamId::Baseline2 => self.baseline_2.availability.snapshot(now),
        }
    }

    pub fn record_delivery_latency(&mut self, stream_id: StreamId, latency_us: u64) {
        let now = Instant::now();
        match stream_id {
            StreamId::A => {
                self.delivery_latency_samples_a.push_back((now, latency_us));
                while let Some((t, _)) = self.delivery_latency_samples_a.front() {
                    if now.duration_since(*t) > Self::RATE_WINDOW {
                        self.delivery_latency_samples_a.pop_front();
                    } else {
                        break;
                    }
                }
            }
            StreamId::B => {
                self.delivery_latency_samples_b.push_back((now, latency_us));
                while let Some((t, _)) = self.delivery_latency_samples_b.front() {
                    if now.duration_since(*t) > Self::RATE_WINDOW {
                        self.delivery_latency_samples_b.pop_front();
                    } else {
                        break;
                    }
                }
            }
            StreamId::Baseline1 | StreamId::Baseline2 => {}
        }
    }

    fn avg_delivery_latency_ms(samples: &VecDeque<(Instant, u64)>, now: Instant) -> f64 {
        let valid: Vec<_> = samples
            .iter()
            .filter(|(t, _)| now.duration_since(*t) <= Self::RATE_WINDOW)
            .collect();
        if valid.is_empty() {
            return 0.0;
        }
        let sum: u64 = valid.iter().map(|(_, v)| v).sum();
        (sum as f64 / valid.len() as f64) / 1000.0 // us -> ms
    }

    pub fn get_delivery_latency_a_ms(&self) -> f64 {
        Self::avg_delivery_latency_ms(&self.delivery_latency_samples_a, Instant::now())
    }

    pub fn get_delivery_latency_b_ms(&self) -> f64 {
        Self::avg_delivery_latency_ms(&self.delivery_latency_samples_b, Instant::now())
    }

    pub fn get_mttr_a_ms(&self) -> u64 {
        if self.recovery_count_a > 0 {
            self.total_recovery_time_a_ms / self.recovery_count_a
        } else {
            0
        }
    }

    pub fn get_mttr_b_ms(&self) -> u64 {
        if self.recovery_count_b > 0 {
            self.total_recovery_time_b_ms / self.recovery_count_b
        } else {
            0
        }
    }

    fn record_sample(samples: &mut VecDeque<(Instant, u64)>, now: Instant, total_count: u64) {
        samples.push_back((now, total_count));

        while let Some((sample_time, _)) = samples.front() {
            if now.duration_since(*sample_time) > Self::RATE_WINDOW {
                samples.pop_front();
            } else {
                break;
            }
        }
    }

    fn rolling_rate(samples: &VecDeque<(Instant, u64)>, now: Instant) -> f64 {
        let (latest_time, latest_total) = match samples.back() {
            Some(sample) => sample,
            None => return 0.0,
        };

        if now.duration_since(*latest_time) > Self::RATE_WINDOW {
            return 0.0;
        }

        let (oldest_time, oldest_total) = match samples.front() {
            Some(sample) => sample,
            None => return 0.0,
        };

        let elapsed = latest_time.duration_since(*oldest_time).as_secs_f64();
        if elapsed <= 0.0 {
            return 0.0;
        }

        latest_total.saturating_sub(*oldest_total) as f64 / elapsed
    }

    pub fn get_all_time_uptime_seconds(&self) -> (u64, u64) {
        let now = Instant::now();

        let uptime_a = if self.connected_a {
            let current_session = self
                .connected_at_a
                .map(|start| now.duration_since(start).as_secs())
                .unwrap_or(0);
            self.connected_seconds_a.saturating_add(current_session)
        } else {
            self.connected_seconds_a
        };

        let uptime_b = if self.connected_b {
            let current_session = self
                .connected_at_b
                .map(|start| now.duration_since(start).as_secs())
                .unwrap_or(0);
            self.connected_seconds_b.saturating_add(current_session)
        } else {
            self.connected_seconds_b
        };

        (uptime_a, uptime_b)
    }

    pub fn get_current_uptime_percentage(&self) -> (f64, f64) {
        let now = Instant::now();

        let uptime_a = if self.connected_a {
            100.0
        } else {
            let connected_time = self.connected_seconds_a;
            let disconnected_time = self
                .disconnected_at_a
                .map(|d| now.duration_since(d).as_secs())
                .unwrap_or(0);

            if connected_time == 0 && disconnected_time == 0 {
                0.0
            } else {
                let total = connected_time + disconnected_time;
                (connected_time as f64 / total as f64) * 100.0
            }
        };

        let uptime_b = if self.connected_b {
            100.0
        } else {
            let connected_time = self.connected_seconds_b;
            let disconnected_time = self
                .disconnected_at_b
                .map(|d| now.duration_since(d).as_secs())
                .unwrap_or(0);

            if connected_time == 0 && disconnected_time == 0 {
                0.0
            } else {
                let total = connected_time + disconnected_time;
                (connected_time as f64 / total as f64) * 100.0
            }
        };

        (uptime_a, uptime_b)
    }

    pub fn get_all_time_uptime_percentage(&self) -> (f64, f64) {
        let (uptime_a, uptime_b) = self.get_all_time_uptime_seconds();
        let (downtime_a, downtime_b) = self.get_all_time_downtime_seconds();

        let total_a = uptime_a.saturating_add(downtime_a);
        let total_b = uptime_b.saturating_add(downtime_b);

        let uptime_pct_a = if total_a > 0 {
            (uptime_a as f64 / total_a as f64) * 100.0
        } else {
            0.0
        };
        let uptime_pct_b = if total_b > 0 {
            (uptime_b as f64 / total_b as f64) * 100.0
        } else {
            0.0
        };

        (uptime_pct_a.min(100.0), uptime_pct_b.min(100.0))
    }

    pub fn get_all_time_downtime_seconds(&self) -> (u64, u64) {
        let now = Instant::now();

        let downtime_a = if self.connected_a {
            self.disconnected_seconds_a
        } else {
            let current_disconnect = self
                .disconnected_at_a
                .map(|d| now.duration_since(d).as_secs())
                .unwrap_or(0);
            self.disconnected_seconds_a
                .saturating_add(current_disconnect)
        };

        let downtime_b = if self.connected_b {
            self.disconnected_seconds_b
        } else {
            let current_disconnect = self
                .disconnected_at_b
                .map(|d| now.duration_since(d).as_secs())
                .unwrap_or(0);
            self.disconnected_seconds_b
                .saturating_add(current_disconnect)
        };

        (downtime_a, downtime_b)
    }

    pub fn get_all_time_uptime_from_downtime(&self) -> (f64, f64) {
        let now = Instant::now();
        let server_run_time = now.duration_since(self.server_start_time).as_secs();

        if server_run_time == 0 {
            return (0.0, 0.0);
        }

        let (downtime_a, downtime_b) = self.get_all_time_downtime_seconds();

        let uptime_a = 100.0 - ((downtime_a as f64 / server_run_time as f64) * 100.0);
        let uptime_b = 100.0 - ((downtime_b as f64 / server_run_time as f64) * 100.0);

        (uptime_a.max(0.0).min(100.0), uptime_b.max(0.0).min(100.0))
    }

    pub fn get_metrics_snapshot(&self) -> UptimeMetricsSnapshot {
        let (uptime_a_seconds, uptime_b_seconds) = self.get_all_time_uptime_seconds();
        let (downtime_a_seconds, downtime_b_seconds) = self.get_all_time_downtime_seconds();
        let now = Instant::now();

        UptimeMetricsSnapshot {
            uptime_a_seconds,
            uptime_b_seconds,
            downtime_a_seconds,
            downtime_b_seconds,
            disconnect_count_a: self.disconnect_count_a,
            disconnect_count_b: self.disconnect_count_b,
            connect_time_sum_a_ms: self.connect_time_sum_a_ms,
            connect_time_sum_b_ms: self.connect_time_sum_b_ms,
            connect_time_count_a: self.connect_time_count_a,
            connect_time_count_b: self.connect_time_count_b,
            total_messages_a: self.total_messages_a,
            total_messages_b: self.total_messages_b,
            total_recovery_time_a_ms: self.total_recovery_time_a_ms,
            total_recovery_time_b_ms: self.total_recovery_time_b_ms,
            recovery_count_a: self.recovery_count_a,
            recovery_count_b: self.recovery_count_b,
            delivery_latency_a_ms: self.get_delivery_latency_a_ms(),
            delivery_latency_b_ms: self.get_delivery_latency_b_ms(),
            baseline_1_uptime_seconds: Self::baseline_uptime_seconds(&self.baseline_1, now),
            baseline_2_uptime_seconds: Self::baseline_uptime_seconds(&self.baseline_2, now),
            baseline_1_downtime_seconds: Self::baseline_downtime_seconds(&self.baseline_1, now),
            baseline_2_downtime_seconds: Self::baseline_downtime_seconds(&self.baseline_2, now),
            baseline_1_total_messages: self.baseline_1.total_messages,
            baseline_2_total_messages: self.baseline_2.total_messages,
        }
    }

    pub fn load_totals(&mut self, total_a: u64, total_b: u64) {
        self.total_messages_a = total_a;
        self.total_messages_b = total_b;
    }

    pub fn get_avg_connect_time_a(&self) -> u64 {
        if self.connect_time_count_a > 0 {
            self.connect_time_sum_a_ms / self.connect_time_count_a
        } else {
            0
        }
    }

    pub fn get_avg_connect_time_b(&self) -> u64 {
        if self.connect_time_count_b > 0 {
            self.connect_time_sum_b_ms / self.connect_time_count_b
        } else {
            0
        }
    }

    pub fn get_connection_latency_a_ms(&self) -> f64 {
        self.last_connect_time_a_ms.unwrap_or(0) as f64
    }

    pub fn get_connection_latency_b_ms(&self) -> f64 {
        self.last_connect_time_b_ms.unwrap_or(0) as f64
    }

    pub fn get_average_rates(&self) -> (f64, f64) {
        let now = Instant::now();
        let rate_a = Self::rolling_rate(&self.message_samples_a, now);
        let rate_b = Self::rolling_rate(&self.message_samples_b, now);

        (rate_a, rate_b)
    }

    pub fn get_current_streak_a(&self) -> f64 {
        if let Some(connected_at) = self.connected_at_a {
            connected_at.elapsed().as_secs() as f64
        } else {
            0.0
        }
    }

    pub fn get_current_streak_b(&self) -> f64 {
        if let Some(connected_at) = self.connected_at_b {
            connected_at.elapsed().as_secs() as f64
        } else {
            0.0
        }
    }

    pub fn get_baseline_rates(&self) -> (f64, f64) {
        let now = Instant::now();
        (
            Self::rolling_rate(&self.baseline_1.message_samples, now),
            Self::rolling_rate(&self.baseline_2.message_samples, now),
        )
    }

    fn baseline_uptime_seconds(baseline: &BaselineStream, now: Instant) -> u64 {
        if baseline.connected {
            let current = baseline
                .connected_at
                .map(|start| now.duration_since(start).as_secs())
                .unwrap_or(0);
            baseline.connected_seconds.saturating_add(current)
        } else {
            baseline.connected_seconds
        }
    }

    fn baseline_downtime_seconds(baseline: &BaselineStream, now: Instant) -> u64 {
        if baseline.connected {
            baseline.disconnected_seconds
        } else {
            let current = baseline
                .disconnected_at
                .map(|d| now.duration_since(d).as_secs())
                .unwrap_or(0);
            baseline.disconnected_seconds.saturating_add(current)
        }
    }

    pub fn get_baseline_uptime_percentages(&self) -> (f64, f64) {
        let now = Instant::now();
        let server_run_time = now.duration_since(self.server_start_time).as_secs();
        if server_run_time == 0 {
            return (0.0, 0.0);
        }
        let down_1 = Self::baseline_downtime_seconds(&self.baseline_1, now);
        let down_2 = Self::baseline_downtime_seconds(&self.baseline_2, now);
        let up_1 = 100.0 - ((down_1 as f64 / server_run_time as f64) * 100.0);
        let up_2 = 100.0 - ((down_2 as f64 / server_run_time as f64) * 100.0);
        (up_1.max(0.0).min(100.0), up_2.max(0.0).min(100.0))
    }

    pub fn get_baseline_1_streak(&self) -> f64 {
        self.baseline_1
            .connected_at
            .map(|t| t.elapsed().as_secs() as f64)
            .unwrap_or(0.0)
    }

    pub fn get_baseline_2_streak(&self) -> f64 {
        self.baseline_2
            .connected_at
            .map(|t| t.elapsed().as_secs() as f64)
            .unwrap_or(0.0)
    }

    pub fn get_detailed_stats(&self, period_seconds: u64) -> UptimeDetailedStats {
        fn estimate_window_uptime(
            lifetime_uptime_seconds: u64,
            lifetime_downtime_seconds: u64,
            requested_window_seconds: u64,
        ) -> (u64, u64, u64, f64) {
            if requested_window_seconds == 0 {
                return (0, 0, 0, 0.0);
            }

            let observed_lifetime_seconds =
                lifetime_uptime_seconds.saturating_add(lifetime_downtime_seconds);
            if observed_lifetime_seconds == 0 {
                return (0, 0, 0, 0.0);
            }

            let observed_window_seconds = observed_lifetime_seconds.min(requested_window_seconds);
            let uptime_ratio = lifetime_uptime_seconds as f64 / observed_lifetime_seconds as f64;
            let mut window_uptime_seconds =
                (uptime_ratio * observed_window_seconds as f64).round() as u64;
            window_uptime_seconds = window_uptime_seconds.min(observed_window_seconds);
            let window_downtime_seconds =
                observed_window_seconds.saturating_sub(window_uptime_seconds);
            let uptime_percent = if observed_window_seconds > 0 {
                (window_uptime_seconds as f64 / observed_window_seconds as f64) * 100.0
            } else {
                0.0
            };

            (
                observed_window_seconds,
                window_uptime_seconds,
                window_downtime_seconds,
                uptime_percent,
            )
        }

        let snapshot = self.get_metrics_snapshot();
        let lifetime_observed_a_seconds = snapshot
            .uptime_a_seconds
            .saturating_add(snapshot.downtime_a_seconds);
        let lifetime_observed_b_seconds = snapshot
            .uptime_b_seconds
            .saturating_add(snapshot.downtime_b_seconds);

        let (
            window_observed_a_seconds,
            window_uptime_a_seconds,
            window_downtime_a_seconds,
            window_uptime_a_percent,
        ) = estimate_window_uptime(
            snapshot.uptime_a_seconds,
            snapshot.downtime_a_seconds,
            period_seconds,
        );
        let (
            window_observed_b_seconds,
            window_uptime_b_seconds,
            window_downtime_b_seconds,
            window_uptime_b_percent,
        ) = estimate_window_uptime(
            snapshot.uptime_b_seconds,
            snapshot.downtime_b_seconds,
            period_seconds,
        );

        let lifetime_uptime_a_percent = if lifetime_observed_a_seconds > 0 {
            (snapshot.uptime_a_seconds as f64 / lifetime_observed_a_seconds as f64) * 100.0
        } else {
            0.0
        };
        let lifetime_uptime_b_percent = if lifetime_observed_b_seconds > 0 {
            (snapshot.uptime_b_seconds as f64 / lifetime_observed_b_seconds as f64) * 100.0
        } else {
            0.0
        };

        let rate_a = if snapshot.uptime_a_seconds > 0 {
            snapshot.total_messages_a as f64 / snapshot.uptime_a_seconds as f64
        } else {
            0.0
        };
        let rate_b = if snapshot.uptime_b_seconds > 0 {
            snapshot.total_messages_b as f64 / snapshot.uptime_b_seconds as f64
        } else {
            0.0
        };

        UptimeDetailedStats {
            // Legacy fields retained for compatibility: these now represent the requested window.
            uptime_a_seconds: window_uptime_a_seconds,
            uptime_b_seconds: window_uptime_b_seconds,
            downtime_a_seconds: window_downtime_a_seconds,
            downtime_b_seconds: window_downtime_b_seconds,
            uptime_a_percent: window_uptime_a_percent,
            uptime_b_percent: window_uptime_b_percent,

            window_requested_seconds: period_seconds,
            window_observed_a_seconds,
            window_observed_b_seconds,
            window_uptime_a_seconds,
            window_uptime_b_seconds,
            window_downtime_a_seconds,
            window_downtime_b_seconds,
            window_uptime_a_percent,
            window_uptime_b_percent,

            lifetime_observed_a_seconds,
            lifetime_observed_b_seconds,
            lifetime_uptime_a_seconds: snapshot.uptime_a_seconds,
            lifetime_uptime_b_seconds: snapshot.uptime_b_seconds,
            lifetime_downtime_a_seconds: snapshot.downtime_a_seconds,
            lifetime_downtime_b_seconds: snapshot.downtime_b_seconds,
            lifetime_uptime_a_percent,
            lifetime_uptime_b_percent,

            disconnect_count_a: snapshot.disconnect_count_a,
            disconnect_count_b: snapshot.disconnect_count_b,
            avg_connect_time_a_ms: self.get_avg_connect_time_a(),
            avg_connect_time_b_ms: self.get_avg_connect_time_b(),
            delivery_latency_a_ms: snapshot.delivery_latency_a_ms,
            delivery_latency_b_ms: snapshot.delivery_latency_b_ms,
            mttr_a_ms: self.get_mttr_a_ms(),
            mttr_b_ms: self.get_mttr_b_ms(),
            total_messages_a: snapshot.total_messages_a,
            total_messages_b: snapshot.total_messages_b,
            avg_rate_a: rate_a,
            avg_rate_b: rate_b,
            connected_a: self.connected_a,
            connected_b: self.connected_b,
            current_streak_a: self.get_current_streak_a(),
            current_streak_b: self.get_current_streak_b(),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct UptimeDetailedStats {
    pub uptime_a_seconds: u64,
    pub uptime_b_seconds: u64,
    pub downtime_a_seconds: u64,
    pub downtime_b_seconds: u64,
    pub uptime_a_percent: f64,
    pub uptime_b_percent: f64,
    pub window_requested_seconds: u64,
    pub window_observed_a_seconds: u64,
    pub window_observed_b_seconds: u64,
    pub window_uptime_a_seconds: u64,
    pub window_uptime_b_seconds: u64,
    pub window_downtime_a_seconds: u64,
    pub window_downtime_b_seconds: u64,
    pub window_uptime_a_percent: f64,
    pub window_uptime_b_percent: f64,
    pub lifetime_observed_a_seconds: u64,
    pub lifetime_observed_b_seconds: u64,
    pub lifetime_uptime_a_seconds: u64,
    pub lifetime_uptime_b_seconds: u64,
    pub lifetime_downtime_a_seconds: u64,
    pub lifetime_downtime_b_seconds: u64,
    pub lifetime_uptime_a_percent: f64,
    pub lifetime_uptime_b_percent: f64,
    pub disconnect_count_a: u64,
    pub disconnect_count_b: u64,
    pub avg_connect_time_a_ms: u64,
    pub avg_connect_time_b_ms: u64,
    pub delivery_latency_a_ms: f64,
    pub delivery_latency_b_ms: f64,
    pub mttr_a_ms: u64,
    pub mttr_b_ms: u64,
    pub total_messages_a: u64,
    pub total_messages_b: u64,
    pub avg_rate_a: f64,
    pub avg_rate_b: f64,
    pub connected_a: bool,
    pub connected_b: bool,
    pub current_streak_a: f64,
    pub current_streak_b: f64,
}

#[cfg(test)]
mod tests {
    use super::{
        comparison_eligibility, ComparisonIneligibilityReason, DeliveryMode,
        StreamEventTimeSnapshot, UptimeTracker,
    };
    use crate::stream::{
        ConnectionStatus, ReconnectReason, SourceEventObservation, StreamId, StreamMessage,
    };
    use std::sync::{Arc, RwLock};
    use std::time::Duration;

    fn status(
        stream_id: StreamId,
        connected: bool,
        connect_time_ms: Option<u64>,
    ) -> ConnectionStatus {
        ConnectionStatus {
            stream_id,
            connected,
            connected_at: None,
            connect_time_ms,
            delivery_available: connected,
            reconnect_reason: None,
            client_recovery: false,
        }
    }

    fn observed_message(stream_id: StreamId, source_time_us: u64, now_us: u64) -> StreamMessage {
        StreamMessage {
            stream_id,
            count: 1,
            delivery_latency_us: Some(now_us.saturating_sub(source_time_us)),
            source_event: Some(SourceEventObservation {
                source_time_us,
                observed_at_us: now_us,
                lag_us: now_us.saturating_sub(source_time_us),
                clock_skew_us: source_time_us.saturating_sub(now_us),
                source_event_id: Some(format!("event-{source_time_us}")),
            }),
        }
    }

    fn live_snapshot(watermark: u64) -> StreamEventTimeSnapshot {
        StreamEventTimeSnapshot {
            source_watermark_us: Some(watermark),
            source_lag_us: Some(1_000),
            delivery_mode: DeliveryMode::Live,
            event_time_coverage: true,
            clock_skew_us: 0,
        }
    }

    #[test]
    fn event_time_mode_is_live_for_recent_low_lag_delivery() {
        let mut tracker = UptimeTracker::with_event_time_thresholds(
            Duration::from_secs(30),
            Duration::from_secs(5),
            Duration::from_secs(30),
        );
        tracker.handle_connection_status(status(StreamId::A, true, Some(1)));
        let now_us = chrono::Utc::now().timestamp_micros() as u64;
        tracker.record_stream_message(&observed_message(StreamId::A, now_us - 1_000, now_us));

        assert_eq!(
            tracker.event_time_snapshot(StreamId::A).delivery_mode,
            DeliveryMode::Live
        );
    }

    #[test]
    fn event_time_mode_is_catching_up_for_old_watermark() {
        let mut tracker = UptimeTracker::with_event_time_thresholds(
            Duration::from_secs(30),
            Duration::from_secs(5),
            Duration::from_secs(30),
        );
        tracker.handle_connection_status(status(StreamId::A, true, Some(1)));
        let now_us = chrono::Utc::now().timestamp_micros() as u64;
        tracker.record_stream_message(&observed_message(StreamId::A, now_us - 60_000_000, now_us));

        assert_eq!(
            tracker.event_time_snapshot(StreamId::A).delivery_mode,
            DeliveryMode::CatchingUp
        );
    }

    #[test]
    fn event_time_mode_is_unknown_without_recent_covered_delivery() {
        let tracker = UptimeTracker::new();
        let snapshot = tracker.event_time_snapshot(StreamId::A);
        assert_eq!(snapshot.delivery_mode, DeliveryMode::Unknown);
        assert!(!snapshot.event_time_coverage);
    }

    #[test]
    fn event_time_mode_returns_unknown_after_disconnect() {
        let mut tracker = UptimeTracker::new();
        tracker.handle_connection_status(status(StreamId::A, true, Some(1)));
        let now_us = chrono::Utc::now().timestamp_micros() as u64;
        tracker.record_stream_message(&observed_message(StreamId::A, now_us, now_us));
        tracker.handle_connection_status(status(StreamId::A, false, None));

        assert_eq!(
            tracker.event_time_snapshot(StreamId::A).delivery_mode,
            DeliveryMode::Unknown
        );
    }

    #[test]
    fn event_time_mode_returns_unknown_after_timestamped_delivery_idles() {
        let mut tracker = UptimeTracker::with_event_time_thresholds(
            Duration::from_secs(30),
            Duration::from_secs(30),
            Duration::ZERO,
        );
        tracker.handle_connection_status(status(StreamId::A, true, Some(1)));
        let now_us = chrono::Utc::now().timestamp_micros() as u64;
        tracker.record_stream_message(&observed_message(StreamId::A, now_us, now_us));
        std::thread::sleep(Duration::from_millis(1));

        assert_eq!(
            tracker.event_time_snapshot(StreamId::A).delivery_mode,
            DeliveryMode::Unknown
        );
    }

    #[test]
    fn comparison_is_eligible_for_overlapping_live_watermarks() {
        let result = comparison_eligibility(
            &live_snapshot(10_000_000),
            &live_snapshot(12_000_000),
            Duration::from_secs(5),
        );
        assert!(result.eligible);
        assert_eq!(result.reason, None);
    }

    #[test]
    fn comparison_reports_each_ineligibility_reason() {
        let live = live_snapshot(10_000_000);
        let catching_up = StreamEventTimeSnapshot {
            delivery_mode: DeliveryMode::CatchingUp,
            ..live
        };
        assert_eq!(
            comparison_eligibility(&catching_up, &live, Duration::from_secs(5)).reason,
            Some(ComparisonIneligibilityReason::CatchingUp)
        );

        let unknown = StreamEventTimeSnapshot {
            delivery_mode: DeliveryMode::Unknown,
            ..live
        };
        assert_eq!(
            comparison_eligibility(&unknown, &live, Duration::from_secs(5)).reason,
            Some(ComparisonIneligibilityReason::UnknownMode)
        );

        let uncovered = StreamEventTimeSnapshot {
            event_time_coverage: false,
            ..live
        };
        assert_eq!(
            comparison_eligibility(&uncovered, &live, Duration::from_secs(5)).reason,
            Some(ComparisonIneligibilityReason::MissingEventTimeCoverage)
        );

        assert_eq!(
            comparison_eligibility(
                &live_snapshot(1_000_000),
                &live_snapshot(20_000_000),
                Duration::from_secs(5),
            )
            .reason,
            Some(ComparisonIneligibilityReason::WatermarkSkew)
        );
    }

    #[tokio::test]
    async fn live_snapshot_serialization_keeps_raw_fields_and_adds_event_time_context() {
        let aggregator = super::StatsAggregator::new(
            "A".to_string(),
            "B".to_string(),
            "Baseline 1".to_string(),
            "Baseline 2".to_string(),
        );
        let mut receiver = aggregator.subscribe();
        let stats = Arc::new(RwLock::new(super::StreamStatsInternal {
            total_a: 10,
            total_b: 8,
        }));
        let mut tracker = UptimeTracker::new();
        tracker.handle_connection_status(status(StreamId::A, true, Some(1)));
        tracker.handle_connection_status(status(StreamId::B, true, Some(1)));
        let now_us = chrono::Utc::now().timestamp_micros() as u64;
        tracker.record_stream_message(&observed_message(StreamId::A, now_us - 1_000, now_us));
        tracker.record_stream_message(&observed_message(StreamId::B, now_us - 2_000, now_us));
        let uptime = Arc::new(RwLock::new(tracker));

        aggregator.process(&stats, &uptime);
        let snapshot = tokio::time::timeout(Duration::from_secs(1), receiver.recv())
            .await
            .expect("snapshot timeout")
            .expect("snapshot broadcast");
        let json = serde_json::to_value(snapshot).expect("serialize live snapshot");

        assert_eq!(json["stream_a"], 10);
        assert!(json.get("rate_a").is_some());
        assert_eq!(json["event_time_a"]["delivery_mode"], "live");
        assert_eq!(json["comparison"]["eligible"], true);
    }

    #[test]
    fn connection_latency_defaults_to_zero_without_samples() {
        let tracker = UptimeTracker::new();
        assert_eq!(tracker.get_connection_latency_a_ms(), 0.0);
        assert_eq!(tracker.get_connection_latency_b_ms(), 0.0);
    }

    #[test]
    fn connection_latency_uses_latest_successful_connect_time() {
        let mut tracker = UptimeTracker::new();

        tracker.handle_connection_status(status(StreamId::A, true, Some(40)));
        tracker.handle_connection_status(status(StreamId::A, false, None));
        tracker.handle_connection_status(status(StreamId::A, true, Some(120)));

        assert_eq!(tracker.get_connection_latency_a_ms(), 120.0);
        assert_eq!(tracker.get_avg_connect_time_a(), 80);
    }

    #[test]
    fn connection_latency_keeps_last_value_when_connect_sample_missing() {
        let mut tracker = UptimeTracker::new();

        tracker.handle_connection_status(status(StreamId::B, true, Some(75)));
        tracker.handle_connection_status(status(StreamId::B, false, None));
        tracker.handle_connection_status(status(StreamId::B, true, None));

        assert_eq!(tracker.get_connection_latency_b_ms(), 75.0);
    }

    #[test]
    fn data_idle_recovery_is_delivery_downtime_not_transport_downtime() {
        let mut tracker = UptimeTracker::new();
        tracker.handle_connection_status(status(StreamId::A, true, Some(10)));
        tracker.record_total_count(StreamId::A, 1);
        tracker.handle_connection_status(ConnectionStatus {
            stream_id: StreamId::A,
            connected: false,
            connected_at: None,
            connect_time_ms: None,
            delivery_available: false,
            reconnect_reason: Some(ReconnectReason::DataIdleTimeout),
            client_recovery: true,
        });
        std::thread::sleep(Duration::from_millis(2));
        tracker.handle_connection_status(status(StreamId::A, true, Some(5)));
        tracker.record_total_count(StreamId::A, 2);

        let availability = tracker.availability_snapshot(StreamId::A);
        assert_eq!(availability.transport_down_seconds, 0);
        assert!(availability.client_recovery_ms >= 1);
        assert_eq!(
            availability.last_reason,
            Some(ReconnectReason::DataIdleTimeout)
        );
        assert_eq!(availability.data_idle_reconnects(), 1);
        assert!(availability.delivery_available);
    }

    #[test]
    fn transport_failure_marks_both_models_down_and_retains_reason() {
        let mut tracker = UptimeTracker::new();
        tracker.handle_connection_status(status(StreamId::B, true, Some(10)));
        tracker.record_total_count(StreamId::B, 1);
        tracker.handle_connection_status(ConnectionStatus {
            stream_id: StreamId::B,
            connected: false,
            connected_at: None,
            connect_time_ms: None,
            delivery_available: false,
            reconnect_reason: Some(ReconnectReason::HandshakeFailure),
            client_recovery: false,
        });

        let availability = tracker.availability_snapshot(StreamId::B);
        assert!(!availability.transport_connected);
        assert!(!availability.delivery_available);
        assert_eq!(
            availability.last_reason,
            Some(ReconnectReason::HandshakeFailure)
        );
        assert_eq!(availability.reason_counts.get("handshakefailure"), Some(&1));
    }
}
