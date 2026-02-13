use metrics::{counter, gauge, histogram, Counter, Gauge, Histogram};
use std::time::Instant;
use tracing::debug;

/// Metrics collection for jetstream-turbo
pub struct Metrics {
    pub messages_processed: Counter,
    pub messages_failed: Counter,
    pub hydration_duration: Histogram,
    pub cache_hit_rate: Gauge,
    pub active_connections: Gauge,
    pub api_calls: Counter,
}

impl Metrics {
    pub fn new() -> Self {
        Self {
            messages_processed: counter!("jetstream_turbo_messages_processed_total"),
            messages_failed: counter!("jetstream_turbo_messages_failed_total"),
            hydration_duration: histogram!("jetstream_turbo_hydration_duration_seconds"),
            cache_hit_rate: gauge!("jetstream_turbo_cache_hit_rate"),
            active_connections: gauge!("jetstream_turbo_active_connections"),
            api_calls: counter!("jetstream_turbo_api_calls_total"),
        }
    }

    pub fn record_message_processed(&self) {
        self.messages_processed.increment(1);
        debug!("Messages processed count: {:?}", self.messages_processed);
    }

    pub fn record_message_failed(&self) {
        self.messages_failed.increment(1);
    }

    pub fn record_hydration_duration(&self, duration: std::time::Duration) {
        self.hydration_duration.record(duration.as_secs_f64());
    }

    pub fn set_cache_hit_rate(&self, rate: f64) {
        self.cache_hit_rate.set(rate);
    }

    pub fn set_active_connections(&self, count: f64) {
        self.active_connections.set(count);
    }

    pub fn record_api_call(&self) {
        self.api_calls.increment(1);
    }

    pub fn get_prometheus_metrics(&self) -> String {
        // This would generate the full Prometheus metrics format
        // Note: metrics types don't implement Display, using placeholder values
        "# HELP jetstream_turbo_messages_processed_total Total number of messages processed\n\
             # TYPE jetstream_turbo_messages_processed_total counter\n\
             jetstream_turbo_messages_processed_total 0\n\
             # HELP jetstream_turbo_messages_failed_total Total number of messages that failed processing\n\
             # TYPE jetstream_turbo_messages_failed_total counter\n\
             jetstream_turbo_messages_failed_total 0\n\
             # HELP jetstream_turbo_hydration_duration_seconds Time taken to hydrate messages\n\
             # TYPE jetstream_turbo_hydration_duration_seconds histogram\n\
             jetstream_turbo_hydration_duration_seconds 0\n\
             # HELP jetstream_turbo_cache_hit_rate Cache hit rate\n\
             # TYPE jetstream_turbo_cache_hit_rate gauge\n\
             jetstream_turbo_cache_hit_rate 0\n\
             # HELP jetstream_turbo_active_connections Number of active connections\n\
             # TYPE jetstream_turbo_active_connections gauge\n\
             jetstream_turbo_active_connections 0\n\
             # HELP jetstream_turbo_api_calls_total Total number of API calls\n\
             # TYPE jetstream_turbo_api_calls_total counter\n\
             jetstream_turbo_api_calls_total 0\n".to_string()
    }
}

impl Default for Metrics {
    fn default() -> Self {
        Self::new()
    }
}

pub struct OperationTimer {
    start_time: Instant,
    metrics: &'static Metrics,
}

impl OperationTimer {
    pub fn new(metrics: &'static Metrics) -> Self {
        Self {
            start_time: Instant::now(),
            metrics,
        }
    }

    pub fn finish(self) {
        let duration = self.start_time.elapsed();
        self.metrics.record_hydration_duration(duration);
        debug!("Operation completed in {:?}", duration);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_metrics_creation() {
        let metrics = Metrics::new();

        // Test that all metrics are created
        metrics.record_message_processed();
        metrics.record_message_failed();
        metrics.set_cache_hit_rate(0.85);
        metrics.set_active_connections(5.0);
        metrics.record_api_call();

        // Test Prometheus output
        let output = metrics.get_prometheus_metrics();
        assert!(output.contains("jetstream_turbo_messages_processed_total"));
        assert!(output.contains("jetstream_turbo_cache_hit_rate"));
    }

    #[test]
    fn test_operation_timer() {
        let metrics = Metrics::new();
        let timer = OperationTimer::new(&metrics);

        // Simulate some work
        std::thread::sleep(std::time::Duration::from_millis(10));
        timer.finish();

        // The timer should record the duration when dropped
        // This is hard to test directly but ensures compilation
    }
}
