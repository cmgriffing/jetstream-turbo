//! Operational telemetry: Prometheus registry-backed metrics helpers.

use prometheus::{
    Encoder, Histogram, HistogramOpts, IntCounter, IntGauge, IntGaugeVec, Opts, Registry,
    TextEncoder,
};
use std::collections::HashMap;

/// Bounded metric label values must come from these stable identifiers only.
pub fn stream_label(stream_id: crate::stream::StreamId) -> &'static str {
    match stream_id {
        crate::stream::StreamId::A => "a",
        crate::stream::StreamId::B => "b",
        crate::stream::StreamId::Baseline1 => "baseline1",
        crate::stream::StreamId::Baseline2 => "baseline2",
    }
}

/// Registry-backed process metrics for the monitor.
#[allow(dead_code)]
pub struct Metrics {
    pub registry: Registry,
    process_epoch: String,
    process_start_seconds_ago: IntGauge,
    transport_state: IntGaugeVec,
    delivery_state: IntGaugeVec,
    last_useful_record_age_seconds: IntGaugeVec,
    last_pong_age_seconds: IntGaugeVec,
    source_lag_seconds: IntGaugeVec,
    outage_episode_count: IntCounter,
    reconnect_attempt_count: IntCounter,
    idle_episode_count: IntCounter,
    record_total: IntCounter,
    storage_failure_count: IntCounter,
    connection_epoch: IntGaugeVec,
    hourly_snapshot_age_seconds: IntGauge,
    dashboard_subscribers: IntGauge,
    incident_retained_count: IntGauge,
    incident_last_success_age_seconds: IntGauge,
    hourly_last_success_age_seconds: IntGauge,
    transport_recovery_seconds: Histogram,
    delivery_recovery_seconds: Histogram,
    data_gap_seconds: Histogram,
}

