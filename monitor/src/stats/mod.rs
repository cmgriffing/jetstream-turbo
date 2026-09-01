pub mod aggregator;
pub mod comparison;
pub mod ordinal;

pub use aggregator::{
    comparison_eligibility, AvailabilitySnapshot, ComparisonEligibility,
    ComparisonIneligibilityReason, DeliveryMode, StatsAggregator, StreamEventTimeSnapshot,
    StreamStats, StreamStatsInternal, UptimeDetailedStats, UptimeMetricsSnapshot, UptimeTracker,
};
pub use ordinal::{
    OrdinalAccounting, OrdinalClassification, OrdinalRing, OrdinalStreamSnapshot,
    OrdinalThresholds, ORDINAL_RING_SLOTS,
};
pub use comparison::{
    ComparisonEngine, ComparisonStreamState, ObservationConfig, PairwiseComparison,
    PairwiseComparisons,
};
