use serde::ser::SerializeStruct;
use serde::{Deserialize, Serialize, Serializer};
use simd_json::OwnedValue;

/// Sentinel used by the vendored simd-json serializer to splice pre-serialized
/// JSON verbatim (see vendor/simd-json RAW_VALUE_TOKEN).
const RAW_TOKEN: &str = "$jetstream_turbo_raw";

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
    pub did: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub time_us: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub seq: Option<u64>,
    pub kind: MessageKind,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub commit: Option<CommitData>,
    /// The original wire JSON this message was parsed from, when available.
    /// Serializing the message then emits these bytes verbatim (identical JSON,
    /// no re-walk through serde). `None` for programmatically-built messages.
    #[serde(skip)]
    pub raw_json: Option<Box<str>>,
}

impl Serialize for JetstreamMessage {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        if let Some(raw) = &self.raw_json {
            return serializer.serialize_newtype_struct(RAW_TOKEN, raw.as_ref());
        }
        // Fallback for messages built without a wire form.
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
    pub record: Option<OwnedValue>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cid: Option<String>,
}

impl JetstreamMessage {
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
    fn serialize_emits_wire_json_verbatim_when_raw_available() {
        let raw = r#"{"did":"did:plc:test","time_us":1770949213790196,"seq":100000,"kind":"commit","commit":{"rev":"3mepgzgimkv23","operation":"create","collection":"app.bsky.feed.post","rkey":"3mepgzgiatv23","record":{"$type":"app.bsky.feed.post","createdAt":"2026-02-13T02:20:02.895Z","text":"Hello world"},"cid":"bafyreiassbuahzdwy64xwlefqcwh6zk4stb4lhht24oozhxn3fhzomrxg4"}}"#;
        let message: JetstreamMessage = serde_json::from_str(raw).unwrap();
        // Programmatically-built messages have no wire form: derived serialization.
        let out = simd_json::to_string(&message).unwrap();
        let v: serde_json::Value = serde_json::from_str(&out).unwrap();
        assert_eq!(v["did"], "did:plc:test");

        // With raw_json present, the exact wire bytes are emitted verbatim.
        let mut with_raw = message.clone();
        with_raw.raw_json = Some(Box::from(raw));
        let out2 = simd_json::to_string(&with_raw).unwrap();
        assert_eq!(out2, raw);
        // Re-parsing the spliced output yields the same message.
        let mut reparsed: JetstreamMessage = serde_json::from_str(&out2).unwrap();
        reparsed.raw_json = None;
        assert_eq!(reparsed.did, message.did);
        assert_eq!(
            reparsed.commit.as_ref().unwrap().rkey.as_deref(),
            Some("3mepgzgiatv23")
        );
    }

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
        assert_eq!(message.did, "did:plc:test");
        assert!(message.is_create_operation());
        assert_eq!(
            message.extract_at_uri(),
            Some("at://did:plc:test/app.bsky.feed.post/3mepgzgiatv23".to_string())
        );
    }
}
