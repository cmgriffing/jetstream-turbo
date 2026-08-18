use crate::models::recovery::SourceEventId;
use crate::models::{bluesky::BlueskyProfile, jetstream::JetstreamMessage};
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize, Serializer};
use std::sync::Arc;

fn serialize_arc_str<S>(value: &Arc<str>, serializer: S) -> Result<S::Ok, S::Error>
where
    S: Serializer,
{
    serializer.serialize_str(value)
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct EnrichedRecord {
    /// Original jetstream message
    pub message: JetstreamMessage,
    /// Hydrated metadata including profiles and referenced content
    #[serde(default)]
    pub hydrated_metadata: HydratedMetadata,
    /// Processing timestamp
    pub processed_at: DateTime<Utc>,
    /// Processing metrics
    pub metrics: ProcessingMetrics,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
#[serde(default)]
pub struct HydratedMetadata {
    /// Author profile information
    #[serde(skip_serializing_if = "Option::is_none")]
    pub author_profile: Option<Arc<BlueskyProfile>>,
    /// Profiles of mentioned users
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub mentioned_profiles: Vec<Arc<BlueskyProfile>>,
    /// Referenced posts (replies, quotes)
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub referenced_posts: Vec<ReferencedPost>,
    /// Extracted hashtags
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub hashtags: Vec<String>,
    /// Extracted URLs
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub urls: Vec<String>,
    /// Extracted mentions
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub mentions: Vec<Mention>,
    /// Content language detection
    #[serde(skip_serializing_if = "Option::is_none")]
    pub detected_language: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ReferencedPost {
    pub uri: String,
    pub cid: String,
    pub text: String,
    #[serde(serialize_with = "serialize_arc_str")]
    pub author_did: Arc<str>,
    pub author_handle: Option<String>,
    pub created_at: DateTime<Utc>,
    pub reply_count: Option<u64>,
    pub like_count: Option<u64>,
    pub repost_count: Option<u64>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Mention {
    #[serde(serialize_with = "serialize_arc_str")]
    pub did: Arc<str>,
    pub handle: Option<String>,
    pub display_name: Option<String>,
    pub start_byte: u32,
    pub end_byte: u32,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct ProcessingMetrics {
    /// Time taken to hydrate this record
    pub hydration_time_ms: u64,
    /// Number of API calls made
    pub api_calls_count: u32,
    /// Cache hit rate for this record
    pub cache_hit_rate: f64,
    /// Number of items fetched from cache vs API
    pub cache_hits: u32,
    pub cache_misses: u32,
}

const DEFAULT_HYDRATED: HydratedMetadata = HydratedMetadata {
    author_profile: None,
    mentioned_profiles: Vec::new(),
    referenced_posts: Vec::new(),
    hashtags: Vec::new(),
    urls: Vec::new(),
    mentions: Vec::new(),
    detected_language: None,
};

const DEFAULT_METRICS: ProcessingMetrics = ProcessingMetrics {
    hydration_time_ms: 0,
    api_calls_count: 0,
    cache_hit_rate: 0.0,
    cache_hits: 0,
    cache_misses: 0,
};

impl EnrichedRecord {
    #[inline(always)]
    pub fn new(message: JetstreamMessage) -> Self {
        Self {
            message,
            hydrated_metadata: DEFAULT_HYDRATED,
            processed_at: DateTime::UNIX_EPOCH,
            metrics: DEFAULT_METRICS,
        }
    }

    pub fn new_with_timestamp(message: JetstreamMessage, processed_at: DateTime<Utc>) -> Self {
        let mut record = Self::new(message);
        record.processed_at = processed_at;
        record
    }

    #[inline(always)]
    pub fn get_at_uri(&self) -> Option<String> {
        self.message.extract_at_uri()
    }

    #[inline(always)]
    pub fn get_did(&self) -> &str {
        self.message.extract_did()
    }

    pub fn source_event_id(&self) -> SourceEventId {
        SourceEventId::from_message(&self.message)
    }

    #[inline(always)]
    pub fn get_text(&self) -> Option<&str> {
        self.message
            .commit
            .as_ref()
            .and_then(|c| c.record.as_ref())
            .and_then(|r| r.get("text").and_then(|v| v.as_str()))
    }

    #[inline(always)]
    pub fn calculate_cache_hit_rate(&mut self) {
        let total = self.metrics.cache_hits + self.metrics.cache_misses;
        self.metrics.cache_hit_rate = if total > 0 {
            self.metrics.cache_hits as f64 / total as f64
        } else {
            0.0
        };
    }
}

impl HydratedMetadata {
    pub fn add_mentioned_profile(&mut self, profile: Arc<BlueskyProfile>) {
        if !self.mentioned_profiles.iter().any(|p| p.did == profile.did) {
            self.mentioned_profiles.push(profile);
        }
    }

    pub fn is_empty(&self) -> bool {
        self.author_profile.is_none()
            && self.mentioned_profiles.is_empty()
            && self.referenced_posts.is_empty()
            && self.hashtags.is_empty()
            && self.urls.is_empty()
            && self.mentions.is_empty()
            && self.detected_language.is_none()
    }

    pub fn add_referenced_post(&mut self, post: ReferencedPost) {
        if !self.referenced_posts.iter().any(|p| p.uri == post.uri) {
            self.referenced_posts.push(post);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::models::jetstream::{CommitData, MessageKind, OperationType};
    use serde_json::json;

    #[test]
    fn test_enriched_record_creation() {
        let message = JetstreamMessage {
            did: "did:plc:test".to_string(),
            time_us: Some(1640995200000000),
            seq: Some(12345),
            kind: MessageKind::Commit,
            commit: Some(CommitData {
                rev: Some("test-rev".to_string()),
                operation_type: OperationType::Create,
                collection: Some("app.bsky.feed.post".to_string()),
                rkey: Some("test123".to_string()),
                record: Some(json!({"text": "Hello world"})),
                cid: Some("bafyrei".to_string()),
            }),
        };

        let enriched = EnrichedRecord::new(message);
        assert_eq!(enriched.get_did(), "did:plc:test");
        assert_eq!(enriched.get_text(), Some("Hello world"));
    }

    #[test]
    fn test_cache_hit_rate_calculation() {
        let mut enriched = EnrichedRecord::new(JetstreamMessage {
            did: "did:plc:test".to_string(),
            time_us: Some(1640995200000000),
            seq: Some(12345),
            kind: MessageKind::Commit,
            commit: Some(CommitData {
                rev: Some("test-rev".to_string()),
                operation_type: OperationType::Create,
                collection: Some("app.bsky.feed.post".to_string()),
                rkey: Some("test123".to_string()),
                record: Some(json!({"text": "Hello"})),
                cid: Some("bafyrei".to_string()),
            }),
        });

        enriched.metrics.cache_hits = 8;
        enriched.metrics.cache_misses = 2;
        enriched.calculate_cache_hit_rate();

        assert_eq!(enriched.metrics.cache_hit_rate, 0.8);
    }

    #[test]
    fn test_empty_hydrated_metadata_serializes_compactly() {
        let message = JetstreamMessage {
            did: "did:plc:test".to_string(),
            time_us: Some(1640995200000000),
            seq: Some(12345),
            kind: MessageKind::Commit,
            commit: Some(CommitData {
                rev: Some("test-rev".to_string()),
                operation_type: OperationType::Create,
                collection: Some("app.bsky.feed.post".to_string()),
                rkey: Some("test123".to_string()),
                record: Some(json!({"text": "Hello world"})),
                cid: Some("bafyrei".to_string()),
            }),
        };

        let enriched = EnrichedRecord::new(message);
        let json = serde_json::to_string(&enriched).unwrap();

        assert!(json.contains("\"hydrated_metadata\":{}"));
        assert!(!json.contains("\"mentioned_profiles\""));
        assert!(!json.contains("\"referenced_posts\""));
        assert!(!json.contains("\"hashtags\""));
        assert!(!json.contains("\"urls\""));
        assert!(!json.contains("\"mentions\""));
    }

    #[test]
    fn test_hydrated_metadata_defaults_when_fields_are_missing() {
        let enriched: EnrichedRecord = serde_json::from_value(json!({
            "message": {
                "did": "did:plc:test",
                "time_us": 1640995200000000_u64,
                "seq": 12345_u64,
                "kind": "commit",
                "commit": {
                    "rev": "test-rev",
                    "operation": "create",
                    "collection": "app.bsky.feed.post",
                    "rkey": "test123",
                    "record": {"text": "Hello world"},
                    "cid": "bafyrei"
                }
            },
            "hydrated_metadata": {},
            "processed_at": "2024-01-01T00:00:00Z",
            "metrics": {
                "hydration_time_ms": 0,
                "api_calls_count": 0,
                "cache_hit_rate": 0.0,
                "cache_hits": 0,
                "cache_misses": 0
            }
        }))
        .unwrap();

        assert!(enriched.hydrated_metadata.is_empty());
    }
}
