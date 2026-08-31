//! Single ordered per-stream transition processor.
//!
//! One processor per configured stream consumes the ordered `StreamEvent`
//! sequence in stream order and derives transport/delivery state, outage
//! accounting, incident ledger commands, and legacy dashboard compatibility
//! effects from it.

use chrono::{DateTime, Duration, Utc};
use tracing::{info, warn};
use std::time::Instant;

use super::transition::{DeliveryState, StreamEvent, StreamTransition, TransportState};
use super::{ConnectionStatus, ReconnectReason, StreamId, StreamMessage};
use crate::incidents::{
    HandshakeFailureReason, IncidentEvent, IncidentEventType, IncidentId, IncidentState,
    IncidentTrigger, TransportLossReason,
};

/// Stable stream identifiers used in incident records and metric labels.
pub fn stable_stream_id(stream_id: StreamId) -> &'static str {
    match stream_id {
        StreamId::A => "a",
        StreamId::B => "b",
        StreamId::Baseline1 => "baseline1",
        StreamId::Baseline2 => "baseline2",
    }
}

/// Commands forwarded from the transition processor to the durable ledger.
#[derive(Debug, Clone)]
pub enum IncidentCommand {
    Open {
        incident_id: IncidentId,
        stream: &'static str,
        trigger: IncidentTrigger,
        detected_at: DateTime<Utc>,
        last_useful_record_at: Option<DateTime<Utc>>,
        connection_epoch: u64,
    },
    AppendEvent {
        incident_id: IncidentId,
        event: IncidentEvent,
    },
    TransportRecovered {
        incident_id: IncidentId,
        recovered_at: DateTime<Utc>,
    },
    Resolve {
        incident_id: IncidentId,
        resolved_at: DateTime<Utc>,
    },
    /// Startup reconciliation: inherit any open incident from a previous process.
    ReconcileOpen {
        process_epoch: String,
        release: String,
        now: DateTime<Utc>,
    },
}

impl IncidentCommand {
    /// Bounded command name for sanitized structured failure logs.
    pub fn name(&self) -> &'static str {
        match self {
            IncidentCommand::Open { .. } => "open",
            IncidentCommand::AppendEvent { .. } => "append_event",
            IncidentCommand::TransportRecovered { .. } => "transport_recovered",
            IncidentCommand::Resolve { .. } => "resolve",
            IncidentCommand::ReconcileOpen { .. } => "reconcile_open",
        }
    }

    pub fn incident_id(&self) -> &IncidentId {
        match self {
            IncidentCommand::Open { incident_id, .. }
            | IncidentCommand::AppendEvent { incident_id, .. }
            | IncidentCommand::TransportRecovered { incident_id, .. }
            | IncidentCommand::Resolve { incident_id, .. } => incident_id,
            IncidentCommand::ReconcileOpen { .. } => {
                unreachable!("reconciliation has no incident id")
            }
        }
    }
}

/// Effects derived from ordered transitions for downstream consumers.
#[derive(Debug)]
pub enum Effect {
    /// Feed the legacy dashboard status path (uptime tracker).
    ConnectionStatus(ConnectionStatus),
    /// Feed the record accounting path (counts, latency, event time).
    Record(StreamMessage),
    /// Emit a ledger command to the durable incident store.
    Incident(IncidentCommand),
    /// A transport outage episode began (connected-to-disconnected).
    OutageStarted,
    /// A reconnect or handshake attempt failed.
    AttemptFailed { ordinal: u64 },
    /// A delivery-idle episode was detected on a live socket.
    IdleEpisode { silence_ms: u64 },
}

/// Per-stream state machine driven by ordered transitions.
pub struct TransitionProcessor {
    stream_id: StreamId,
    transport: TransportState,
    delivery: DeliveryState,
    connection_epoch: u64,
    /// Boundary of the current outage, if any.
    outage_started: Option<Instant>,
    /// Failed reconnect attempts within the current outage.
    outage_attempts: u64,
    /// Incident open for a continuous delivery disruption, if any.
    active_incident: Option<(IncidentId, IncidentTrigger)>,
    /// Monotonic per-incident event sequence assigned by the processor.
    next_event_sequence: i64,
}

