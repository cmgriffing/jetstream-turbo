//! Bounded transition types shared by the stream client and transition processor.
//!
//! These types carry stable serde names; they are part of the dashboard contract
//! and must not change serialization without a compatibility review.

use serde::{Deserialize, Serialize};

use crate::incidents::{HandshakeFailureReason, TransportLossReason};

/// Orthogonal transport state for a configured stream.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TransportState {
    Connecting,
    Connected,
    Disconnected,
}

/// Orthogonal useful-delivery state for a configured stream.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DeliveryState {
    /// No useful record observed since startup or recovery.
    Unknown,
    /// Handshake succeeded; awaiting the first useful record.
    Waiting,
    /// Useful records are arriving.
    Delivering,
    /// A previously delivering stream crossed the delivery-idle deadline while
    /// the transport remained alive.
    Idle,
}

/// Ordered, low-rate state transition emitted by the stream client.
///
/// Transitions are emitted on the same ordered channel as record batches so
/// consumers observe exactly one ordering authority per stream.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum StreamTransition {
    /// WebSocket handshake completed successfully.
    HandshakeSucceeded { connect_time_ms: u64 },
    /// The first useful text record after a period without delivery arrived.
    DeliveryResumed,
    /// The delivery-idle deadline elapsed while the transport remained alive.
    DeliveryIdle { silence_ms: u64 },
    /// True transport loss: socket error, peer close, or missed liveness deadline.
    TransportLost {
        reason: TransportLossReason,
        /// Milliseconds since this outage's original socket failure, when part
        /// of an ongoing outage carried across reconnect attempts.
        outage_elapsed_ms: Option<u64>,
    },
    /// A reconnect or initial handshake attempt failed.
    ReconnectAttemptFailed {
        /// 1-based attempt ordinal within the current outage or startup.
        ordinal: u64,
        reason: HandshakeFailureReason,
        /// Bounded exponential backoff delay scheduled before the next attempt.
        scheduled_delay_ms: u64,
    },
}

/// One event on the single ordered per-stream channel.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(untagged)]
pub enum StreamEvent {
    Transition(StreamTransition),
    Record(super::StreamMessage),
}
