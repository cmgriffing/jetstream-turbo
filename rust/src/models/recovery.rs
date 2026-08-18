use crate::models::jetstream::{JetstreamMessage, MessageKind, OperationType};
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use std::fmt;

/// The externally visible lifecycle of Jetstream recovery.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RecoveryPhase {
    Connecting,
    Replaying,
    CatchingUp,
    Live,
    UnrecoverableGap,
}

/// The classified cause of the most recent Jetstream reconnect.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ReconnectReason {
    ConnectTimeout,
    HandshakeFailure,
    SocketRead,
    PeerClose,
    DataIdleTimeout,
    ReplayRejected,
    ReplayClamped,
}

impl ReconnectReason {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::ConnectTimeout => "connect_timeout",
            Self::HandshakeFailure => "handshake_failure",
            Self::SocketRead => "socket_read",
            Self::PeerClose => "peer_close",
            Self::DataIdleTimeout => "data_idle_timeout",
            Self::ReplayRejected => "replay_rejected",
            Self::ReplayClamped => "replay_clamped",
        }
    }
}

impl fmt::Display for ReconnectReason {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(self.as_str())
    }
}

/// A source identity that remains stable across endpoint switches and replay.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct SourceEventId(String);

impl SourceEventId {
    /// Derives an identity only from portable source fields.
    pub fn from_message(message: &JetstreamMessage) -> Self {
        let mut identity = String::from("v1");
        push_component(&mut identity, &message.did);
        push_component(&mut identity, message_kind_name(message.kind));
        push_component(
            &mut identity,
            &message
                .time_us
                .map(|value| value.to_string())
                .unwrap_or_default(),
        );

        if let Some(commit) = &message.commit {
            push_component(&mut identity, commit.rev.as_deref().unwrap_or_default());
            push_component(&mut identity, operation_name(commit.operation_type));
            push_component(
                &mut identity,
                commit.collection.as_deref().unwrap_or_default(),
            );
            push_component(&mut identity, commit.rkey.as_deref().unwrap_or_default());
            push_component(&mut identity, commit.cid.as_deref().unwrap_or_default());
        }

        Self(identity)
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl From<String> for SourceEventId {
    fn from(value: String) -> Self {
        Self(value)
    }
}

impl fmt::Display for SourceEventId {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.0)
    }
}

/// Portable source position associated with an accepted ingress event.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SourceCursor {
    pub time_us: u64,
    pub source_seq: Option<u64>,
    pub source_event_id: SourceEventId,
}

impl SourceCursor {
    pub fn from_message(message: &JetstreamMessage) -> Option<Self> {
        Some(Self {
            time_us: message.time_us?,
            source_seq: message.seq,
            source_event_id: SourceEventId::from_message(message),
        })
    }
}

/// The highest contiguous source position durably completed by the pipeline.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct IngestionCheckpoint {
    pub ingress_ordinal: u64,
    pub cursor: SourceCursor,
    pub updated_at: DateTime<Utc>,
}

/// Inclusive ingress and cursor bounds carried by one pipeline batch.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct IngressRange {
    pub start_ordinal: u64,
    pub end_ordinal: u64,
    pub start_cursor: SourceCursor,
    pub end_cursor: SourceCursor,
}

/// One accepted source event paired with its process-local ordering metadata.
#[derive(Debug, Clone)]
pub struct IngressEvent {
    pub ordinal: u64,
    pub cursor: SourceCursor,
    pub message: JetstreamMessage,
}

impl IngressEvent {
    pub fn new(ordinal: u64, message: JetstreamMessage) -> Option<Self> {
        Some(Self {
            ordinal,
            cursor: SourceCursor::from_message(&message)?,
            message,
        })
    }
}

/// A contiguous set of accepted ingress events submitted to pipeline work.
#[derive(Debug)]
pub struct IngressBatch {
    events: Vec<IngressEvent>,
    range: IngressRange,
}

impl IngressBatch {
    pub fn new(events: Vec<IngressEvent>) -> Option<Self> {
        let first = events.first()?;
        let last = events.last()?;
        let range = IngressRange {
            start_ordinal: first.ordinal,
            end_ordinal: last.ordinal,
            start_cursor: first.cursor.clone(),
            end_cursor: last.cursor.clone(),
        };
        range.is_valid().then_some(Self { events, range })
    }

    pub fn range(&self) -> &IngressRange {
        &self.range
    }

    pub fn into_parts(self) -> (Vec<IngressEvent>, IngressRange) {
        (self.events, self.range)
    }
}

impl IngressRange {
    pub fn is_valid(&self) -> bool {
        self.start_ordinal <= self.end_ordinal
            && self.start_cursor.time_us <= self.end_cursor.time_us
    }

    pub fn follows(&self, ordinal: u64) -> bool {
        self.start_ordinal == ordinal.saturating_add(1)
    }
}

fn push_component(identity: &mut String, component: &str) {
    identity.push('|');
    identity.push_str(&component.len().to_string());
    identity.push(':');
    identity.push_str(component);
}

const fn message_kind_name(kind: MessageKind) -> &'static str {
    match kind {
        MessageKind::Commit => "commit",
        MessageKind::Identity => "identity",
        MessageKind::Account => "account",
        MessageKind::Unknown => "unknown",
    }
}

const fn operation_name(operation: OperationType) -> &'static str {
    match operation {
        OperationType::Create => "create",
        OperationType::Update => "update",
        OperationType::Delete => "delete",
        OperationType::Unknown => "unknown",
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::models::jetstream::CommitData;

    fn message(seq: u64) -> JetstreamMessage {
        JetstreamMessage {
            did: "did:plc:test".to_string(),
            time_us: Some(1_640_995_200_000_000),
            seq: Some(seq),
            kind: MessageKind::Commit,
            commit: Some(CommitData {
                rev: Some("rev-1".to_string()),
                operation_type: OperationType::Create,
                collection: Some("app.bsky.feed.post".to_string()),
                rkey: Some("post-1".to_string()),
                record: None,
                cid: Some("bafy-test".to_string()),
            }),
        }
    }

    #[test]
    fn source_event_id_ignores_endpoint_local_sequence() {
        assert_eq!(
            SourceEventId::from_message(&message(10)),
            SourceEventId::from_message(&message(99))
        );
    }

    #[test]
    fn source_event_id_distinguishes_commit_identity() {
        let original = message(10);
        let mut changed = original.clone();
        changed.commit.as_mut().unwrap().rkey = Some("post-2".to_string());

        assert_ne!(
            SourceEventId::from_message(&original),
            SourceEventId::from_message(&changed)
        );
    }

    #[test]
    fn source_cursor_requires_event_time() {
        let mut without_time = message(10);
        without_time.time_us = None;

        assert_eq!(SourceCursor::from_message(&without_time), None);
    }

    #[test]
    fn ingress_range_requires_ordered_ordinals_and_event_time() {
        let cursor = SourceCursor::from_message(&message(10)).unwrap();
        let range = IngressRange {
            start_ordinal: 2,
            end_ordinal: 1,
            start_cursor: cursor.clone(),
            end_cursor: cursor,
        };

        assert!(!range.is_valid());
    }

    #[test]
    fn reconnect_reason_uses_metric_label() {
        assert_eq!(
            ReconnectReason::DataIdleTimeout.as_str(),
            "data_idle_timeout"
        );
    }
}
