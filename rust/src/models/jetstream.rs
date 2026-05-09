use serde::{Deserialize, Serialize, Serializer};

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

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct JetstreamMessage {
    pub did: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub time_us: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub seq: Option<u64>,
    pub kind: MessageKind,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub commit: Option<CommitData>,
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
    pub record: Option<serde_json::Value>,
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

    #[test]
    fn test_extract_mentioned_dids() {
        let json_str = r#"
        {
            "did": "did:plc:author",
            "time_us": 1770949213800000,
            "kind": "commit",
            "commit": {
                "operation": "create",
                "collection": "app.bsky.feed.post",
                "rkey": "test123",
                "record": {
                    "$type": "app.bsky.feed.post",
                    "text": "Test",
                    "reply": {
                        "parent": {
                            "cid": "bafyrei...",
                            "uri": "at://did:plc:parent123/app.bsky.feed.post/parent456"
                        },
                        "root": {
                            "cid": "bafyrei...",
                            "uri": "at://did:plc:root789/app.bsky.feed.post/root000"
                        }
                    }
                }
            }
        }
        "#;

        let message: JetstreamMessage = serde_json::from_str(json_str).unwrap();
        let mentioned = message.extract_mentioned_dids();
        assert!(mentioned.contains(&"did:plc:parent123"));
        assert!(mentioned.contains(&"did:plc:root789"));
    }
}