impl Metrics {
    pub fn new(process_epoch: String) -> Self {
        let base = |name: &str, help: &str| Opts::new(name, help);
        let label_opts = |name: &str, help: &str| base(name, help).const_labels(HashMap::new());
        let _ = label_opts;
        let registry = Registry::new();

        let process_start_seconds_ago = IntGauge::with_opts(base(
            "monitor_process_start_seconds_ago",
            "Seconds since the monitor process started; changes identify process resets.",
        ))
        .expect("valid process start gauge");

        let transport_state = IntGaugeVec::new(
            base(
                "monitor_stream_transport_state",
                "Transport state per stream: 0=disconnected, 1=connecting, 2=connected.",
            ),
            &["stream"],
        )
        .expect("valid transport state vec");

        let delivery_state = IntGaugeVec::new(
            base(
                "monitor_stream_delivery_state",
                "Delivery state per stream: 0=unknown, 1=waiting, 2=delivering, 3=idle.",
            ),
            &["stream"],
        )
        .expect("valid delivery state vec");

        let last_useful_record_age_seconds = IntGaugeVec::new(
            base(
                "monitor_stream_last_useful_record_age_seconds",
                "Seconds since the last useful text record per stream.",
            ),
            &["stream"],
        )
        .expect("valid record age vec");

        let last_pong_age_seconds = IntGaugeVec::new(
            base(
                "monitor_stream_last_pong_age_seconds",
                "Seconds since the last peer liveness evidence per stream.",
            ),
            &["stream"],
        )
        .expect("valid pong age vec");

        let source_lag_seconds = IntGaugeVec::new(
            base(
                "monitor_stream_source_lag_seconds",
                "Source event-time lag in seconds per stream when known.",
            ),
            &["stream"],
        )
        .expect("valid source lag vec");

        let outage_episode_count = IntCounter::with_opts(base(
            "monitor_outage_episode_total",
            "Count of connected-to-disconnected transport outage episodes.",
        ))
        .expect("valid outage counter");

        let reconnect_attempt_count = IntCounter::with_opts(base(
            "monitor_reconnect_attempt_total",
            "Count of failed reconnect or handshake attempts.",
        ))
        .expect("valid reconnect counter");

        let idle_episode_count = IntCounter::with_opts(base(
            "monitor_idle_episode_total",
            "Count of delivery-idle episodes detected on heartbeat-responsive sockets.",
        ))
        .expect("valid idle counter");

        let record_total = IntCounter::with_opts(base(
            "monitor_record_total",
            "Total useful text records observed across all streams.",
        ))
        .expect("valid record counter");

        let storage_failure_count = IntCounter::with_opts(base(
            "monitor_storage_failure_total",
            "Count of persistence failures for incident or hourly writes.",
        ))
        .expect("valid storage failure counter");

        let connection_epoch = IntGaugeVec::new(
            base(
                "monitor_stream_connection_epoch",
                "Connection epoch per stream; increments on each handshake.",
            ),
            &["stream"],
        )
        .expect("valid connection epoch vec");

        let hourly_snapshot_age_seconds = IntGauge::with_opts(base(
            "monitor_hourly_snapshot_age_seconds",
            "Seconds since the last successful hourly snapshot write.",
        ))
        .expect("valid hourly snapshot age gauge");

        let dashboard_subscribers = IntGauge::with_opts(base(
            "monitor_dashboard_subscribers",
            "Live dashboard subscribers.",
        ))
        .expect("valid subscribers gauge");

        let incident_retained_count = IntGauge::with_opts(base(
            "monitor_incidents_retained",
            "Number of incidents currently retained in the ledger.",
        ))
        .expect("valid retained incidents gauge");

        let incident_last_success_age_seconds = IntGauge::with_opts(base(
            "monitor_incident_last_success_age_seconds",
            "Seconds since the last successful incident write.",
        ))
        .expect("valid incident write age gauge");

        let hourly_last_success_age_seconds = IntGauge::with_opts(base(
            "monitor_hourly_last_success_age_seconds",
            "Seconds since the last successful hourly write.",
        ))
        .expect("valid hourly write age gauge");

        let transport_recovery_seconds = Histogram::with_opts(HistogramOpts::new(
            "monitor_transport_recovery_seconds",
            "Transport recovery duration from outage boundary to handshake success.",
        ))
        .expect("valid transport recovery histogram");

        let delivery_recovery_seconds = Histogram::with_opts(HistogramOpts::new(
            "monitor_delivery_recovery_seconds",
            "Delivery recovery duration from disruption boundary to first useful record.",
        ))
        .expect("valid delivery recovery histogram");

        let data_gap_seconds = Histogram::with_opts(HistogramOpts::new(
            "monitor_data_gap_seconds",
            "Total detected data-gap duration.",
        ))
        .expect("valid data gap histogram");

        let _ = registry.register(Box::new(process_start_seconds_ago.clone()));
        let _ = registry.register(Box::new(transport_state.clone()));
        let _ = registry.register(Box::new(delivery_state.clone()));
        let _ = registry.register(Box::new(last_useful_record_age_seconds.clone()));
        let _ = registry.register(Box::new(last_pong_age_seconds.clone()));
        let _ = registry.register(Box::new(source_lag_seconds.clone()));
        let _ = registry.register(Box::new(outage_episode_count.clone()));
        let _ = registry.register(Box::new(reconnect_attempt_count.clone()));
        let _ = registry.register(Box::new(idle_episode_count.clone()));
        let _ = registry.register(Box::new(record_total.clone()));
        let _ = registry.register(Box::new(storage_failure_count.clone()));
        let _ = registry.register(Box::new(connection_epoch.clone()));
        let _ = registry.register(Box::new(hourly_snapshot_age_seconds.clone()));
        let _ = registry.register(Box::new(dashboard_subscribers.clone()));
        let _ = registry.register(Box::new(incident_retained_count.clone()));
        let _ = registry.register(Box::new(incident_last_success_age_seconds.clone()));
        let _ = registry.register(Box::new(hourly_last_success_age_seconds.clone()));
        let _ = registry.register(Box::new(transport_recovery_seconds.clone()));
        let _ = registry.register(Box::new(delivery_recovery_seconds.clone()));
        let _ = registry.register(Box::new(data_gap_seconds.clone()));

        Self {
            registry,
            process_epoch,
            process_start_seconds_ago,
            transport_state,
            delivery_state,
            last_useful_record_age_seconds,
            last_pong_age_seconds,
            source_lag_seconds,
            outage_episode_count,
            reconnect_attempt_count,
            idle_episode_count,
            record_total,
            storage_failure_count,
            connection_epoch,
            hourly_snapshot_age_seconds,
            dashboard_subscribers,
            incident_retained_count,
            incident_last_success_age_seconds,
            hourly_last_success_age_seconds,
            transport_recovery_seconds,
            delivery_recovery_seconds,
            data_gap_seconds,
        }
    }

