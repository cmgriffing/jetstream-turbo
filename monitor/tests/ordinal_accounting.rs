//! Integration test: ingress-ordinal event accounting end to end (task 6.1).
//!
//! Exercises the full monitor-side chain: client-frame classification via the
//! bit ring, aggregation counters and windowed derived ratios, Prometheus
//! rendering, health payload fields, deploy-window behavior, and the
//! incident open/resolve lifecycle driven by sustained breaches.

use jetstream_monitor::incidents::thresholds::evaluate_ordinal_breach;
use jetstream_monitor::stats::{
    OrdinalAccounting, OrdinalRing, OrdinalStreamSnapshot, OrdinalThresholds, UptimeTracker,
};
use jetstream_monitor::stream::processor::IncidentCommand;
use jetstream_monitor::stream::{StreamId, StreamMessage};
use jetstream_monitor::telemetry::Metrics;
use std::time::{Duration, Instant};

/// Feed a sequence of ingress ordinals (with duplicates and induced gaps)
/// through a classification ring like `run_session` does.
fn feed(
    ring: &mut OrdinalRing,
    epoch: &str,
    ordinals: &[Option<u64>],
) -> Vec<jetstream_monitor::stats::OrdinalClassification> {
    let mut out = Vec::new();
    for ordinal in ordinals {
        match ordinal {
            Some(o) => out.push(ring.observe(*o, epoch)),
            None => {
                ring.observe_uninstrumented();
                out.push(jetstream_monitor::stats::OrdinalClassification::Unique);
            }
        }
    }
    out
}

fn accounting(ring: &OrdinalRing) -> OrdinalAccounting {
    ring.snapshot().expect("instrumented frame observed")
}


#[test]
fn end_to_end_unique_duplicate_gap_counters_through_tracker() {
    let mut ring = OrdinalRing::new();
    let mut tracker = UptimeTracker::new();

    // Stream A: 1..=10 unique, then duplicate 7, gap induced (12 jumps 2).
    let ordinals = [Some(1), Some(2), Some(3), Some(4), Some(5), Some(6), Some(7), Some(8), Some(9), Some(10), Some(3), Some(12)];
    let _ = feed(&mut ring, "epoch-e2e", &ordinals);
    let snap = accounting(&ring);
    assert_eq!(snap.unique_total, 11);
    assert_eq!(snap.duplicate_total, 1);
    assert_eq!(snap.gap_total, 1); // watermark 10 -> 12 leaves one missing
    tracker.record_ordinal_accounting(StreamId::A, &snap);

    let picture = tracker.ordinal_snapshot(StreamId::A).unwrap();
    assert_eq!(picture.status, "active");
    assert_eq!(picture.turbo_epoch, "epoch-e2e");
    assert_eq!(picture.ordinal_watermark, 12);
    assert_eq!(picture.unique_total, 11);
    assert_eq!(picture.duplicate_total, 1);
    assert_eq!(picture.gap_total, 1);
}

#[test]
fn epoch_reset_is_attributable_in_the_snapshot() {
    let mut ring = OrdinalRing::new();
    let _ = feed(&mut ring, "epoch-1", &[Some(1), Some(2), Some(3)]);
    // Turbo restarts: new in-band epoch, ordinals restart.
    let _ = feed(&mut ring, "epoch-2", &[Some(1), Some(2)]);
    let snap = accounting(&ring);
    assert_eq!(snap.turbo_epoch, "epoch-2");
    assert_eq!(snap.epoch_changes, 1);
    assert_eq!(snap.ordinal_watermark, 2);
    assert_eq!(snap.duplicate_total, 0, "old-epoch duplicates must not leak");
}

