use crate::models::record_data::RecordData;
use serde::{Deserialize, Serialize, Serializer};
use std::sync::Arc;

/// Convert a `serde_json::Value` record into the semantic record view used by
/// `CommitData::record`. Kept as a helper so callers constructing messages
/// (fixtures, tests) do not need to depend on record extraction directly.
pub fn owned_record(value: serde_json::Value) -> RecordData {
    RecordData::from_value(value)
}

#[repr(u8)]
#[derive(Debug, Copy, Clone, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum MessageKind {
    Commit,
    Identity,
    Account,
    #[serde(other)]
    Unknown,
}

impl Serialize for MessageKind {
    #[inline(always)]
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.serialize_str(match self {
            MessageKind::Commit => "commit",
            MessageKind::Identity => "identity",
            MessageKind::Account => "account",
            MessageKind::Unknown => "unknown",
        })
    }
}

impl MessageKind {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Commit => "commit",
            Self::Identity => "identity",
            Self::Account => "account",
            Self::Unknown => "unknown",
        }
    }
}

#[repr(u8)]
#[derive(Debug, Copy, Clone, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum OperationType {
    Create,
    Update,
    Delete,
    #[serde(other)]
    Unknown,
}

impl Serialize for OperationType {
    #[inline(always)]
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.serialize_str(match self {
            OperationType::Create => "create",
            OperationType::Update => "update",
            OperationType::Delete => "delete",
            OperationType::Unknown => "unknown",
        })
    }
}

#[derive(Debug, Clone, Deserialize)]
pub struct JetstreamMessage {
    pub did: Arc<str>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub time_us: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub seq: Option<u64>,
    pub kind: MessageKind,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub commit: Option<CommitData>,
    /// The original wire bytes this message was parsed from (when parsed from
    /// an owned buffer). Stored verbatim so the storage path can write the
    /// exact bytes received instead of re-encoding the parsed structure.
    #[serde(skip)]
    pub raw_json: Option<String>,
}

// Manual Serialize: mirrors the derived field-wise output exactly (raw_json is
// not part of the canonical JSON — it is only emitted via `write_json`).
impl Serialize for JetstreamMessage {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        use serde::ser::SerializeStruct;
        let mut state = serializer.serialize_struct("JetstreamMessage", 5)?;
        state.serialize_field("did", &self.did)?;
        if self.time_us.is_some() {
            state.serialize_field("time_us", &self.time_us)?;
        }
        if self.seq.is_some() {
            state.serialize_field("seq", &self.seq)?;
        }
        state.serialize_field("kind", &self.kind)?;
        if self.commit.is_some() {
            state.serialize_field("commit", &self.commit)?;
        }
        state.end()
    }
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct CommitData {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub rev: Option<String>,
    #[serde(rename = "operation")]
    pub operation_type: OperationType,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub collection: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub rkey: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub record: Option<RecordData>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cid: Option<String>,
}

impl JetstreamMessage {
    /// Write this message's JSON to `out`. When the message was parsed from
    /// owned wire bytes (`raw_json` present), the original bytes are written
    /// verbatim — byte-faithful to the Jetstream event and avoids re-encoding
    /// the parsed structure (envelope fields + record DOM) at store time.
    /// Otherwise falls back to canonical field-wise serialization.
    pub fn write_json(&self, out: &mut Vec<u8>) {
        if let Some(raw) = &self.raw_json {
            out.extend_from_slice(raw.as_bytes());
        } else {
            simd_json::to_writer(out, self).expect("serialize message to JSON");
        }
    }

    #[inline(always)]
    pub fn extract_at_uri(&self) -> Option<String> {
        let commit = self.commit.as_ref()?;
        let collection = commit.collection.as_ref()?;
        let rkey = commit.rkey.as_ref()?;

        let mut uri = String::with_capacity(7 + self.did.len() + collection.len() + rkey.len());
        uri.push_str("at://");
        uri.push_str(&self.did);
        uri.push('/');
        uri.push_str(collection);
        uri.push('/');
        uri.push_str(rkey);
        Some(uri)
    }

    #[inline(always)]
    pub fn extract_did(&self) -> &str {
        &self.did
    }

    pub fn is_create_operation(&self) -> bool {
        if let Some(commit) = &self.commit {
            return commit.operation_type == OperationType::Create;
        }
        false
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_jetstream_message_parsing() {
        let json_str = r#"
        {
            "did": "did:plc:test",
            "time_us": 1770949213790196,
            "kind": "commit",
            "commit": {
                "rev": "3mepgzgimkv23",
                "operation": "create",
                "collection": "app.bsky.feed.post",
                "rkey": "3mepgzgiatv23",
                "record": {
                    "$type": "app.bsky.feed.post",
                    "createdAt": "2026-02-13T02:20:02.89585500Z",
                    "text": "Hello world"
                },
                "cid": "bafyreiassbuahzdwy64xwlefqcwh6zk4stb4lhht24oozhxn3fhzomrxg4"
            }
        }
        "#;

        let message: JetstreamMessage = serde_json::from_str(json_str).unwrap();
        assert_eq!(message.did.as_ref(), "did:plc:test");
        assert!(message.is_create_operation());
        assert_eq!(
            message.extract_at_uri(),
            Some("at://did:plc:test/app.bsky.feed.post/3mepgzgiatv23".to_string())
        );
    }
}