impl TransitionProcessor {
    /// Create a processor for one stream; observation starts unconnected.
    pub fn new(stream_id: StreamId) -> Self {
        Self {
            stream_id,
            transport: TransportState::Disconnected,
            delivery: DeliveryState::Unknown,
            connection_epoch: 0,
            outage_started: None,
            outage_attempts: 0,
            active_incident: None,
            next_event_sequence: 1,
        }
    }

    /// Process one ordered event and return derived effects in order.
    pub fn process(&mut self, event: StreamEvent, wall_now: DateTime<Utc>) -> Vec<Effect> {
        let mut effects = Vec::new();
        match event {
            StreamEvent::Record(record) => {
                effects.push(Effect::Record(record));
            }
            StreamEvent::Transition(transition) => {
                effects =
                    self.apply_transition(transition, wall_now);
            }
        }
        effects
    }

    fn apply_transition(
        &mut self,
        transition: StreamTransition,
        wall_now: DateTime<Utc>,
    ) -> Vec<Effect> {
        let mut effects = Vec::new();
        match transition {
            StreamTransition::HandshakeSucceeded { connect_time_ms } => {
                self.transport = TransportState::Connected;
                info!(
                    target: "monitor::transition",
                    stream = stable_stream_id(self.stream_id),
                    connect_time_ms,
                    connection_epoch = self.connection_epoch,
                    "transport recovered"
                );
                self.connection_epoch = self.connection_epoch.saturating_add(1);
                if self.delivery != DeliveryState::Delivering {
                    self.delivery = DeliveryState::Waiting;
                }
                if self.outage_started.take().is_some() {
                    // Transport recovery within an open outage counts the
                    // successful reconnect as the final attempt, then resets.
                    self.outage_attempts = self.outage_attempts.saturating_add(1);
                }
                if let Some((incident_id, _)) = self.active_incident.clone() {
                    effects.push(Effect::Incident(IncidentCommand::AppendEvent {
                        incident_id: incident_id.clone(),
                        event: IncidentEvent {
                            sequence: self.next_event_sequence,
                            event_type: IncidentEventType::TransportRecovered,
                            occurred_at: wall_now,
                            reason: None,
                            attempt_ordinal: Some(self.outage_attempts),
                            scheduled_delay_ms: None,
                        },
                    }));
                    self.next_event_sequence = self.next_event_sequence.saturating_add(1);
                    effects.push(Effect::Incident(IncidentCommand::TransportRecovered {
                        incident_id,
                        recovered_at: wall_now,
                    }));
                }
                // Attempt accounting resets after successful transport recovery.
                self.outage_attempts = 0;
                effects.push(Effect::ConnectionStatus(ConnectionStatus {
                    stream_id: self.stream_id,
                    connected: true,
                    connected_at: Some(Instant::now()),
                    connect_time_ms: Some(connect_time_ms),
                    delivery_available: self.delivery == DeliveryState::Delivering,
                    reconnect_reason: None,
                    client_recovery: false,
                }));
            }
            StreamTransition::DeliveryResumed => {
                self.delivery = DeliveryState::Delivering;
                info!(
                    target: "monitor::transition",
                    stream = stable_stream_id(self.stream_id),
                    "delivery recovered"
                );
                if let Some((incident_id, _)) = self.active_incident.take() {
                    effects.push(Effect::Incident(IncidentCommand::Resolve {
                        incident_id,
                        resolved_at: wall_now,
                    }));
                }
            }
            StreamTransition::DeliveryIdle { silence_ms } => {
                self.delivery = DeliveryState::Idle;
                warn!(
                    target: "monitor::transition",
                    stream = stable_stream_id(self.stream_id),
                    silence_ms,
                    "delivery idle detected on responsive socket"
                );
                effects.push(Effect::IdleEpisode { silence_ms });
                let last_useful_record_at = Some(wall_now - Duration::milliseconds(silence_ms as i64));
                self.open_incident(
                    IncidentTrigger::DeliveryIdle,
                    last_useful_record_at,
                    wall_now,
                    &mut effects,
                );
            }
            StreamTransition::TransportLost {
                reason,
                outage_elapsed_ms,
            } => {
                self.transport = TransportState::Disconnected;
                warn!(
                    target: "monitor::transition",
                    stream = stable_stream_id(self.stream_id),
                    reason = reason.as_str(),
                    "transport lost"
                );
                let new_outage = self.outage_started.is_none();
                if new_outage {
                    self.outage_started = Some(Instant::now());
                    self.outage_attempts = 0;
                    effects.push(Effect::OutageStarted);
                }
                effects.push(Effect::ConnectionStatus(ConnectionStatus {
                    stream_id: self.stream_id,
                    connected: false,
                    connected_at: None,
                    connect_time_ms: None,
                    delivery_available: false,
                    reconnect_reason: Some(legacy_reason(reason)),
                    client_recovery: false,
                }));
                if self.delivery == DeliveryState::Delivering {
                    self.delivery = DeliveryState::Unknown;
                }
                match self.active_incident.clone() {
                    // Idle evolved into transport loss: one incident continues.
                    Some((incident_id, _)) => {
                        effects.push(Effect::Incident(IncidentCommand::AppendEvent {
                            incident_id,
                            event: IncidentEvent {
                                sequence: self.next_event_sequence,
                                event_type: IncidentEventType::TransportLost,
                                occurred_at: wall_now,
                                reason: Some(reason.as_str().to_string()),
                                attempt_ordinal: None,
                                scheduled_delay_ms: None,
                            },
                        }));
                        self.next_event_sequence = self.next_event_sequence.saturating_add(1);
                    }
                    None => {
                        let last_useful_record_at: Option<DateTime<Utc>> = None;
                        self.open_incident(
                            IncidentTrigger::TransportLoss,
                            last_useful_record_at,
                            wall_now,
                            &mut effects,
                        );
                        // Record the loss reason on the newly opened incident.
                        if let Some((incident_id, _)) = self.active_incident.clone() {
                            effects.push(Effect::Incident(IncidentCommand::AppendEvent {
                                incident_id,
                                event: IncidentEvent {
                                    sequence: self.next_event_sequence,
                                    event_type: IncidentEventType::TransportLost,
                                    occurred_at: wall_now,
                                    reason: Some(reason.as_str().to_string()),
                                    attempt_ordinal: None,
                                    scheduled_delay_ms: None,
                                },
                            }));
                            self.next_event_sequence = self.next_event_sequence.saturating_add(1);
                        }
                    }
                }
                let _ = outage_elapsed_ms;
            }
            StreamTransition::ReconnectAttemptFailed {
                ordinal,
                reason,
                scheduled_delay_ms,
            } => {
                self.outage_attempts = self.outage_attempts.saturating_add(1);
                info!(
                    target: "monitor::transition",
                    stream = stable_stream_id(self.stream_id),
                    attempt_ordinal = ordinal,
                    reason = reason.as_str(),
                    scheduled_delay_ms,
                    "reconnect attempt failed"
                );
                effects.push(Effect::AttemptFailed { ordinal });
                if let Some((incident_id, _)) = self.active_incident.clone() {
                    effects.push(Effect::Incident(IncidentCommand::AppendEvent {
                        incident_id,
                        event: IncidentEvent {
                            sequence: self.next_event_sequence,
                            event_type: IncidentEventType::ReconnectAttemptFailed,
                            occurred_at: wall_now,
                            reason: Some(reason.as_str().to_string()),
                            attempt_ordinal: Some(ordinal),
                            scheduled_delay_ms: Some(scheduled_delay_ms),
                        },
                    }));
                    self.next_event_sequence = self.next_event_sequence.saturating_add(1);
                }
            }
        }
        effects
    }

