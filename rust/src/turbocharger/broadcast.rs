//! Monitor broadcast envelope: in-band ordinal accounting facts per record.
//!
//! The monitor-facing WebSocket wraps each `EnrichedRecord` in this envelope
//! instead of adding fields to the record itself, so the serialized shape of
//! records written to durable storage is unchanged (design D8). The record is
//! serialized with `#[serde(flatten)]` so its fields stay at the JSON top
//! level; the two accounting fields are additive, which keeps legacy monitor
//! consumers working (they ignore unknown fields).

use crate::models::enriched::EnrichedRecord;

/// Stable JSON field names for the ordinal accounting facts.
pub const TURBO_EPOCH_FIELD: &str = "turbo_epoch";
pub const INGRESS_ORDINAL_FIELD: &str = "ingress_ordinal";

/// A monitor broadcast record carrying its producer epoch and ingress ordinal.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct MonitorBroadcastEnvelope {
    #[serde(flatten)]
    pub record: EnrichedRecord,
    /// Identifier of the turbo process epoch that produced this record.
    pub turbo_epoch: String,
    /// Monotonic ingress ordinal, unique within the process epoch.
    pub ingress_ordinal: u64,
}

impl MonitorBroadcastEnvelope {
    pub fn new(record: EnrichedRecord, turbo_epoch: impl Into<String>, ingress_ordinal: u64) -> Self {
        Self {
            record,
            turbo_epoch: turbo_epoch.into(),
            ingress_ordinal,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::models::jetstream::JetstreamMessage;
    use chrono::Utc;

    fn record() -> EnrichedRecord {
        let message: JetstreamMessage = serde_json::from_value(serde_json::json!({
            "did": "did:plc:test",
            "time_us": 1_700_000_000_000_000u64,
            "kind": "commit"
        }))
        .expect("valid message");
        EnrichedRecord {
            message,
            hydrated_metadata: Default::default(),
            processed_at: Utc::now(),
            metrics: Default::default(),
        }
    }

    #[test]
    fn envelope_keeps_record_fields_at_top_level_and_adds_facts() {
        let json = serde_json::to_value(MonitorBroadcastEnvelope::new(
            record(),
            "1.0.0-1700000000",
            42,
        ))
        .expect("serialize");
        assert_eq!(json[TURBO_EPOCH_FIELD], "1.0.0-1700000000");
        assert_eq!(json[INGRESS_ORDINAL_FIELD], 42);
        // Record fields are flattened, so legacy consumers still find them.
        assert!(json["message"].is_object());
        assert!(json["processed_at"].is_string());
        assert!(json["metrics"].is_object());
        assert!(
            json.get("hydrated_metadata").is_some(),
            "record fields must remain present for legacy consumers"
        );
    }

    #[test]
    fn storage_serialization_of_records_is_unchanged() {
        // The envelope must not alter the record's own serialized shape.
        let record = record();
        let bare = serde_json::to_value(&record).expect("bare serialize");
        let mut expected = bare.as_object().expect("object").clone();
        expected.insert("turbo_epoch".to_string(), serde_json::json!("e1"));
        expected.insert("ingress_ordinal".to_string(), serde_json::json!(7));
        let enveloped =
            serde_json::to_value(MonitorBroadcastEnvelope::new(record, "e1", 7)).expect("env");
        assert_eq!(
            enveloped.as_object().expect("object"),
            &expected,
            "envelope must equal the record's durable serialization plus two fields"
        );
    }
}