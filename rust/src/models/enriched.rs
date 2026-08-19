use crate::client::HydrationFailure;
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

/// Serialize the author profile by splicing its pre-computed JSON fragment
/// (see `BlueskyProfile::serialized_json`) instead of re-walking the struct.
fn serialize_author_profile_spliced<S>(
    profile: &Option<Arc<BlueskyProfile>>,
    serializer: S,
) -> Result<S::Ok, S::Error>
where
    S: Serializer,
{
    match profile {
        Some(profile) => serializer
            .serialize_newtype_struct(simd_json::serde::RAW_VALUE_TOKEN, profile.serialized_json()),
        None => serializer.serialize_none(),
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
#[serde(default)]
pub struct HydratedMetadata {
    /// Quality of optional enrichment. Missing on legacy records means unknown.
    pub hydration_quality: HydrationQuality,
    /// Bounded, privacy-safe details for temporarily unavailable enrichment.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub degradation_summaries: Vec<HydrationFailure>,
    /// Author profile information
    #[serde(
        skip_serializing_if = "Option::is_none",
        serialize_with = "serialize_author_profile_spliced"
    )]
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

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum HydrationQuality {
    #[default]
    Unknown,
    Complete,
    Partial,
}

impl HydrationQuality {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Unknown => "unknown",
            Self::Complete => "complete",
            Self::Partial => "partial",
        }
    }

    pub fn from_storage(value: &str) -> Self {
        match value {
            "complete" => Self::Complete,
            "partial" => Self::Partial,
            _ => Self::Unknown,
        }
    }
}

pub const MAX_DEGRADATION_SUMMARIES: usize = 8;

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
    hydration_quality: HydrationQuality::Unknown,
    degradation_summaries: Vec::new(),
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
        use simd_json::prelude::*;
        self.message
            .commit
            .as_ref()
            .and_then(|c| c.record.as_ref())
            .and_then(|r| r.value().get("text").and_then(|v| v.as_str()))
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
    pub fn add_degradation(&mut self, failure: HydrationFailure) {
        self.hydration_quality = HydrationQuality::Partial;
        if self.degradation_summaries.len() < MAX_DEGRADATION_SUMMARIES
            && !self.degradation_summaries.contains(&failure)
        {
            self.degradation_summaries.push(failure);
        }
    }

    pub fn add_mentioned_profile(&mut self, profile: Arc<BlueskyProfile>) {
        if !self.mentioned_profiles.iter().any(|p| p.did == profile.did) {
            self.mentioned_profiles.push(profile);
        }
    }

    pub fn is_empty(&self) -> bool {
        self.hydration_quality == HydrationQuality::Unknown
            && self.degradation_summaries.is_empty()
            && self.author_profile.is_none()
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
    use crate::client::{BlueskyOperation, UpstreamFailureCategory};
    use crate::models::jetstream::{CommitData, MessageKind, OperationType, RecordValue};
    use serde_json::json;

    #[test]
    fn test_enriched_record_creation() {
        let message = JetstreamMessage {
            did: "did:plc:test".to_string(),
            time_us: Some(1640995200000000),
            seq: Some(12345),
            kind: MessageKind::Commit,
            commit: Some(Box::new(CommitData {
                rev: Some("test-rev".to_string()),
                operation_type: OperationType::Create,
                collection: Some("app.bsky.feed.post".to_string()),
                rkey: Some("test123".to_string()),
                record: Some(RecordValue::from_value(
                    simd_json::json!({"text": "Hello world"}),
                )),
                cid: Some("bafyrei".to_string()),
            })),
            raw_json: None,
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
            commit: Some(Box::new(CommitData {
                rev: Some("test-rev".to_string()),
                operation_type: OperationType::Create,
                collection: Some("app.bsky.feed.post".to_string()),
                rkey: Some("test123".to_string()),
                record: Some(RecordValue::from_value(simd_json::json!({"text": "Hello"}))),
                cid: Some("bafyrei".to_string()),
            })),
            raw_json: None,
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
            commit: Some(Box::new(CommitData {
                rev: Some("test-rev".to_string()),
                operation_type: OperationType::Create,
                collection: Some("app.bsky.feed.post".to_string()),
                rkey: Some("test123".to_string()),
                record: Some(RecordValue::from_value(
                    simd_json::json!({"text": "Hello world"}),
                )),
                cid: Some("bafyrei".to_string()),
            })),
            raw_json: None,
        };

        let enriched = EnrichedRecord::new(message);
        let json = serde_json::to_string(&enriched).unwrap();

        assert!(json.contains("\"hydrated_metadata\":{\"hydration_quality\":\"unknown\"}"));
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
        assert_eq!(
            enriched.hydrated_metadata.hydration_quality,
            HydrationQuality::Unknown
        );
    }

    #[test]
    fn degradation_summaries_are_bounded_and_privacy_safe() {
        let mut metadata = HydratedMetadata::default();
        for index in 0..(MAX_DEGRADATION_SUMMARIES + 4) {
            metadata.add_degradation(HydrationFailure {
                operation: BlueskyOperation::Posts,
                category: UpstreamFailureCategory::ServerError,
                status_class: Some("5xx".to_string()),
                attempts: 4,
                request_fingerprint: format!("safe-{index}"),
                isolation: None,
            });
        }

        assert_eq!(metadata.hydration_quality, HydrationQuality::Partial);
        assert_eq!(
            metadata.degradation_summaries.len(),
            MAX_DEGRADATION_SUMMARIES
        );
        let serialized = serde_json::to_string(&metadata).unwrap();
        assert!(!serialized.contains("at://"));
        assert!(!serialized.to_ascii_lowercase().contains("authorization"));
    }
}