    fn open_incident(
        &mut self,
        trigger: IncidentTrigger,
        last_useful_record_at: Option<DateTime<Utc>>,
        detected_at: DateTime<Utc>,
        effects: &mut Vec<Effect>,
    ) {
        if self.active_incident.is_some() {
            return;
        }
        let incident_id = IncidentId::generate();
        self.active_incident = Some((incident_id.clone(), trigger));
        self.next_event_sequence = 1;
        let _ = last_useful_record_at;
        let _ = detected_at;
        effects.push(Effect::Incident(IncidentCommand::Open {
            incident_id,
            stream: stable_stream_id(self.stream_id),
            trigger,
            detected_at,
            last_useful_record_at,
            connection_epoch: self.connection_epoch,
        }));
    }

    pub fn stream_id(&self) -> StreamId {
        self.stream_id
    }

    pub fn transport_state(&self) -> TransportState {
        self.transport
    }

    pub fn delivery_state(&self) -> DeliveryState {
        self.delivery
    }

    pub fn connection_epoch(&self) -> u64 {
        self.connection_epoch
    }

    pub fn outage_attempts(&self) -> u64 {
        self.outage_attempts
    }
}

fn legacy_reason(reason: TransportLossReason) -> ReconnectReason {
    match reason {
        TransportLossReason::SocketError => ReconnectReason::SocketRead,
        TransportLossReason::SocketWrite => ReconnectReason::SocketWrite,
        TransportLossReason::PeerClose => ReconnectReason::PeerClose,
        TransportLossReason::LivenessDeadline => ReconnectReason::DataIdleTimeout,
    }
}

