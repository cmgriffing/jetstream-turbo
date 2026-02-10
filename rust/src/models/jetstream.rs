use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct JetstreamMessage {
    pub did: String,
    pub seq: u64,
    pub time_us: u64,
    pub commit: CommitData,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct CommitData {
    pub seq: u64,
    pub rebase: bool,
    pub time_us: u64,
    pub operation: Operation,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(tag = "type", rename_all = "lowercase")]
pub enum Operation {
    Create { record: Record },
    Update { record: Record },
    Delete,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct Record {
    pub uri: String,
    pub cid: String,
    pub author: String,
    pub r#type: String,
    pub created_at: DateTime<Utc>,
    pub fields: serde_json::Value,
    pub embed: Option<serde_json::Value>,
    pub labels: Option<Vec<Label>>,
    pub langs: Option<Vec<String>>,
    pub reply: Option<ReplyRef>,
    pub tags: Option<Vec<String>>,
    pub facets: Option<Vec<Facet>>,
    pub collections: Option<Vec<String>>,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct Label {
    pub src: String,
    pub uri: String,
    pub val: String,
    pub cts: DateTime<Utc>,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct ReplyRef {
    pub root: RecordRef,
    pub parent: RecordRef,
    pub gate: Option<RecordRef>,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct RecordRef {
    pub uri: String,
    pub cid: String,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct Facet {
    pub index: FacetIndex,
    pub features: Vec<FacetFeature>,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct FacetIndex {
    pub byte_start: u32,
    pub byte_end: u32,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct FacetFeature {
    pub r#type: String,
    pub uri: String,
    pub did: Option<String>,
}

impl JetstreamMessage {
    pub fn extract_at_uri(&self) -> Option<&str> {
        match &self.commit.operation {
            Operation::Create { record } => Some(&record.uri),
            Operation::Update { record } => Some(&record.uri),
            Operation::Delete => None,
        }
    }

    pub fn extract_did(&self) -> &str {
        &self.did
    }

    pub fn is_create_operation(&self) -> bool {
        matches!(self.commit.operation, Operation::Create { .. })
    }

    pub fn extract_mentioned_dids(&self) -> Vec<&str> {
        let mut mentioned_dids = Vec::new();

        if let Operation::Create { record } | Operation::Update { record } = &self.commit.operation
        {
            // Extract from reply references
            if let Some(reply) = &record.reply {
                mentioned_dids.push(reply.root.uri.split(':').next().unwrap_or(""));
                mentioned_dids.push(reply.parent.uri.split(':').next().unwrap_or(""));
            }

            // Extract from facets (mentions, quotes)
            if let Some(facets) = &record.facets {
                for facet in facets {
                    for feature in &facet.features {
                        if let Some(did) = &feature.did {
                            mentioned_dids.push(did);
                        }
                    }
                }
            }

            // Extract from embeds
            if let Some(embed) = &record.embed {
                if let Some(embed_obj) = embed.as_object() {
                    if let Some(record) = embed_obj.get("record") {
                        if let Some(uri) = record.get("uri") {
                            if let Some(uri_str) = uri.as_str() {
                                mentioned_dids.push(uri_str.split(':').next().unwrap_or(""));
                            }
                        }
                    }
                }
            }
        }

        mentioned_dids.retain(|did| !did.is_empty() && did.starts_with("did:plc:"));
        mentioned_dids.dedup();
        mentioned_dids
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn test_jetstream_message_parsing() {
        let json_str = r#"
        {
            "did": "did:plc:test",
            "seq": 12345,
            "time_us": 1640995200000000,
            "commit": {
                "seq": 12345,
                "rebase": false,
                "time_us": 1640995200000000,
                "operation": {
                    "type": "create",
                    "record": {
                        "uri": "at://did:plc:test/app.bsky.feed.post/test",
                        "cid": "bafyrei",
                        "author": "did:plc:test",
                        "type": "app.bsky.feed.post",
                        "created_at": "2022-01-01T00:00:00Z",
                        "fields": {}
                    }
                }
            }
        }
        "#;

        let message: JetstreamMessage = serde_json::from_str(json_str).unwrap();
        assert_eq!(message.did, "did:plc:test");
        assert_eq!(message.seq, 12345);
        assert!(message.is_create_operation());
    }

    #[test]
    fn test_extract_at_uri() {
        let message = JetstreamMessage {
            did: "did:plc:test".to_string(),
            seq: 12345,
            time_us: 1640995200000000,
            commit: CommitData {
                seq: 12345,
                rebase: false,
                time_us: 1640995200000000,
                operation: Operation::Create {
                    record: Record {
                        uri: "at://did:plc:test/app.bsky.feed.post/test".to_string(),
                        cid: "bafyrei".to_string(),
                        author: "did:plc:test".to_string(),
                        r#type: "app.bsky.feed.post".to_string(),
                        created_at: chrono::Utc::now(),
                        fields: json!({}),
                        embed: None,
                        labels: None,
                        langs: None,
                        reply: None,
                        tags: None,
                        facets: None,
                        collections: None,
                    },
                },
            },
        };

        assert_eq!(
            message.extract_at_uri(),
            Some("at://did:plc:test/app.bsky.feed.post/test")
        );
    }
}
