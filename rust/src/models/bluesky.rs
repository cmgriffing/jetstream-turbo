use chrono::{DateTime, Utc};
use serde::{Deserialize, Deserializer, Serialize, Serializer};
use std::sync::Arc;

fn serialize_did<S>(did: &Arc<str>, serializer: S) -> Result<S::Ok, S::Error>
where
    S: Serializer,
{
    serializer.serialize_str(did)
}

fn deserialize_did<'de, D>(deserializer: D) -> Result<Arc<str>, D::Error>
where
    D: Deserializer<'de>,
{
    let s: String = Deserialize::deserialize(deserializer)?;
    Ok(Arc::from(s))
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BlueskyProfile {
    #[serde(serialize_with = "serialize_did", deserialize_with = "deserialize_did")]
    pub did: Arc<str>,
    pub handle: String,
    #[serde(default, rename = "displayName")]
    pub display_name: Option<String>,
    pub description: Option<String>,
    pub avatar: Option<String>,
    pub banner: Option<String>,
    #[serde(default, rename = "followersCount")]
    pub followers_count: Option<u64>,
    #[serde(default, rename = "followsCount")]
    pub follows_count: Option<u64>,
    #[serde(default, rename = "postsCount")]
    pub posts_count: Option<u64>,
    pub indexed_at: Option<DateTime<Utc>>,
    pub created_at: Option<DateTime<Utc>>,
    pub labels: Option<Vec<Label>>,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct BlueskyPost {
    pub uri: String,
    pub cid: String,
    pub author: BlueskyProfile,
    pub text: String,
    pub created_at: DateTime<Utc>,
    pub embed: Option<Embed>,
    pub reply: Option<ReplyInfo>,
    pub facets: Option<Vec<Facet>>,
    pub labels: Option<Vec<Label>>,
    pub like_count: Option<u64>,
    pub repost_count: Option<u64>,
    pub reply_count: Option<u64>,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(untagged)]
pub enum Embed {
    Images(ImagesEmbed),
    External(ExternalEmbed),
    Record(RecordEmbed),
    RecordWithMedia(Box<RecordWithMediaEmbed>),
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct ImagesEmbed {
    pub images: Vec<Image>,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct Image {
    pub thumb: String,
    pub fullsize: String,
    pub alt: String,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct ExternalEmbed {
    pub uri: String,
    pub title: String,
    pub description: Option<String>,
    pub thumb: Option<String>,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct RecordEmbed {
    pub record: RecordRef,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct RecordWithMediaEmbed {
    pub record: RecordRef,
    pub media: Box<Embed>,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct RecordRef {
    pub uri: String,
    pub cid: String,
    pub author: Option<BlueskyProfile>,
    pub value: Option<serde_json::Value>,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct ReplyInfo {
    pub root: RecordRef,
    pub parent: RecordRef,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct Facet {
    pub index: FacetIndex,
    pub features: Vec<Feature>,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct FacetIndex {
    pub byte_start: u32,
    pub byte_end: u32,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(tag = "$type", rename_all = "camelCase")]
pub enum Feature {
    #[serde(rename = "app.bsky.richtext.facet#link")]
    Link { uri: String },
    #[serde(rename = "app.bsky.richtext.facet#mention")]
    Mention { did: String },
    #[serde(rename = "app.bsky.richtext.facet#tag")]
    Tag { tag: String },
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct Label {
    pub src: String,
    pub uri: String,
    pub val: String,
    pub cts: DateTime<Utc>,
    pub neg: Option<bool>,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct ActorProfile {
    #[serde(serialize_with = "serialize_did", deserialize_with = "deserialize_did")]
    pub did: Arc<str>,
    pub handle: String,
    pub display_name: Option<String>,
    pub description: Option<String>,
    pub avatar: Option<String>,
    pub banner: Option<String>,
    #[serde(default)]
    pub followers_count: Option<u64>,
    #[serde(default)]
    pub follows_count: Option<u64>,
    #[serde(default)]
    pub posts_count: Option<u64>,
    pub indexed_at: Option<DateTime<Utc>>,
    pub labels: Option<Vec<Label>>,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct ActorDefs {
    pub handle: String,
    pub display_name: Option<String>,
    pub description: Option<String>,
    pub avatar: Option<String>,
    pub labels: Option<Vec<Label>>,
}

// API Request/Response Types
#[derive(Debug, Clone, Deserialize)]
pub struct GetProfileResponse {
    #[serde(deserialize_with = "deserialize_did")]
    pub did: Arc<str>,
    pub handle: String,
    pub display_name: Option<String>,
    pub description: Option<String>,
    pub avatar: Option<String>,
    pub banner: Option<String>,
    #[serde(default)]
    pub followers_count: Option<u64>,
    #[serde(default)]
    pub follows_count: Option<u64>,
    #[serde(default)]
    pub posts_count: Option<u64>,
    pub indexed_at: Option<String>,
    pub created_at: Option<String>,
    pub labels: Option<Vec<Label>>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct GetProfilesResponse {
    pub profiles: Vec<GetProfileResponse>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct GetPostResponse {
    pub uri: String,
    pub cid: String,
    pub author: GetProfileResponse,
    pub record: serde_json::Value,
    pub embed: Option<serde_json::Value>,
    pub reply: Option<serde_json::Value>,
    pub labels: Option<Vec<Label>>,
    pub like_count: Option<u64>,
    pub repost_count: Option<u64>,
    pub reply_count: Option<u64>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct GetPostsResponse {
    pub uri: String,
    pub cid: String,
    pub author: GetProfileResponse,
    pub record: serde_json::Value,
    pub embed: Option<serde_json::Value>,
    pub reply: Option<serde_json::Value>,
    pub labels: Option<Vec<Label>>,
    pub like_count: Option<u64>,
    pub repost_count: Option<u64>,
    pub reply_count: Option<u64>,
}

impl From<GetProfileResponse> for BlueskyProfile {
    fn from(profile: GetProfileResponse) -> Self {
        Self {
            did: profile.did,
            handle: profile.handle,
            display_name: profile.display_name,
            description: profile.description,
            avatar: profile.avatar,
            banner: profile.banner,
            followers_count: profile.followers_count,
            follows_count: profile.follows_count,
            posts_count: profile.posts_count,
            indexed_at: profile.indexed_at.and_then(|s| {
                DateTime::parse_from_rfc3339(&s)
                    .ok()
                    .map(|dt| dt.with_timezone(&Utc))
            }),
            created_at: profile.created_at.and_then(|s| {
                DateTime::parse_from_rfc3339(&s)
                    .ok()
                    .map(|dt| dt.with_timezone(&Utc))
            }),
            labels: profile.labels,
        }
    }
}

#[derive(Debug, Clone, Deserialize)]
pub struct GetPostsBulkResponse {
    pub posts: Vec<GetPostsResponse>,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_bluesky_profile_deserialization() {
        let json_str = r#"
        {
            "did": "did:plc:test",
            "handle": "test.bsky.social",
            "displayName": "Test User",
            "description": "A test user",
            "followersCount": 100,
            "followsCount": 50,
            "postsCount": 25
        }
        "#;

        let profile: BlueskyProfile = serde_json::from_str(json_str).unwrap();
        assert_eq!(profile.did.as_ref(), "did:plc:test");
        assert_eq!(profile.handle, "test.bsky.social");
        assert_eq!(profile.display_name, Some("Test User".to_string()));
    }
}
