//! Sustained ordinal-accounting thresholds driving the incident ledger
//! (task 4.3, design D10). Evaluation is pure state plus emitted commands,
//! so the open/resolve wiring is independently testable without a live
//! process loop.

use super::{IncidentEvent, IncidentEventType, IncidentId, IncidentTrigger};
use crate::stats::ordinal::{OrdinalStreamSnapshot, OrdinalThresholds};
use crate::stream::processor::{stable_stream_id, IncidentCommand};
use crate::stream::StreamId;
use std::collections::HashMap;
use std::time::Instant;

/// Bounded evidence JSON carried by threshold events (task 4.3).
#[derive(serde::Serialize, Debug, Clone, PartialEq)]
pub struct OrdinalEvidence {
    pub turbo_epoch: String,
    pub ordinal_watermark: u64,
    pub duplicate_ratio: f64,
    pub gap_rate: f64,
    pub unique_total: u64,
    pub duplicate_total: u64,
    pub gap_total: u64,
}

const MAX_EVIDENCE_JSON_CHARS: usize = 512;

pub fn ordinal_evidence_json(snapshot: &OrdinalStreamSnapshot) -> String {
    let evidence = OrdinalEvidence {
        turbo_epoch: snapshot.turbo_epoch.clone(),
        ordinal_watermark: snapshot.ordinal_watermark,
        duplicate_ratio: snapshot.duplicate_ratio,
        gap_rate: snapshot.gap_rate,
        unique_total: snapshot.unique_total,
        duplicate_total: snapshot.duplicate_total,
        gap_total: snapshot.gap_total,
    };
    serde_json::to_string(&evidence)
        .unwrap_or_default()
        .chars()
        .take(MAX_EVIDENCE_JSON_CHARS)
        .collect()
}

/// Per-stream per-reason breach tracking.
#[derive(Default, Debug)]
pub struct OrdinalReasonState {
    breach: Option<Instant>,
    open_incident: Option<IncidentId>,
    below_since: Option<Instant>,
    event_sequence: i64,
}

/// Per-stream state for both threshold reasons.
#[derive(Default, Debug)]
pub struct OrdinalIncidentState {
    pub duplicate: HashMap<StreamId, OrdinalReasonState>,
    pub gap: HashMap<StreamId, OrdinalReasonState>,
}

/// One evaluation step of the sustained-breach open/resolve wiring. Emits
/// ordered ledger commands; the caller forwards them to the durable store.
#[allow(clippy::too_many_arguments)]
pub fn evaluate_ordinal_breach(
    stream_id: StreamId,
    snapshot: &OrdinalStreamSnapshot,
    thresholds: &OrdinalThresholds,
    state: &mut OrdinalIncidentState,
    now: Instant,
    commands: &mut Vec<IncidentCommand>,
) {
    evaluate_reason(
        stream_id,
        snapshot,
        snapshot.duplicate_ratio,
        thresholds.duplicate_ratio,
        IncidentTrigger::DuplicateDelivery,
        state.duplicate.entry(stream_id).or_default(),
        thresholds,
        now,
        commands,
    );
    evaluate_reason(
        stream_id,
        snapshot,
        snapshot.gap_rate,
        thresholds.gap_rate,
        IncidentTrigger::OrdinalGap,
        state.gap.entry(stream_id).or_default(),
        thresholds,
        now,
        commands,
    );
}

