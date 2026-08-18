pub mod aggregator;

pub use aggregator::{
    comparison_eligibility, AvailabilitySnapshot, ComparisonEligibility,
    ComparisonIneligibilityReason, DeliveryMode, StatsAggregator, StreamEventTimeSnapshot,
    StreamStats, StreamStatsInternal, UptimeDetailedStats, UptimeMetricsSnapshot, UptimeTracker,
};