    pub fn process_epoch(&self) -> &str {
        &self.process_epoch
    }

    pub fn record_incident_storage_failure(&self) {
        self.storage_failure_count.inc();
    }

    pub fn record_transport_outage(&self) {
        self.outage_episode_count.inc();
    }

    pub fn record_reconnect_attempt(&self) {
        self.reconnect_attempt_count.inc();
    }

    pub fn record_idle_episode(&self) {
        self.idle_episode_count.inc();
    }

    pub fn record_useful_record(&self) {
        self.record_total.inc();
    }

    pub fn set_transport_state(
        &self,
        stream_id: crate::stream::StreamId,
        state: crate::stream::TransportState,
    ) {
        let value = match state {
            crate::stream::TransportState::Disconnected => 0,
            crate::stream::TransportState::Connecting => 1,
            crate::stream::TransportState::Connected => 2,
        };
        self.transport_state
            .with_label_values(&[stream_label(stream_id)])
            .set(value);
    }

    pub fn set_delivery_state(
        &self,
        stream_id: crate::stream::StreamId,
        state: crate::stream::DeliveryState,
    ) {
        let value = match state {
            crate::stream::DeliveryState::Unknown => 0,
            crate::stream::DeliveryState::Waiting => 1,
            crate::stream::DeliveryState::Delivering => 2,
            crate::stream::DeliveryState::Idle => 3,
        };
        self.delivery_state
            .with_label_values(&[stream_label(stream_id)])
            .set(value);
    }

    pub fn set_last_useful_record_age(&self, stream_id: crate::stream::StreamId, age_seconds: u64) {
        self.last_useful_record_age_seconds
            .with_label_values(&[stream_label(stream_id)])
            .set(age_seconds as i64);
    }

    pub fn set_last_pong_age(&self, stream_id: crate::stream::StreamId, age_seconds: u64) {
        self.last_pong_age_seconds
            .with_label_values(&[stream_label(stream_id)])
            .set(age_seconds as i64);
    }

    pub fn set_connection_epoch(&self, stream_id: crate::stream::StreamId, epoch: u64) {
        self.connection_epoch
            .with_label_values(&[stream_label(stream_id)])
            .set(epoch as i64);
    }

    pub fn set_process_start_seconds_ago(&self, seconds: u64) {
        self.process_start_seconds_ago.set(seconds as i64);
    }

    pub fn set_hourly_snapshot_age(&self, age_seconds: u64) {
        self.hourly_snapshot_age_seconds.set(age_seconds as i64);
    }

    pub fn set_dashboard_subscribers(&self, count: i64) {
        self.dashboard_subscribers.set(count);
    }

    pub fn set_incident_retained_count(&self, count: u64) {
        self.incident_retained_count.set(count as i64);
    }

    pub fn set_incident_last_success_age(&self, age_seconds: u64) {
        self.incident_last_success_age_seconds
            .set(age_seconds as i64);
    }