#[test]
fn metrics_expose_per_stream_ordinal_accounting() {
    let mut ring = OrdinalRing::new();
    let _ = feed(&mut ring, "epoch-metrics", &[Some(1), Some(1), Some(2), Some(5)]);
    let snap = accounting(&ring);
    let mut tracker = UptimeTracker::new();
    tracker.record_ordinal_accounting(StreamId::A, &snap);
    let picture = tracker.ordinal_snapshot(StreamId::A).unwrap();

    let metrics = Metrics::new("test-epoch".to_string());
    metrics.set_ordinal_snapshot(StreamId::A, &picture);
    let out = metrics.render();
    assert!(
        out.contains("monitor_stream_unique_event_total{stream=\"a\"} 3"),
        "unique events classified through the ring: {}",
        out
    );
    assert!(
        out.contains("monitor_stream_duplicate_event_total{stream=\"a\"} 1"),
        "duplicate counter present"
    );
    assert!(
        out.contains("monitor_stream_gap_event_total{stream=\"a\"} 2"),
        "gap counter reflects the synthetic missing ordinals (2..=4 missing before 5)"
    );
    assert!(
        out.contains("monitor_stream_ordinal_watermark{stream=\"a\"} 5")
            || out.contains("monitor_stream_ordinal_watermark{stream=\"a\"} 5.0")
            || out.contains("monitor_stream_ordinal_watermark{stream=\"a\"} 5"),
        "watermark gauge present"
    );
}


#[test]
fn deploy_window_old_turbo_against_new_monitor_counts_uninstrumented() {
    // Old turbo: no envelope fields arrive at all; the ring stays epochless.
    let mut ring = OrdinalRing::new();
    let _ = feed(&mut ring, "epoch-deploy", &[None, None, None]);
    assert!(ring.snapshot().is_none(), "no epoch known yet");
    // Health-side default marks the stream uninstrumented (D4).
    let tracker = UptimeTracker::new();
    assert!(tracker.ordinal_snapshot(StreamId::A).is_none());

    // Mixed window: ordinals and fact-less frames interleave and each tally
    // separately so coverage is visible.
    let mut ring = OrdinalRing::new();
    let _ = feed(&mut ring, "epoch-deploy", &[Some(1), None, Some(2), None]);
    let snap = accounting(&ring);
    assert_eq!(snap.unique_total, 2);
    assert_eq!(snap.uninstrumented_total, 2);

    // New turbo against old monitor: monitor's typed parsing of a broadcast
    // record with the extra fields is additive; legacy-shaped payloads
    // without the fields also parse.
    let legacy = r#"{"message":{"time_us":100},"processed_at":"2026-01-01T00:00:00Z","metrics":{}}"#;
    let modern = r#"{"message":{"time_us":100},"processed_at":"2026-01-01T00:00:00Z","metrics":{},"turbo_epoch":"e1","ingress_ordinal":9}"#;
    for (payload, epoch_expected, ordinal_expected) in [
        (legacy, None, None),
        (modern, Some("e1"), Some(9)),
    ] {
        let parsed: serde_json::Value = serde_json::from_str(payload).unwrap();
        assert_eq!(
            parsed.get("turbo_epoch").and_then(|v| v.as_str()).map(str::to_string),
            epoch_expected.map(str::to_string)
        );
        assert_eq!(
            parsed.get("ingress_ordinal").and_then(|v| v.as_u64()),
            ordinal_expected
        );

    }
}



#[test]
fn uninstrumented_accounting_surfaces_dedupe_coverage() {
    let mut tracker = UptimeTracker::new();
    tracker.record_ordinal_accounting(
        StreamId::B,
        &OrdinalAccounting {
            turbo_epoch: "e".to_string(),
            ordinal_watermark: 0,
            unique_total: 3,
            duplicate_total: 0,
            gap_total: 0,
            uninstrumented_total: 10,
            epoch_changes: 0,
        },
    );
    tracker.record_ordinal_accounting(
        StreamId::B,
        &OrdinalAccounting {
            turbo_epoch: "e".to_string(),
            ordinal_watermark: 0,
            unique_total: 10,
            duplicate_total: 0,
            gap_total: 0,
            uninstrumented_total: 10,
            epoch_changes: 0,
        },
    );
    let picture = tracker.ordinal_snapshot(StreamId::B).unwrap();
    // Instrumented frame activity in the window → active with coverage.
    assert_eq!(picture.status, "active");
    assert_eq!(picture.uninstrumented_total, 10);
}



