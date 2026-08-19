use compact_str::CompactString;
use serde::ser::SerializeStruct;
use serde::{Deserialize, Serialize, Serializer};
use simd_json::OwnedValue;
use std::sync::{Arc, OnceLock};

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
    pub did: CompactString,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub time_us: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub seq: Option<u64>,
    pub kind: MessageKind,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub commit: Option<Box<CommitData>>,
    /// The original wire JSON this message was parsed from, when available.
    /// Serializing the message then emits these bytes verbatim (identical JSON,
    /// no re-walk through serde). `None` for programmatically-built messages.
    #[serde(skip)]
    pub raw_json: Option<Arc<str>>,
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

/// A post record whose JSON tree is built lazily.
///
/// The raw wire JSON is kept verbatim (cheap to store and scan); the
/// `OwnedValue` tree is parsed on first actual read. Records without
/// reply/embed/facets never pay the tree-build cost.
///
/// For parsed messages the raw bytes are shared with the message's `raw_json`
/// (one backing allocation, no duplicate copy of the record sub-range);
/// `Clone` is then a refcount bump instead of a deep copy.
#[derive(Debug)]
pub struct RecordValue {
    backing: Arc<str>,
    /// Byte span of this record within `backing`.
    start: usize,
    end: usize,
    value: OnceLock<OwnedValue>,
}

impl RecordValue {
    /// Build from a raw JSON string captured from the wire (no tree build).
    pub fn from_raw(raw: Box<str>) -> Self {
        let backing: Arc<str> = Arc::from(raw);
        let end = backing.len();
        Self {
            backing,
            start: 0,
            end,
            value: OnceLock::new(),
        }
    }

    /// Build from a byte span into a shared backing buffer (the message's wire
    /// JSON), avoiding a duplicate copy of the record's bytes.
    #[inline(always)]
    pub fn from_span(backing: Arc<str>, start: usize, end: usize) -> Self {
        Self {
            backing,
            start,
            end,
            value: OnceLock::new(),
        }
    }

    /// Build from an already-materialized value (fixtures/tests).
    pub fn from_value(value: OwnedValue) -> Self {
        let raw = simd_json::to_string(&value).unwrap_or_default();
        let backing: Arc<str> = Arc::from(raw);
        let end = backing.len();
        Self {
            backing,
            start: 0,
            end,
            value: OnceLock::from(value),
        }
    }

    /// The raw JSON bytes of this record.
    #[inline(always)]
    pub fn raw(&self) -> &str {
        &self.backing[self.start..self.end]
    }

    /// The materialized JSON tree, parsed on first access.
    pub fn value(&self) -> &OwnedValue {
        self.value.get_or_init(|| {
            // The raw came from a parsed message or a fixture, so it is valid JSON.
            let mut owned = self.raw().to_string();
            // SAFETY: the raw came from a parsed message or a fixture, so it is
            // valid JSON; the parse rewrites it in place and we own the buffer.
            unsafe { simd_json::from_str(&mut owned).expect("record raw must be valid JSON") }
        })
    }
}

impl Clone for RecordValue {
    fn clone(&self) -> Self {
        Self {
            backing: Arc::clone(&self.backing),
            start: self.start,
            end: self.end,
            value: OnceLock::new(),
        }
    }
}

impl Serialize for RecordValue {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        self.value().serialize(serializer)
    }
}

/// Find the byte span of the first object-valued `"record"` key in a Jetstream
/// wire message (the commit's record). String-aware: keys inside string values
/// (including escaped quotes) do not match.
pub fn find_record_span(wire: &str) -> Option<(usize, usize)> {
    let bytes = wire.as_bytes();
    let n = bytes.len();
    let mut i = 0;
    while i < n {
        if bytes[i] == b'"' {
            let start = i;
            i += 1;
            while i < n && bytes[i] != b'"' {
                if bytes[i] == b'\\' {
                    i += 1;
                }
                i += 1;
            }
            if i >= n {
                return None;
            }
            i += 1; // closing quote
            if &wire[start + 1..i - 1] == "record" {
                // skip whitespace then expect ':'
                while i < n && bytes[i].is_ascii_whitespace() {
                    i += 1;
                }
                if i < n && bytes[i] == b':' {
                    i += 1;
                    while i < n && bytes[i].is_ascii_whitespace() {
                        i += 1;
                    }
                    if i < n && bytes[i] == b'{' {
                        let val_start = i;
                        let end = scan_container_end(bytes, i, b'{', b'}');
                        return Some((val_start, end));
                    }
                    // record was null/scalar: treat as absent
                    return None;
                }
            }
        } else {
            i += 1;
        }
    }
    None
}

/// Scan a JSON container starting at `start` (the opening bracket) and return
/// the index just past its matching close, skipping string content and escapes.
fn scan_container_end(bytes: &[u8], start: usize, open: u8, close: u8) -> usize {
    let n = bytes.len();
    let mut depth = 1usize;
    let mut i = start + 1;
    while i < n && depth > 0 {
        match bytes[i] {
            b'"' => {
                i += 1;
                while i < n && bytes[i] != b'"' {
                    if bytes[i] == b'\\' {
                        i += 1;
                    }
                    i += 1;
                }
                i += 1;
            }
            c if c == open => {
                depth += 1;
                i += 1;
            }
            c if c == close => {
                depth -= 1;
                i += 1;
            }
            _ => i += 1,
        }
    }
    i.min(n)
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct CommitData {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub rev: Option<CompactString>,
    #[serde(rename = "operation")]
    pub operation_type: OperationType,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub collection: Option<CompactString>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub rkey: Option<CompactString>,
    /// Populated post-parse from the wire (see `parse_message`); the value tree
    /// is built lazily on first read.  keeps the field out
    /// of the wire parse (no tree build) while still serializing it.
    #[serde(skip_deserializing)]
    pub record: Option<RecordValue>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cid: Option<String>,
}

impl JetstreamMessage {
    /// Populate the lazily-stored record from a wire-form JSON string. Used by
    /// deserialization paths that bypass `parse_message` (storage reads, tests).
    pub fn populate_record_from_wire(&mut self, wire: &str) {
        if self.commit.is_some() {
            if let Some((start, end)) = find_record_span(wire) {
                self.commit.as_mut().unwrap().record =
                    Some(RecordValue::from_raw(wire[start..end].into()));
            }
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
    fn serialize_emits_wire_json_verbatim_when_raw_available() {
        let raw = r#"{"did":"did:plc:test","time_us":1770949213790196,"seq":100000,"kind":"commit","commit":{"rev":"3mepgzgimkv23","operation":"create","collection":"app.bsky.feed.post","rkey":"3mepgzgiatv23","record":{"$type":"app.bsky.feed.post","createdAt":"2026-02-13T02:20:02.895Z","text":"Hello world"},"cid":"bafyreiassbuahzdwy64xwlefqcwh6zk4stb4lhht24oozhxn3fhzomrxg4"}}"#;
        let message: JetstreamMessage = serde_json::from_str(raw).unwrap();
        // Programmatically-built messages have no wire form: derived serialization.
        let out = simd_json::to_string(&message).unwrap();
        let v: serde_json::Value = serde_json::from_str(&out).unwrap();
        assert_eq!(v["did"], "did:plc:test");

        // With raw_json present, the exact wire bytes are emitted verbatim.
        let mut with_raw = message.clone();
        with_raw.raw_json = Some(Arc::from(raw));
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
            Some("at://did:plc:test/app.bsky.feed.post/3mepgzgiatv23".into())
        );
    }
}
