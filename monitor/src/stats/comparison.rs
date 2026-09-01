use crate::stats::{ComparisonIneligibilityReason, DeliveryMode};
use crate::stream::{SourceEventObservation, StreamId};
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, HashMap, HashSet};
use std::time::Duration;

#[derive(Debug, Clone, Copy)]
pub struct ObservationConfig {
    pub horizon: Duration,
    pub bucket_width: Duration,
    pub settlement_allowance: Duration,
}

impl ObservationConfig {
    pub fn validate(self) -> Result<Self, &'static str> {
        if self.horizon.is_zero() || self.bucket_width.is_zero() {
            return Err("comparison horizon and bucket width must be positive");
        }
        if self.bucket_width > self.horizon {
            return Err("comparison bucket width must not exceed horizon");
        }
        if self.settlement_allowance >= self.horizon {
            return Err("comparison settlement allowance must be less than horizon");
        }
        Ok(self)
    }
}

impl Default for ObservationConfig {
    fn default() -> Self {
        Self {
            horizon: Duration::from_secs(5 * 60),
            bucket_width: Duration::from_secs(5),
            settlement_allowance: Duration::from_secs(10),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct PairwiseComparison {
    pub epoch_id: Option<u64>,
    pub window_start_us: Option<u64>,
    pub window_end_us: Option<u64>,
    pub covered_seconds: u64,
    pub left_unique_count: u64,
    pub right_unique_count: u64,
    pub left_rate: Option<f64>,
    pub right_rate: Option<f64>,
    pub count_delta: Option<i64>,
    pub rate_delta: Option<f64>,
    pub eligible: bool,
    pub reason: Option<ComparisonIneligibilityReason>,
}

impl PairwiseComparison {
    fn ineligible(epoch_id: Option<u64>, reason: ComparisonIneligibilityReason) -> Self {
        Self {
            epoch_id,
            window_start_us: None,
            window_end_us: None,
            covered_seconds: 0,
            left_unique_count: 0,
            right_unique_count: 0,
            left_rate: None,
            right_rate: None,
            count_delta: None,
            rate_delta: None,
            eligible: false,
            reason: Some(reason),
        }
    }
}

impl Default for PairwiseComparison {
    fn default() -> Self {
        Self::ineligible(None, ComparisonIneligibilityReason::LegacyUnknown)
    }
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
pub struct PairwiseComparisons {
    pub primary: PairwiseComparison,
    pub stream_a_baseline_1: PairwiseComparison,
    pub stream_a_baseline_2: PairwiseComparison,
    pub stream_b_baseline_1: PairwiseComparison,
    pub stream_b_baseline_2: PairwiseComparison,
}

#[derive(Debug, Clone, Copy)]
pub struct ComparisonStreamState {
    pub connected: bool,
    pub delivery_available: bool,
    pub mode: DeliveryMode,
    pub source_watermark_us: Option<u64>,
}

#[derive(Debug, Default)]
struct SourceBucket {
    raw_arrivals: u64,
    identities: HashSet<String>,
    identity_complete: bool,
}

#[derive(Debug, Default)]
struct SourceBuckets {
    watermark_us: Option<u64>,
    buckets: BTreeMap<u64, SourceBucket>,
    evicted_late_observation: bool,
}

impl SourceBuckets {
    fn record(&mut self, observation: &SourceEventObservation, config: ObservationConfig) {
        let width = duration_us(config.bucket_width);
        let start = observation.source_time_us / width * width;
        let previous_settled = self
            .watermark_us
            .map(|watermark| watermark.saturating_sub(duration_us(config.settlement_allowance)));
        let finalized =
            previous_settled.is_some_and(|settled| start.saturating_add(width) <= settled);
        self.watermark_us = Some(
            self.watermark_us
                .map_or(observation.source_time_us, |current| {
                    current.max(observation.source_time_us)
                }),
        );

        if finalized && !self.buckets.contains_key(&start) {
            self.evicted_late_observation = true;
        } else {
            let bucket = self.buckets.entry(start).or_insert_with(|| SourceBucket {
                identity_complete: true,
                ..SourceBucket::default()
            });
            bucket.raw_arrivals = bucket.raw_arrivals.saturating_add(1);
            if finalized {
                bucket.identity_complete = false;
            } else if let Some(identity) = &observation.source_event_id {
                bucket.identities.insert(identity.clone());
            } else {
                bucket.identity_complete = false;
            }
        }

        let oldest = self
            .watermark_us
            .unwrap_or_default()
            .saturating_sub(duration_us(config.horizon));
        self.buckets
            .retain(|bucket_start, _| bucket_start.saturating_add(width) > oldest);
    }
}

#[derive(Debug, Default)]
struct PairEpoch {
    next_epoch_id: u64,
    active_epoch_id: Option<u64>,
    epoch_start_us: Option<u64>,
    reason: Option<ComparisonIneligibilityReason>,
}

#[derive(Debug)]
pub struct ComparisonEngine {
    config: ObservationConfig,
    streams: HashMap<StreamId, SourceBuckets>,
    epochs: HashMap<(StreamId, StreamId), PairEpoch>,
}

const PAIRS: [(StreamId, StreamId); 5] = [
    (StreamId::A, StreamId::B),
    (StreamId::A, StreamId::Baseline1),
    (StreamId::A, StreamId::Baseline2),
    (StreamId::B, StreamId::Baseline1),
    (StreamId::B, StreamId::Baseline2),
];

impl ComparisonEngine {
    pub fn new(config: ObservationConfig) -> Self {
        Self {
            config: config
                .validate()
                .expect("validated comparison configuration"),
            streams: HashMap::new(),
            epochs: HashMap::new(),
        }
    }

    pub fn record(&mut self, stream_id: StreamId, observation: &SourceEventObservation) {
        self.streams
            .entry(stream_id)
            .or_default()
            .record(observation, self.config);
    }

    pub fn refresh(
        &mut self,
        states: &HashMap<StreamId, ComparisonStreamState>,
        watermark_skew_threshold: Duration,
    ) {
        for pair in PAIRS {
            let reason = self.prerequisite_reason(pair, states, watermark_skew_threshold);
            let epoch = self.epochs.entry(pair).or_default();
            if let Some(reason) = reason {
                epoch.active_epoch_id = None;
                epoch.epoch_start_us = None;
                epoch.reason = Some(reason);
                continue;
            }
            if epoch.active_epoch_id.is_none() {
                epoch.next_epoch_id = epoch.next_epoch_id.saturating_add(1);
                epoch.active_epoch_id = Some(epoch.next_epoch_id);
                epoch.epoch_start_us =
                    shared_watermark(self.streams.get(&pair.0), self.streams.get(&pair.1)).map(
                        |watermark| {
                            let width = duration_us(self.config.bucket_width);
                            watermark / width * width + width
                        },
                    );
            }
            epoch.reason = None;
        }
    }

    pub fn snapshots(&self) -> PairwiseComparisons {
        PairwiseComparisons {
            primary: self.snapshot((StreamId::A, StreamId::B)),
            stream_a_baseline_1: self.snapshot((StreamId::A, StreamId::Baseline1)),
            stream_a_baseline_2: self.snapshot((StreamId::A, StreamId::Baseline2)),
            stream_b_baseline_1: self.snapshot((StreamId::B, StreamId::Baseline1)),
            stream_b_baseline_2: self.snapshot((StreamId::B, StreamId::Baseline2)),
        }
    }

    fn prerequisite_reason(
        &self,
        pair: (StreamId, StreamId),
        states: &HashMap<StreamId, ComparisonStreamState>,
        watermark_skew_threshold: Duration,
    ) -> Option<ComparisonIneligibilityReason> {
        let Some(left) = states.get(&pair.0) else {
            return Some(ComparisonIneligibilityReason::Disconnected);
        };
        let Some(right) = states.get(&pair.1) else {
            return Some(ComparisonIneligibilityReason::Disconnected);
        };
        if !left.connected || !right.connected {
            return Some(ComparisonIneligibilityReason::Disconnected);
        }
        if !left.delivery_available || !right.delivery_available {
            return Some(ComparisonIneligibilityReason::IdleDelivery);
        }
        if left.mode == DeliveryMode::CatchingUp || right.mode == DeliveryMode::CatchingUp {
            return Some(ComparisonIneligibilityReason::CatchingUp);
        }
        if left.mode != DeliveryMode::Live || right.mode != DeliveryMode::Live {
            return Some(ComparisonIneligibilityReason::UnknownMode);
        }
        if left
            .source_watermark_us
            .zip(right.source_watermark_us)
            .is_none_or(|(left, right)| {
                left.abs_diff(right) > duration_us(watermark_skew_threshold)
            })
        {
            return Some(ComparisonIneligibilityReason::WatermarkSkew);
        }
        None
    }

    fn snapshot(&self, pair: (StreamId, StreamId)) -> PairwiseComparison {
        let epoch = self.epochs.get(&pair);
        let epoch_id = epoch.and_then(|epoch| epoch.active_epoch_id);
        if let Some(reason) = epoch.and_then(|epoch| epoch.reason) {
            return PairwiseComparison::ineligible(epoch_id, reason);
        }
        let Some(epoch_start) = epoch.and_then(|epoch| epoch.epoch_start_us) else {
            return PairwiseComparison::ineligible(
                epoch_id,
                ComparisonIneligibilityReason::SettlementPending,
            );
        };
        let (Some(left), Some(right)) = (self.streams.get(&pair.0), self.streams.get(&pair.1))
        else {
            return PairwiseComparison::ineligible(
                epoch_id,
                ComparisonIneligibilityReason::MissingSharedCoverage,
            );
        };
        if left.evicted_late_observation || right.evicted_late_observation {
            return PairwiseComparison::ineligible(
                epoch_id,
                ComparisonIneligibilityReason::IncompleteIdentityCoverage,
            );
        }
        let Some(end) = shared_settled_end(Some(left), Some(right), self.config) else {
            return PairwiseComparison::ineligible(
                epoch_id,
                ComparisonIneligibilityReason::SettlementPending,
            );
        };
        let width = duration_us(self.config.bucket_width);
        let end = end / width * width;
        if end <= epoch_start {
            return PairwiseComparison::ineligible(
                epoch_id,
                ComparisonIneligibilityReason::SettlementPending,
            );
        }
        let mut left_count = 0u64;
        let mut right_count = 0u64;
        let mut cursor = epoch_start;
        while cursor.saturating_add(width) <= end {
            let (Some(left_bucket), Some(right_bucket)) =
                (left.buckets.get(&cursor), right.buckets.get(&cursor))
            else {
                return PairwiseComparison::ineligible(
                    epoch_id,
                    ComparisonIneligibilityReason::MissingSharedCoverage,
                );
            };
            if !left_bucket.identity_complete || !right_bucket.identity_complete {
                return PairwiseComparison::ineligible(
                    epoch_id,
                    ComparisonIneligibilityReason::IncompleteIdentityCoverage,
                );
            }
            left_count = left_count.saturating_add(left_bucket.identities.len() as u64);
            right_count = right_count.saturating_add(right_bucket.identities.len() as u64);
            cursor = cursor.saturating_add(width);
        }
        let covered_seconds = end.saturating_sub(epoch_start) / 1_000_000;
        if covered_seconds == 0 {
            return PairwiseComparison::ineligible(
                epoch_id,
                ComparisonIneligibilityReason::SettlementPending,
            );
        }
        let left_rate = left_count as f64 / covered_seconds as f64;
        let right_rate = right_count as f64 / covered_seconds as f64;
        PairwiseComparison {
            epoch_id,
            window_start_us: Some(epoch_start),
            window_end_us: Some(end),
            covered_seconds,
            left_unique_count: left_count,
            right_unique_count: right_count,
            left_rate: Some(left_rate),
            right_rate: Some(right_rate),
            count_delta: Some(left_count as i64 - right_count as i64),
            rate_delta: Some(left_rate - right_rate),
            eligible: true,
            reason: None,
        }
    }
}

fn shared_settled_end(
    left: Option<&SourceBuckets>,
    right: Option<&SourceBuckets>,
    config: ObservationConfig,
) -> Option<u64> {
    left?
        .watermark_us
        .zip(right?.watermark_us)
        .map(|(left, right)| {
            left.min(right)
                .saturating_sub(duration_us(config.settlement_allowance))
        })
}

fn shared_watermark(left: Option<&SourceBuckets>, right: Option<&SourceBuckets>) -> Option<u64> {
    left?
        .watermark_us
        .zip(right?.watermark_us)
        .map(|(left, right)| left.min(right))
}

fn duration_us(duration: Duration) -> u64 {
    duration.as_micros().min(u64::MAX as u128) as u64
}

#[cfg(test)]
mod tests {
    use super::*;

    fn observation(time: u64, identity: Option<&str>) -> SourceEventObservation {
        SourceEventObservation {
            source_time_us: time,
            observed_at_us: time,
            lag_us: 0,
            clock_skew_us: 0,
            source_event_id: identity.map(str::to_string),
            ingress_ordinal: None,
            turbo_epoch: None,
        }
    }

    fn config() -> ObservationConfig {
        ObservationConfig {
            horizon: Duration::from_secs(30),
            bucket_width: Duration::from_secs(5),
            settlement_allowance: Duration::from_secs(5),
        }
    }

    fn live_states(
        left_watermark: u64,
        right_watermark: u64,
    ) -> HashMap<StreamId, ComparisonStreamState> {
        [
            StreamId::A,
            StreamId::B,
            StreamId::Baseline1,
            StreamId::Baseline2,
        ]
        .into_iter()
        .map(|stream_id| {
            let watermark = if stream_id == StreamId::A {
                left_watermark
            } else {
                right_watermark
            };
            (
                stream_id,
                ComparisonStreamState {
                    connected: true,
                    delivery_available: true,
                    mode: DeliveryMode::Live,
                    source_watermark_us: Some(watermark),
                },
            )
        })
        .collect()
    }

    #[test]
    fn duplicate_overlap_counts_unique_identity_once_and_memory_is_bounded() {
        let mut buckets = SourceBuckets::default();
        buckets.record(&observation(1_000_000, Some("same")), config());
        buckets.record(&observation(2_000_000, Some("same")), config());
        for second in (5..=100).step_by(5) {
            buckets.record(
                &observation(second * 1_000_000, Some(&format!("{second}"))),
                config(),
            );
        }
        assert!(buckets.buckets.len() <= 7);
    }

    #[test]
    fn late_before_settlement_is_included_but_after_finalization_is_incomplete() {
        let mut buckets = SourceBuckets::default();
        buckets.record(&observation(1_000_000, Some("first")), config());
        buckets.record(&observation(4_000_000, Some("late-in-open")), config());
        buckets.record(&observation(20_000_000, Some("advance")), config());
        buckets.record(&observation(3_000_000, Some("too-late")), config());
        assert!(!buckets.buckets.get(&0).unwrap().identity_complete);
        assert_eq!(buckets.buckets.get(&0).unwrap().identities.len(), 2);
    }

    #[test]
    fn epoch_excludes_replay_surplus_and_counts_a_shared_settled_window() {
        let mut engine = ComparisonEngine::new(config());
        engine.record(StreamId::A, &observation(1_000_000, Some("replay-1")));
        engine.record(StreamId::A, &observation(2_000_000, Some("replay-2")));
        engine.record(StreamId::B, &observation(2_000_000, Some("startup")));
        engine.refresh(&live_states(2_000_000, 2_000_000), Duration::from_secs(5));

        for stream in [StreamId::A, StreamId::B] {
            engine.record(stream, &observation(6_000_000, Some("shared")));
            engine.record(stream, &observation(15_000_000, Some("advance")));
        }
        engine.refresh(&live_states(15_000_000, 15_000_000), Duration::from_secs(5));
        let comparison = engine.snapshots().primary;

        assert!(comparison.eligible);
        assert_eq!(comparison.window_start_us, Some(5_000_000));
        assert_eq!(comparison.window_end_us, Some(10_000_000));
        assert_eq!(comparison.count_delta, Some(0));
    }

    #[test]
    fn catch_up_skew_disconnect_and_reconnect_end_the_epoch() {
        let mut engine = ComparisonEngine::new(config());
        for stream in [StreamId::A, StreamId::B] {
            engine.record(stream, &observation(1_000_000, Some("start")));
        }
        let mut states = live_states(1_000_000, 1_000_000);
        engine.refresh(&states, Duration::from_secs(5));
        let first_epoch = engine.snapshots().primary.epoch_id;

        states.get_mut(&StreamId::A).unwrap().mode = DeliveryMode::CatchingUp;
        engine.refresh(&states, Duration::from_secs(5));
        assert_eq!(
            engine.snapshots().primary.reason,
            Some(ComparisonIneligibilityReason::CatchingUp)
        );
        states = live_states(20_000_000, 1_000_000);
        engine.refresh(&states, Duration::from_secs(5));
        assert_eq!(
            engine.snapshots().primary.reason,
            Some(ComparisonIneligibilityReason::WatermarkSkew)
        );
        states = live_states(1_000_000, 1_000_000);
        states.get_mut(&StreamId::A).unwrap().connected = false;
        engine.refresh(&states, Duration::from_secs(5));
        assert_eq!(
            engine.snapshots().primary.reason,
            Some(ComparisonIneligibilityReason::Disconnected)
        );
        states.get_mut(&StreamId::A).unwrap().connected = true;
        engine.refresh(&states, Duration::from_secs(5));
        assert!(engine.snapshots().primary.epoch_id > first_epoch);
    }
}