    pub fn set_hourly_last_success_age(&self, age_seconds: u64) {
        self.hourly_last_success_age_seconds.set(age_seconds as i64);
    }

    pub fn observe_transport_recovery(&self, seconds: f64) {
        self.transport_recovery_seconds
            .observe(seconds.clamp(0.0, 10_000.0));
    }

    pub fn observe_delivery_recovery(&self, seconds: f64) {
        self.delivery_recovery_seconds
            .observe(seconds.clamp(0.0, 86_400.0));
    }

    pub fn observe_data_gap(&self, seconds: f64) {
        self.data_gap_seconds.observe(seconds.clamp(0.0, 86_400.0));
    }

    pub fn set_source_lag(&self, stream_id: crate::stream::StreamId, lag_seconds: u64) {
        self.source_lag_seconds
            .with_label_values(&[stream_label(stream_id)])
            .set(lag_seconds as i64);
    }

    pub fn render(&self) -> String {
        let encoder = TextEncoder::new();
        let mut buffer = Vec::new();
        if encoder
            .encode(&self.registry.gather(), &mut buffer)
            .is_err()
        {
            return String::new();
        }
        String::from_utf8(buffer).unwrap_or_default()
    }

    pub fn content_type() -> &'static str {
        prometheus::TEXT_FORMAT
    }
}
#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn renders_required_metric_families() {
        let metrics = Metrics::new("test-epoch".to_string());
        metrics.record_transport_outage();
        metrics.record_reconnect_attempt();
        metrics.record_idle_episode();
        metrics.record_useful_record();
        metrics.record_incident_storage_failure();
        metrics.set_transport_state(
            crate::stream::StreamId::A,
            crate::stream::TransportState::Connected,
        );
        metrics.set_delivery_state(
            crate::stream::StreamId::A,
            crate::stream::DeliveryState::Delivering,
        );
        metrics.set_last_useful_record_age(crate::stream::StreamId::A, 3);
        metrics.set_last_pong_age(crate::stream::StreamId::A, 1);
        metrics.set_source_lag(crate::stream::StreamId::A, 2);
        metrics.set_connection_epoch(crate::stream::StreamId::A, 7);
        metrics.set_hourly_snapshot_age(120);
        metrics.set_dashboard_subscribers(4);
        metrics.set_incident_retained_count(11);

        let out = metrics.render();
        for family in [
            "monitor_process_start_seconds_ago",
            "monitor_stream_transport_state",
            "monitor_stream_delivery_state",
            "monitor_stream_last_useful_record_age_seconds",
            "monitor_stream_last_pong_age_seconds",
            "monitor_stream_source_lag_seconds",
            "monitor_outage_episode_total",
            "monitor_reconnect_attempt_total",
            "monitor_idle_episode_total",
            "monitor_record_total",
            "monitor_storage_failure_total",
            "monitor_stream_connection_epoch",
            "monitor_hourly_snapshot_age_seconds",
            "monitor_dashboard_subscribers",
            "monitor_incidents_retained",
            "monitor_transport_recovery_seconds",
            "monitor_delivery_recovery_seconds",
            "monitor_data_gap_seconds",
        ] {
            assert!(
                out.contains(&format!("# HELP {family}")),
                "missing family {family}"
            );
        }
    }

    #[test]
    fn stream_labels_are_bounded_and_exclude_sensitive_values() {
        let metrics = Metrics::new("test-epoch".to_string());
        metrics.set_transport_state(
            crate::stream::StreamId::Baseline1,
            crate::stream::TransportState::Connected,
        );
        let out = metrics.render();
        assert!(out.contains("stream=\"baseline1\""));
        for forbidden in ["ws://", "http", "did:", "Bearer", "password"] {
            assert!(
                !out.contains(forbidden),
                "output must not contain {forbidden}"
            );
        }
    }

    #[test]
    fn content_type_is_prometheus_text_format() {
        assert_eq!(Metrics::content_type(), "text/plain; version=0.0.4");
    }
}
