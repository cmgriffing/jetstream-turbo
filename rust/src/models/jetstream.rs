use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct JetstreamMessage {
    pub did: String,
    #[serde(default)]
    pub time_us: Option<u64>,
    #[serde(default)]
    pub seq: Option<u64>,
    pub kind: String,
    #[serde(default)]
    pub commit: Option<CommitData>,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct CommitData {
    #[serde(default)]
    pub rev: Option<String>,
    #[serde(rename = "operation")]
    pub operation_type: String,
    #[serde(default)]
    pub collection: Option<String>,
    #[serde(default)]
    pub rkey: Option<String>,
    #[serde(default)]
    pub record: Option<serde_json::Value>,
    #[serde(default)]
    pub cid: Option<String>,
}

impl JetstreamMessage {
    pub fn extract_at_uri(&self) -> Option<String> {
        if let Some(commit) = &self.commit {
            if let (Some(collection), Some(rkey)) = (&commit.collection, &commit.rkey) {
                return Some(format!("at://{}/{}/{}", self.did, collection, rkey));
            }
        }
        None
    }

    pub fn extract_did(&self) -> &str {
        &self.did
    }

    pub fn is_create_operation(&self) -> bool {
        if let Some(commit) = &self.commit {
            return commit.operation_type == "create";
        }
        false
    }

    pub fn extract_mentioned_dids(&self) -> Vec<&str> {
        let mut mentioned_dids = Vec::new();

        if let Some(commit) = &self.commit {
            if let Some(record) = &commit.record {
                // Extract from reply references
                if let Some(reply) = record.get("reply") {
                    if let Some(parent) = reply.get("parent") {
                        if let Some(uri) = parent.get("uri").and_then(|u| u.as_str()) {
                            if let Some(did) = uri.split(':').nth(2) {
                                mentioned_dids.push(did);
                            }
                        }
                    }
                    if let Some(root) = reply.get("root") {
                        if let Some(uri) = root.get("uri").and_then(|u| u.as_str()) {
                            if let Some(did) = uri.split(':').nth(2) {
                                mentioned_dids.push(did);
                            }
                        }
                    }
                }

                // Extract from facets (mentions)
                if let Some(facets) = record.get("facets").and_then(|f| f.as_array()) {
                    for facet in facets {
                        if let Some(features) = facet.get("features").and_then(|f| f.as_array()) {
                            for feature in features {
                                // Check for mention features
                                if let Some(did) = feature.get("did").and_then(|d| d.as_str()) {
                                    mentioned_dids.push(did);
                                }
                            }
                        }
                    }
                }

                // Extract from embeds (quotes)
                if let Some(embed) = record.get("embed") {
                    if let Some(embed_record) = embed.get("record") {
                        if let Some(uri) = embed_record.get("uri").and_then(|u| u.as_str()) {
                            if let Some(did) = uri.split(':').nth(2) {
                                mentioned_dids.push(did);
                            }
                        }
                    }
                }
            }
        }

        mentioned_dids.retain(|did| did.starts_with("did:plc:"));
        mentioned_dids.dedup();
        mentioned_dids
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