// Keep the handshake-failure and incident-state references used above visible
// for the compiler; remove in a later cleanup pass if no longer required.
#[allow(dead_code)]
fn _unused_references(_h: HandshakeFailureReason, _s: IncidentState) {}
#[cfg(test)]
mod tests {
    use super::*;
    use crate::incidents::HandshakeFailureReason;

    fn wall(offset_ms: i64) -> DateTime<Utc> {
        Utc::now() + Duration::milliseconds(offset_ms)
    }

    fn processor() -> TransitionProcessor {
        TransitionProcessor::new(StreamId::A)
    }

    fn failed_attempt(ordinal: u64) -> StreamEvent {
        StreamEvent::Transition(StreamTransition::ReconnectAttemptFailed {
            ordinal,
            reason: HandshakeFailureReason::ConnectError,
            scheduled_delay_ms: 1_000,
        })
    }

    fn run(proc: &mut TransitionProcessor, events: Vec<StreamEvent>, t: i64) -> Vec<Effect> {
        let mut all = Vec::new();
        for (i, event) in events.into_iter().enumerate() {
            all.extend(proc.process(event, wall(t + i as i64)));
        }
        all
    }

    #[test]
    fn startup_starts_disconnected_with_unknown_delivery() {
        let proc = processor();
        assert_eq!(proc.transport_state(), TransportState::Disconnected);
        assert_eq!(proc.delivery_state(), DeliveryState::Unknown);
        assert_eq!(proc.connection_epoch(), 0);
        assert_eq!(proc.outage_attempts(), 0);
    }

    #[test]
    fn failed_startup_attempts_do_not_open_an_outage_or_incident() {
        let mut proc = processor();
        let effects = run(&mut proc, vec![failed_attempt(1), failed_attempt(2)], 0);

        assert_eq!(
            effects
                .iter()
                .filter(|e| matches!(e, Effect::AttemptFailed { .. }))
                .count(),
            2
        );
        assert!(effects.iter().all(|e| !matches!(e, Effect::OutageStarted)));
        assert!(effects.iter().all(|e| !matches!(e, Effect::Incident(_))));
        assert_eq!(proc.outage_attempts(), 2);
        assert_eq!(proc.transport_state(), TransportState::Disconnected);
        assert_eq!(proc.delivery_state(), DeliveryState::Unknown);
    }