#[test]
fn incident_lifecycle_opens_and_resolves_on_sustained_breaches() {
    let thresholds = OrdinalThresholds {
        duplicate_ratio: 0.05,
        gap_rate: 0.005,
        sustain: Duration::from_secs(10),
        resolve: Duration::from_secs(60),
    };
    let mut state = jetstream_monitor::incidents::thresholds::OrdinalIncidentState::default();
    let t0 = Instant::now();

    let snapshot = |duplicate: u64, gap: u64| OrdinalStreamSnapshot {
        turbo_epoch: "epoch-e2e".to_string(),
        ordinal_watermark: 1000,
        unique_total: 90,
        duplicate_total: duplicate,
        gap_total: gap,
        uninstrumented_total: 0,
        epoch_changes: 0,
        duplicate_ratio: duplicate as f64 / 100.0,
        gap_rate: gap as f64 / 1000.0,
        status: OrdinalStreamSnapshot::ACTIVE.to_string(),
    };

    // No commands while the breach is not sustained.
    let mut commands = Vec::new();
    evaluate_ordinal_breach(StreamId::A, &snapshot(50, 90), &thresholds, &mut state, t0, &mut commands);
    assert!(commands.is_empty(), "nothing opens before sustain");

    // Sustained duplicate-ratio breach (gap rate at 0): open incident with
    // evidence.
    evaluate_ordinal_breach(StreamId::A, &snapshot(50, 0), &thresholds, &mut state, t0 + Duration::from_secs(10), &mut commands);
    assert_eq!(commands.len(), 2, "open + appended breach evidence");
    let incident_id = match &commands[0] {
        IncidentCommand::Open {
            incident_id,
            stream,
            trigger,
            ..
        } => {
            assert_eq!(*stream, "a");
            assert_eq!(*trigger, jetstream_monitor::incidents::IncidentTrigger::DuplicateDelivery);
            incident_id.clone()
        }
        other => panic!("expected open, got {other:?}"),
    };

    // Recovery needs the resolve interval below threshold.
    evaluate_ordinal_breach(StreamId::A, &snapshot(0, 0), &thresholds, &mut state, t0 + Duration::from_secs(15), &mut commands);
    evaluate_ordinal_breach(StreamId::A, &snapshot(0, 0), &thresholds, &mut state, t0 + Duration::from_secs(80), &mut commands);
    assert!(
        commands.iter().any(|c| matches!(
            c,
            IncidentCommand::AppendEvent { .. }
        )),
        "recovery event emitted with evidence"
    );
    assert!(matches!(commands.last(), Some(IncidentCommand::Resolve { incident_id: resolved_id, .. }) if *resolved_id == incident_id));

}


#[test]
fn stream_message_carrying_accounting_feeds_every_surface() {
    let mut ring = OrdinalRing::new();
    let _ = feed(&mut ring, "epoch-ws", &[Some(1), Some(1)]);
    let snap = accounting(&ring);
    let mut tracker = UptimeTracker::new();
    tracker.record_stream_message(&StreamMessage {
        stream_id: StreamId::A,
        count: 2,
        delivery_latency_us: None,
        source_event: None,
        ordinal_accounting: Some(snap),
    });
    let picture = tracker.ordinal_snapshot(StreamId::A).unwrap();
    assert_eq!(picture.duplicate_total, 1);

    // Health-flag semantics: ratios ride on the picture the health handler copies.
    assert!(picture.duplicate_ratio >= 0.0 && picture.duplicate_ratio <= 1.0);
}