/// Open (after sustain) or resolve (after recovery interval) one
/// threshold-based incident reason for one stream.
#[allow(clippy::too_many_arguments)]
fn evaluate_reason(
    stream_id: StreamId,
    snapshot: &OrdinalStreamSnapshot,
    value: f64,
    threshold: f64,
    trigger: IncidentTrigger,
    reason: &mut OrdinalReasonState,
    thresholds: &OrdinalThresholds,
    now: Instant,
    commands: &mut Vec<IncidentCommand>,
) {
    let stream = stable_stream_id(stream_id);
    let now_wall = chrono::Utc::now();
    if value > threshold {
        reason.below_since = None;
        let breach_started = *reason.breach.get_or_insert(now);
        if reason.open_incident.is_none()
            && now.duration_since(breach_started) >= thresholds.sustain
        {
            let incident_id = IncidentId::generate();
            commands.push(IncidentCommand::Open {
                incident_id: incident_id.clone(),
                stream,
                trigger,
                detected_at: now_wall,
                last_useful_record_at: None,
                connection_epoch: 0,
            });
            reason.event_sequence = 1;
            commands.push(IncidentCommand::AppendEvent {
                incident_id: incident_id.clone(),
                event: IncidentEvent {
                    sequence: 1,
                    event_type: IncidentEventType::ThresholdBreached,
                    occurred_at: now_wall,
                    reason: Some(trigger.reason().to_string()),
                    attempt_ordinal: None,
                    scheduled_delay_ms: None,
                    evidence: Some(ordinal_evidence_json(snapshot)),
                },
            });
            reason.open_incident = Some(incident_id);
        }
    } else {
        reason.breach = None;
        let below_started = reason.below_since.get_or_insert(now);
        if let Some(incident_id) = reason.open_incident.clone() {
            if now.duration_since(*below_started) >= thresholds.resolve {
                reason.event_sequence += 1;
                commands.push(IncidentCommand::AppendEvent {
                    incident_id: incident_id.clone(),
                    event: IncidentEvent {
                        sequence: reason.event_sequence,
                        event_type: IncidentEventType::ThresholdRecovered,
                        occurred_at: now_wall,
                        reason: Some(trigger.reason().to_string()),
                        attempt_ordinal: None,
                        scheduled_delay_ms: None,
                        evidence: Some(ordinal_evidence_json(snapshot)),
                    },
                });
                commands.push(IncidentCommand::Resolve {
                    incident_id,
                    resolved_at: now_wall,
                });
                reason.open_incident = None;
                reason.below_since = None;
            }
        } else {
            reason.below_since = None;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    fn snapshot(duplicate_ratio: f64, gap_rate: f64) -> OrdinalStreamSnapshot {
        OrdinalStreamSnapshot {
            turbo_epoch: "epoch-x".to_string(),
            ordinal_watermark: 100,
            unique_total: 90,
            duplicate_total: 8,
            gap_total: 2,
            uninstrumented_total: 0,
            epoch_changes: 0,
            duplicate_ratio,
            gap_rate,
            status: "active".to_string(),
        }
    }

    fn thresholds() -> OrdinalThresholds {
        OrdinalThresholds {
            duplicate_ratio: 0.05,
            gap_rate: 0.005,
            sustain: Duration::from_secs(60),
            resolve: Duration::from_secs(300),
        }
    }

    fn evaluate_at(
        now: Instant,
        state: &mut OrdinalIncidentState,
        duplicate_ratio: f64,
        gap_rate: f64,
    ) -> Vec<IncidentCommand> {
        let snap = snapshot(duplicate_ratio, gap_rate);
        let mut commands = Vec::new();
        evaluate_ordinal_breach(StreamId::A, &snap, &thresholds(), state, now, &mut commands);
        commands
    }

    #[test]
    fn duplicate_breach_opens_incident_only_after_sustain() {
        let mut state = OrdinalIncidentState::default();
        let t0 = Instant::now();

        // Breach just started: nothing yet.
        assert!(evaluate_at(t0, &mut state, 0.5, 0.0).is_empty());
        // Sustain interval partially elapsed: still nothing.
        assert!(evaluate_at(t0 + Duration::from_secs(59), &mut state, 0.5, 0.0).is_empty());
        // Sustain reached: open incident plus breach evidence event.
        let commands = evaluate_at(t0 + Duration::from_secs(60), &mut state, 0.5, 0.0);
        assert_eq!(commands.len(), 2);
        assert!(matches!(commands[0], IncidentCommand::Open { .. }));
        match &commands[1] {
            IncidentCommand::AppendEvent { event, .. } => {
                assert_eq!(
                    event.event_type,
                    super::super::IncidentEventType::ThresholdBreached
                );
                assert_eq!(event.reason.as_deref(), Some("duplicate_delivery"));
                let evidence = event.evidence.as_deref().expect("evidence");
                assert!(evidence.contains("\"turbo_epoch\":\"epoch-x\""));
                assert!(evidence.contains("\"duplicate_ratio\":0.5"));
            }
            other => panic!("expected append event, got {other:?}"),
        }
    }

    #[test]
    fn gap_breach_uses_stable_ordinal_gap_reason() {
        let mut state = OrdinalIncidentState::default();
        let t0 = Instant::now();
        assert!(evaluate_at(t0, &mut state, 0.0, 0.9).is_empty());
        let commands = evaluate_at(t0 + Duration::from_secs(61), &mut state, 0.0, 0.9);
        assert_eq!(commands.len(), 2);
        assert!(matches!(
            &commands[0],
            IncidentCommand::Open { trigger: IncidentTrigger::OrdinalGap, .. }
        ));
        match &commands[1] {
            IncidentCommand::AppendEvent { event, .. } => {
                assert_eq!(event.reason.as_deref(), Some("ordinal_gap"));
            }
            other => panic!("expected append event, got {other:?}"),
        }
    }

    #[test]
    fn recovery_resolves_the_incident_after_the_resolve_interval() {
        let mut state = OrdinalIncidentState::default();
        let t0 = Instant::now();
        let _ = evaluate_at(t0, &mut state, 0.5, 0.0);
        let commands = evaluate_at(t0 + Duration::from_secs(60), &mut state, 0.5, 0.0);
        let incident_id = match &commands[0] {
            IncidentCommand::Open { incident_id, .. } => incident_id.clone(),
            other => panic!("expected open, got {other:?}"),
        };

        // Still breached: nothing new.
        assert!(evaluate_at(t0 + Duration::from_secs(120), &mut state, 0.5, 0.0).is_empty());
        // Falls below: recovery waits for the resolve interval.
        assert!(evaluate_at(t0 + Duration::from_secs(130), &mut state, 0.0, 0.0).is_empty());
        let commands = evaluate_at(t0 + Duration::from_secs(435), &mut state, 0.0, 0.0);
        assert!(commands.len() >= 2);
        match &commands[0] {
            IncidentCommand::AppendEvent { incident_id: event_incident_id, event } => {
                assert_eq!(event_incident_id, &incident_id);
                assert_eq!(
                    event.event_type,
                    super::super::IncidentEventType::ThresholdRecovered
                );
            }
            other => panic!("expected recovered event, got {other:?}"),
        }
        match &commands[1] {
            IncidentCommand::Resolve {
                incident_id: resolved_id,
                ..
            } => assert_eq!(*resolved_id, incident_id),
            other => panic!("expected resolve, got {other:?}"),
        }
        // Subsequent healthy evaluations emit nothing.
        assert!(evaluate_at(t0 + Duration::from_secs(500), &mut state, 0.0, 0.0).is_empty());
    }

    #[test]
    fn separate_streams_track_breaches_independently() {
        let mut state = OrdinalIncidentState::default();
        let t0 = Instant::now();
        let snap_a = snapshot(0.5, 0.0);
        let snap_b = snapshot(0.0, 0.0);
        let mut commands = Vec::new();
        evaluate_ordinal_breach(StreamId::A, &snap_a, &thresholds(), &mut state, t0, &mut commands);
        assert!(commands.is_empty());
        // Stream B does not inherit A's breach clock.
        evaluate_ordinal_breach(StreamId::B, &snap_b, &thresholds(), &mut state, t0, &mut commands);
        evaluate_ordinal_breach(StreamId::A, &snap_a, &thresholds(), &mut state, t0 + Duration::from_secs(60), &mut commands);
        assert_eq!(commands.len(), 2);
        match &commands[0] {
            IncidentCommand::Open { stream, .. } => assert_eq!(*stream, "a"),
            other => panic!("unexpected command {other:?}"),
        }
    }
}