    #[test]
    fn handshake_then_first_record_is_ordered_without_spurious_incident() {
        let mut proc = processor();
        let effects = run(
            &mut proc,
            vec![
                StreamEvent::Transition(StreamTransition::HandshakeSucceeded { connect_time_ms: 12 }),
                StreamEvent::Transition(StreamTransition::DeliveryResumed),
                StreamEvent::Record(StreamMessage {
                    stream_id: StreamId::A,
                    count: 1,
                    delivery_latency_us: Some(10),
                    source_event: None,
                }),
            ],
            0,
        );

        // exactly one legacy connected status, no incident commands at all
        let statuses: Vec<_> = effects
            .iter()
            .filter_map(|e| match e {
                Effect::ConnectionStatus(status) => Some(status.connected),
                _ => None,
            })
            .collect();
        assert_eq!(statuses, vec![true]);
        assert!(effects.iter().all(|e| !matches!(e, Effect::Incident(_))));
        assert_eq!(proc.transport_state(), TransportState::Connected);
        assert_eq!(proc.delivery_state(), DeliveryState::Delivering);
        assert_eq!(proc.connection_epoch(), 1);
    }

    #[test]
    fn idle_then_transport_loss_keeps_one_incident_across_events() {
        let mut proc = processor();
        let mut effects = run(
            &mut proc,
            vec![
                StreamEvent::Transition(StreamTransition::HandshakeSucceeded { connect_time_ms: 5 }),
                StreamEvent::Transition(StreamTransition::DeliveryResumed),
                StreamEvent::Transition(StreamTransition::DeliveryIdle { silence_ms: 30_000 }),
            ],
            0,
        );
        // Idle event opens exactly one incident.
        let opened: Vec<&IncidentCommand> = effects
            .iter()
            .filter_map(|e| match e {
                Effect::Incident(cmd @ IncidentCommand::Open { .. }) => Some(cmd),
                _ => None,
            })
            .collect();
        assert_eq!(opened.len(), 1);

        // Idle socket then loses transport: same incident continues.
        effects = run(
            &mut proc,
            vec![
                StreamEvent::Transition(StreamTransition::TransportLost {
                    reason: crate::incidents::TransportLossReason::PeerClose,
                    outage_elapsed_ms: None,
                }),
                StreamEvent::Transition(StreamTransition::ReconnectAttemptFailed {
                    ordinal: 1,
                    reason: HandshakeFailureReason::ConnectTimeout,
                    scheduled_delay_ms: 1_000,
                }),
            ],
            10,
        );
        let incident_ids: Vec<&IncidentId> = effects
            .iter()
            .filter_map(|e| match e {
                Effect::Incident(cmd) => Some(cmd.incident_id()),
                _ => None,
            })
            .collect();
        assert!(!incident_ids.is_empty());
        let first = incident_ids.first().copied();
        assert!(incident_ids.iter().all(|id| Some(*id) == first));
        // Exactly one outage episode was reported for the whole disruption.
        assert_eq!(
            effects
                .iter()
                .filter(|e| matches!(e, Effect::OutageStarted))
                .count(),
            1
        );
    }

