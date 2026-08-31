//! Operational telemetry: Prometheus registry-backed metrics helpers.

use prometheus::{
    Encoder, IntCounter, IntGauge, IntGaugeVec, Opts, Registry, TextEncoder,
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
}

impl Metrics {
    pub fn new(process_epoch: String) -> Self {
        let base = |name: &str, help: &str| Opts::new(name, help);
        let label_opts = |name: &str, help: &str| {
            base(name, help).const_labels(HashMap::new())
        };
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

    pub fn set_last_useful_record_age(
        &self,
        stream_id: crate::stream::StreamId,
        age_seconds: u64,
    ) {
        self.last_useful_record_age_seconds
            .with_label_values(&[stream_label(stream_id)])
            .set(age_seconds as i64);
    }

    pub fn set_last_pong_age(&self, stream_id: crate::stream::StreamId, age_seconds: u64) {
        self.last_pong_age_seconds
            .with_label_values(&[stream_label(stream_id)])
            .set(age_seconds as i64);
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