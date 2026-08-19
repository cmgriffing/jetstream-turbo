pub mod aggregator;
pub mod comparison;

pub use aggregator::{
    comparison_eligibility, AvailabilitySnapshot, ComparisonEligibility,
    ComparisonIneligibilityReason, DeliveryMode, StatsAggregator, StreamEventTimeSnapshot,
    StreamStats, StreamStatsInternal, UptimeDetailedStats, UptimeMetricsSnapshot, UptimeTracker,
};
pub use comparison::{
    ComparisonEngine, ComparisonStreamState, ObservationConfig, PairwiseComparison,
    PairwiseComparisons,
};