    #[test]
    fn record_after_idle_race_resolves_incident_exactly_once_and_restarts_cleanly() {
        let mut proc = processor();
        run(
            &mut proc,
            vec![
                StreamEvent::Transition(StreamTransition::HandshakeSucceeded { connect_time_ms: 5 }),
                StreamEvent::Transition(StreamTransition::DeliveryResumed),
                StreamEvent::Transition(StreamTransition::DeliveryIdle { silence_ms: 30_000 }),
            ],
            0,
        );
        // Useful record and idle detection processed in ordered succession:
        // one deterministic final delivery state, at most one incident boundary.
        let effects = run(
            &mut proc,
            vec![
                StreamEvent::Transition(StreamTransition::DeliveryResumed),
                StreamEvent::Record(StreamMessage {
                    stream_id: StreamId::A,
                    count: 10,
                    delivery_latency_us: None,
                    source_event: None,
                }),
                StreamEvent::Transition(StreamTransition::DeliveryIdle { silence_ms: 31_000 }),
                StreamEvent::Transition(StreamTransition::DeliveryIdle { silence_ms: 31_000 }),
            ],
            100,
        );
        let resolves = effects
            .iter()
            .filter(|e| matches!(e, Effect::Incident(IncidentCommand::Resolve { .. })))
            .count();
        assert_eq!(resolves, 1, "idle race should resolve the first incident once");
        let opens = effects
            .iter()
            .filter(|e| matches!(e, Effect::Incident(IncidentCommand::Open { .. })))
            .count();
        assert_eq!(
            opens,
            1,
            "duplicate idle transitions must not open a second incident for the same window"
        );
        assert_eq!(proc.delivery_state(), DeliveryState::Idle);
        assert_eq!(proc.transport_state(), TransportState::Connected);
    }

    #[test]
    fn repeated_failed_handshakes_belong_to_one_outage_and_recover_from_original_boundary() {
        let mut proc = processor();
        // One connected socket fails.
        run(
            &mut proc,
            vec![
                StreamEvent::Transition(StreamTransition::HandshakeSucceeded { connect_time_ms: 1 }),
                StreamEvent::Transition(StreamTransition::DeliveryResumed),
            ],
            0,
        );
        let effects = run(
            &mut proc,
            vec![
                StreamEvent::Transition(StreamTransition::TransportLost {
                    reason: crate::incidents::TransportLossReason::SocketError,
                    outage_elapsed_ms: None,
                }),
                failed_attempt(1),
                failed_attempt(2),
                failed_attempt(3),
                StreamEvent::Transition(StreamTransition::HandshakeSucceeded { connect_time_ms: 8 }),
            ],
            10,
        );

        // One outage episode across the whole disruption.
        assert_eq!(
            effects
                .iter()
                .filter(|e| matches!(e, Effect::OutageStarted))
                .count(),
            1
        );
        // The recovering connect is counted as the final attempt in the
        // transport-recovered event, then attempt accounting resets.
        let recovered = effects
            .iter()
            .find_map(|e| match e {
                Effect::Incident(IncidentCommand::AppendEvent { event, .. }) => {
                    matches!(event.event_type, IncidentEventType::TransportRecovered)
                        .then(|| event.attempt_ordinal)
                        .flatten()
                }
                _ => None,
            });
        assert_eq!(recovered, Some(4));
        assert_eq!(proc.outage_attempts(), 0);
        assert_eq!(proc.transport_state(), TransportState::Connected);
    }

    #[test]
    fn backoff_resets_after_recovered_connection() {
        let mut proc = processor();
        run(
            &mut proc,
            vec![
                StreamEvent::Transition(StreamTransition::HandshakeSucceeded { connect_time_ms: 1 }),
                StreamEvent::Transition(StreamTransition::TransportLost {
                    reason: crate::incidents::TransportLossReason::LivenessDeadline,
                    outage_elapsed_ms: None,
                }),
                failed_attempt(1),
                failed_attempt(2),
                StreamEvent::Transition(StreamTransition::HandshakeSucceeded { connect_time_ms: 3 }),
            ],
            0,
        );
        assert_eq!(proc.outage_attempts(), 0, "attempts reset on transport recovery");

        let effects = run(
            &mut proc,
            vec![StreamEvent::Transition(StreamTransition::TransportLost {
                reason: crate::incidents::TransportLossReason::SocketError,
                outage_elapsed_ms: None,
            })],
            50,
        );
        assert!(effects.iter().any(|e| matches!(e, Effect::OutageStarted)));
    }
}
