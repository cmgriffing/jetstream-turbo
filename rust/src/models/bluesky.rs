use chrono::{DateTime, Utc};
use serde::de::{self, MapAccess, Visitor};
use serde::{Deserialize, Serialize, Serializer};
use std::fmt;
use std::sync::Arc;

fn serialize_did<S>(did: &Arc<str>, serializer: S) -> Result<S::Ok, S::Error>
where
    S: Serializer,
{
    serializer.serialize_str(did)
}

#[derive(Debug, Clone, Serialize)]
pub struct BlueskyProfile {
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
    #[serde(skip_serializing_if = "Option::is_none")]
    pub indexed_at: Option<DateTime<Utc>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub created_at: Option<DateTime<Utc>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub labels: Option<Vec<Label>>,
}

impl<'de> Deserialize<'de> for BlueskyProfile {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        enum Field {
            Did,
            Handle,
            DisplayName,
            Description,
            Avatar,
            Banner,
            FollowersCount,
            FollowsCount,
            PostsCount,
            IndexedAt,
            CreatedAt,
            Labels,
            Ignore,
        }

        impl<'de> Deserialize<'de> for Field {
            fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
            where
                D: serde::Deserializer<'de>,
            {
                struct FieldVisitor;

                impl Visitor<'_> for FieldVisitor {
                    type Value = Field;

                    fn expecting(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
                        formatter.write_str("a BlueskyProfile field")
                    }

                    fn visit_str<E>(self, value: &str) -> Result<Self::Value, E>
                    where
                        E: de::Error,
                    {
                        Ok(match value {
                            "did" => Field::Did,
                            "handle" => Field::Handle,
                            "displayName" => Field::DisplayName,
                            "description" => Field::Description,
                            "avatar" => Field::Avatar,
                            "banner" => Field::Banner,
                            "followersCount" => Field::FollowersCount,
                            "followsCount" => Field::FollowsCount,
                            "postsCount" => Field::PostsCount,
                            "indexed_at" => Field::IndexedAt,
                            "created_at" => Field::CreatedAt,
                            "labels" => Field::Labels,
                            _ => Field::Ignore,
                        })
                    }
                }

                deserializer.deserialize_identifier(FieldVisitor)
            }
        }

        struct BlueskyProfileVisitor;

        impl<'de> Visitor<'de> for BlueskyProfileVisitor {
            type Value = BlueskyProfile;

            fn expecting(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
                formatter.write_str("a BlueskyProfile")
            }

            fn visit_map<A>(self, mut map: A) -> Result<Self::Value, A::Error>
            where
                A: MapAccess<'de>,
            {
                let mut did = None;
                let mut handle = None;
                let mut display_name = None;
                let mut description = None;
                let mut avatar = None;
                let mut banner = None;
                let mut followers_count = None;
                let mut follows_count = None;
                let mut posts_count = None;
                let mut indexed_at = None;
                let mut created_at = None;
                let mut labels = None;

                while let Some(field) = map.next_key()? {
                    match field {
                        Field::Did => {
                            if did.is_some() {
                                return Err(de::Error::duplicate_field("did"));
                            }
                            did = Some(map.next_value()?);
                        }
                        Field::Handle => {
                            if handle.is_some() {
                                return Err(de::Error::duplicate_field("handle"));
                            }
                            handle = Some(map.next_value()?);
                        }
                        Field::DisplayName => {
                            if display_name.is_some() {
                                return Err(de::Error::duplicate_field("displayName"));
                            }
                            display_name = Some(map.next_value()?);
                        }
                        Field::Description => {
                            if description.is_some() {
                                return Err(de::Error::duplicate_field("description"));
                            }
                            description = Some(map.next_value()?);
                        }
                        Field::Avatar => {
                            if avatar.is_some() {
                                return Err(de::Error::duplicate_field("avatar"));
                            }
                            avatar = Some(map.next_value()?);
                        }
                        Field::Banner => {
                            if banner.is_some() {
                                return Err(de::Error::duplicate_field("banner"));
                            }
                            banner = Some(map.next_value()?);
                        }
                        Field::FollowersCount => {
                            if followers_count.is_some() {
                                return Err(de::Error::duplicate_field("followersCount"));
                            }
                            followers_count = Some(map.next_value()?);
                        }
                        Field::FollowsCount => {
                            if follows_count.is_some() {
                                return Err(de::Error::duplicate_field("followsCount"));
                            }
                            follows_count = Some(map.next_value()?);
                        }
                        Field::PostsCount => {
                            if posts_count.is_some() {
                                return Err(de::Error::duplicate_field("postsCount"));
                            }
                            posts_count = Some(map.next_value()?);
                        }
                        Field::IndexedAt => {
                            if indexed_at.is_some() {
                                return Err(de::Error::duplicate_field("indexed_at"));
                            }
                            indexed_at = Some(map.next_value()?);
                        }
                        Field::CreatedAt => {
                            if created_at.is_some() {
                                return Err(de::Error::duplicate_field("created_at"));
                            }
                            created_at = Some(map.next_value()?);
                        }
                        Field::Labels => {
                            if labels.is_some() {
                                return Err(de::Error::duplicate_field("labels"));
                            }
                            labels = Some(map.next_value()?);
                        }
                        Field::Ignore => {
                            let _ = map.next_value::<de::IgnoredAny>()?;
                        }
                    }
                }

                Ok(BlueskyProfile {
                    did: did.ok_or_else(|| de::Error::missing_field("did"))?,
                    handle: handle.ok_or_else(|| de::Error::missing_field("handle"))?,
                    display_name: display_name.unwrap_or_default(),
                    description: description.unwrap_or_default(),
                    avatar: avatar.unwrap_or_default(),
                    banner: banner.unwrap_or_default(),
                    followers_count: followers_count.unwrap_or_default(),
                    follows_count: follows_count.unwrap_or_default(),
                    posts_count: posts_count.unwrap_or_default(),
                    indexed_at: indexed_at.unwrap_or_default(),
                    created_at: created_at.unwrap_or_default(),
                    labels: labels.unwrap_or_default(),
                })
            }
        }

        const FIELDS: &[&str] = &[
            "did",
            "handle",
            "displayName",
            "description",
            "avatar",
            "banner",
            "followersCount",
            "followsCount",
            "postsCount",
            "indexed_at",
            "created_at",
            "labels",
        ];

        deserializer.deserialize_struct("BlueskyProfile", FIELDS, BlueskyProfileVisitor)
    }
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
    #[serde(serialize_with = "serialize_did")]
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
