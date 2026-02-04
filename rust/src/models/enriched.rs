use crate::models::{bluesky::BlueskyProfile, jetstream::JetstreamMessage};
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct EnrichedRecord {
    /// Original jetstream message
    pub message: JetstreamMessage,
    /// Hydrated metadata including profiles and referenced content
    pub hydrated_metadata: HydratedMetadata,
    /// Processing timestamp
    pub processed_at: DateTime<Utc>,
    /// Processing metrics
    pub metrics: ProcessingMetrics,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct HydratedMetadata {
    /// Author profile information
    pub author_profile: Option<BlueskyProfile>,
    /// Profiles of mentioned users
    pub mentioned_profiles: Vec<BlueskyProfile>,
    /// Referenced posts (replies, quotes)
    pub referenced_posts: Vec<ReferencedPost>,
    /// Extracted hashtags
    pub hashtags: Vec<String>,
    /// Extracted URLs
    pub urls: Vec<String>,
    /// Extracted mentions
    pub mentions: Vec<Mention>,
    /// Content language detection
    pub detected_language: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ReferencedPost {
    pub uri: String,
    pub cid: String,
    pub text: String,
    pub author_did: String,
    pub author_handle: Option<String>,
    pub created_at: DateTime<Utc>,
    pub reply_count: Option<u64>,
    pub like_count: Option<u64>,
    pub repost_count: Option<u64>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Mention {
    pub did: String,
    pub handle: Option<String>,
    pub display_name: Option<String>,
    pub start_byte: u32,
    pub end_byte: u32,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
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

impl EnrichedRecord {
    pub fn new(message: JetstreamMessage) -> Self {
        Self {
            message,
            hydrated_metadata: HydratedMetadata {
                author_profile: None,
                mentioned_profiles: Vec::new(),
                referenced_posts: Vec::new(),
                hashtags: Vec::new(),
                urls: Vec::new(),
                mentions: Vec::new(),
                detected_language: None,
            },
            processed_at: Utc::now(),
            metrics: ProcessingMetrics {
                hydration_time_ms: 0,
                api_calls_count: 0,
                cache_hit_rate: 0.0,
                cache_hits: 0,
                cache_misses: 0,
            },
        }
    }

    pub fn get_at_uri(&self) -> Option<&str> {
        self.message.extract_at_uri()
    }

    pub fn get_did(&self) -> &str {
        self.message.extract_did()
    }

    pub fn get_text(&self) -> Option<&str> {
        match &self.message.commit.operation {
            crate::models::jetstream::Operation::Create { record }
            | crate::models::jetstream::Operation::Update { record } => {
                record.fields.get("text").and_then(|v| v.as_str())
            }
            crate::models::jetstream::Operation::Delete => None,
        }
    }

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
    pub fn add_mentioned_profile(&mut self, profile: BlueskyProfile) {
        if !self.mentioned_profiles.iter().any(|p| p.did == profile.did) {
            self.mentioned_profiles.push(profile);
        }
    }

    pub fn add_referenced_post(&mut self, post: ReferencedPost) {
        if !self.referenced_posts.iter().any(|p| p.uri == post.uri) {
            self.referenced_posts.push(post);
        }
    }

    pub fn extract_content_features(
        &mut self,
        text: &str,
        facets: &Option<Vec<crate::models::jetstream::Facet>>,
    ) {
        // Reset arrays
        self.hashtags.clear();
        self.urls.clear();
        self.mentions.clear();

        // Extract from facets
        if let Some(facets) = facets {
            for facet in facets {
                let (start, end) = (facet.index.byte_start, facet.index.byte_end);

                for feature in &facet.features {
                    match feature.r#type.as_str() {
                        "app.bsky.richtext.facet#tag" => {
                            if let (Some(start_usize), Some(end_usize)) =
                                (start.try_into().ok(), end.try_into().ok())
                            {
                                if let Some(hashtag) = text.get(start_usize..end_usize) {
                                    self.hashtags
                                        .push(hashtag.trim_start_matches('#').to_lowercase());
                                }
                            }
                        }
                        "app.bsky.richtext.facet#link" => {
                            if let Some(uri) = feature.uri.strip_prefix("http") {
                                self.urls.push(uri.to_string());
                            }
                        }
                        "app.bsky.richtext.facet#mention" => {
                            if let Some(did) = &feature.did {
                                self.mentions.push(Mention {
                                    did: did.clone(),
                                    handle: None,
                                    display_name: None,
                                    start_byte: start,
                                    end_byte: end,
                                });
                            }
                        }
                        _ => {}
                    }
                }
            }
        }
    }
}

impl Default for HydratedMetadata {
    fn default() -> Self {
        Self {
            author_profile: None,
            mentioned_profiles: Vec::new(),
            referenced_posts: Vec::new(),
            hashtags: Vec::new(),
            urls: Vec::new(),
            mentions: Vec::new(),
            detected_language: None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::models::jetstream::{CommitData, Operation, Record};
    use serde_json::json;

    #[test]
    fn test_enriched_record_creation() {
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
                        created_at: Utc::now(),
                        fields: json!({"text": "Hello world"}),
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

        let enriched = EnrichedRecord::new(message);
        assert_eq!(enriched.get_did(), "did:plc:test");
        assert_eq!(enriched.get_text(), Some("Hello world"));
    }

    #[test]
    fn test_cache_hit_rate_calculation() {
        let mut enriched = EnrichedRecord::new(JetstreamMessage {
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
                        created_at: Utc::now(),
                        fields: json!({"text": "Hello"}),
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
        });

        enriched.metrics.cache_hits = 8;
        enriched.metrics.cache_misses = 2;
        enriched.calculate_cache_hit_rate();

        assert_eq!(enriched.metrics.cache_hit_rate, 0.8);
    }
}
