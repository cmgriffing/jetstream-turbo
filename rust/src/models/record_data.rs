//! Semantic view of a Bluesky record's JSON, captured during parsing.
//!
//! The wire parse previously materialized every record as a full
//! `simd_json::OwnedValue` DOM (HashMap + per-key String allocs) even though
//! the only consumers are (a) reference extraction in the hydrator and (b) the
//! `text` accessor. With storage now writing the raw wire bytes verbatim, the
//! DOM is pure overhead: ~1.6ms/batch to build, ~1.2ms/batch to drop.
//!
//! `RecordData` replaces it: a small lenient serde walker extracts exactly the
//! fields the pipeline reads (`text`, reply refs, embed URI, facets) straight
//! from the parser's tape in one pass. It never fails on any JSON value
//! (unexpected shapes degrade to empty data, mirroring the previous
//! `RecordView` semantics over the DOM).

use serde::de::{self, Deserialize, Deserializer, IgnoredAny, MapAccess, SeqAccess, Visitor};
use serde::Serialize;
use std::fmt;

/// Semantic contents of a record captured during parsing.
#[derive(Debug, Clone, Default, PartialEq, Serialize)]
pub struct RecordData {
    /// The post text, if present and a string.
    pub text: Option<String>,
    /// `reply.parent.uri`
    pub reply_parent_uri: Option<String>,
    /// `reply.root.uri`
    pub reply_root_uri: Option<String>,
    /// `embed.record.uri`
    pub embed_record_uri: Option<String>,
    /// All facets (rich-text annotations) present in the record.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub facets: Vec<FacetData>,
}

/// One rich-text facet: byte indices plus its typed features.
#[derive(Debug, Clone, Default, PartialEq, Serialize)]
pub struct FacetData {
    pub byte_start: u32,
    pub byte_end: u32,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub features: Vec<FeatureData>,
}

/// A typed rich-text feature within a facet.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub enum FeatureData {
    Tag { tag: String },
    Link { uri: String },
    Mention { did: String },
}

impl RecordData {
    /// Build a `RecordData` from a `serde_json::Value` (fixtures/tests). Mirrors
    /// the wire-extraction semantics.
    pub fn from_value(value: serde_json::Value) -> Self {
        let mut data = RecordData::default();
        data.text = value
            .get("text")
            .and_then(|v| v.as_str())
            .map(str::to_owned);
        if let Some(reply) = value.get("reply") {
            data.reply_parent_uri = reply
                .get("parent")
                .and_then(|p| p.get("uri"))
                .and_then(|v| v.as_str())
                .map(str::to_owned);
            data.reply_root_uri = reply
                .get("root")
                .and_then(|r| r.get("uri"))
                .and_then(|v| v.as_str())
                .map(str::to_owned);
        }
        if let Some(embed) = value.get("embed") {
            data.embed_record_uri = embed
                .get("record")
                .and_then(|r| r.get("uri"))
                .and_then(|v| v.as_str())
                .map(str::to_owned);
        }
        if let Some(facets) = value.get("facets").and_then(|v| v.as_array()) {
            for facet in facets {
                let Some(index) = facet.get("index") else { continue };
                let Some(byte_start) = index.get("byteStart").and_then(|v| v.as_u64()) else {
                    continue;
                };
                let Some(byte_end) = index.get("byteEnd").and_then(|v| v.as_u64()) else {
                    continue;
                };
                let features = facet
                    .get("features")
                    .and_then(|f| f.as_array())
                    .map(|arr| {
                        arr.iter()
                            .filter_map(|feature| {
                                match feature
                                    .get("$type")
                                    .and_then(|v| v.as_str())
                                {
                                    Some("app.bsky.richtext.facet#tag") => feature
                                        .get("tag")
                                        .and_then(|v| v.as_str())
                                        .map(|tag| FeatureData::Tag {
                                            tag: tag.to_owned(),
                                        }),
                                    Some("app.bsky.richtext.facet#link") => feature
                                        .get("uri")
                                        .and_then(|v| v.as_str())
                                        .map(|uri| FeatureData::Link {
                                            uri: uri.to_owned(),
                                        }),
                                    Some("app.bsky.richtext.facet#mention") => feature
                                        .get("did")
                                        .and_then(|v| v.as_str())
                                        .map(|did| FeatureData::Mention {
                                            did: did.to_owned(),
                                        }),
                                    _ => None,
                                }
                            })
                            .collect()
                    })
                    .unwrap_or_default();
                data.facets.push(FacetData {
                    byte_start: byte_start as u32,
                    byte_end: byte_end as u32,
                    features,
                });
            }
        }
        data
    }
}

// ---------------------------------------------------------------------------
// Lenient wire extraction. Every visitor tolerates ANY JSON value without
// failing (matching the previous always-succeeding DOM parse): expected shapes
// are read, everything else is consumed and yields defaults.
// ---------------------------------------------------------------------------

/// A string that degrades to `None` for any non-string value.
struct LenStr(Option<String>);

impl<'de> Deserialize<'de> for LenStr {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        deserializer.deserialize_any(LenStrVisitor)
    }
}

struct LenStrVisitor;

impl<'de> Visitor<'de> for LenStrVisitor {
    type Value = LenStr;

    fn expecting(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("a JSON value")
    }

    fn visit_unit<E>(self) -> Result<Self::Value, E>
    where
        E: de::Error,
    {
        Ok(LenStr(None))
    }

    fn visit_bool<E>(self, _v: bool) -> Result<Self::Value, E>
    where
        E: de::Error,
    {
        Ok(LenStr(None))
    }

    fn visit_i64<E>(self, _v: i64) -> Result<Self::Value, E>
    where
        E: de::Error,
    {
        Ok(LenStr(None))
    }

    fn visit_u64<E>(self, _v: u64) -> Result<Self::Value, E>
    where
        E: de::Error,
    {
        Ok(LenStr(None))
    }

    fn visit_f64<E>(self, _v: f64) -> Result<Self::Value, E>
    where
        E: de::Error,
    {
        Ok(LenStr(None))
    }

    fn visit_str<E>(self, v: &str) -> Result<Self::Value, E>
    where
        E: de::Error,
    {
        Ok(LenStr(Some(v.to_owned())))
    }

    fn visit_string<E>(self, v: String) -> Result<Self::Value, E>
    where
        E: de::Error,
    {
        Ok(LenStr(Some(v)))
    }

    fn visit_seq<A>(self, mut seq: A) -> Result<Self::Value, A::Error>
    where
        A: SeqAccess<'de>,
    {
        while let Some(_) = seq.next_element::<IgnoredAny>()? {}
        Ok(LenStr(None))
    }

    fn visit_map<A>(self, mut map: A) -> Result<Self::Value, A::Error>
    where
        A: MapAccess<'de>,
    {
        while let Some(_) = map.next_entry::<IgnoredAny, IgnoredAny>()? {}
        Ok(LenStr(None))
    }
}

/// Lenient default methods for structural visitors (scalars degrade to the
/// default value; the macro does NOT emit `visit_seq`/`visit_map`, which each
/// visitor implements itself).
macro_rules! lenient_scalars {
    () => {
        fn expecting(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
            f.write_str("a JSON value")
        }

        fn visit_unit<E>(self) -> Result<Self::Value, E>
        where
            E: de::Error,
        {
            Ok(Default::default())
        }

        fn visit_bool<E>(self, _v: bool) -> Result<Self::Value, E>
        where
            E: de::Error,
        {
            Ok(Default::default())
        }

        fn visit_i64<E>(self, _v: i64) -> Result<Self::Value, E>
        where
            E: de::Error,
        {
            Ok(Default::default())
        }

        fn visit_u64<E>(self, _v: u64) -> Result<Self::Value, E>
        where
            E: de::Error,
        {
            Ok(Default::default())
        }

        fn visit_f64<E>(self, _v: f64) -> Result<Self::Value, E>
        where
            E: de::Error,
        {
            Ok(Default::default())
        }

        fn visit_str<E>(self, _v: &str) -> Result<Self::Value, E>
        where
            E: de::Error,
        {
            Ok(Default::default())
        }

        fn visit_string<E>(self, _v: String) -> Result<Self::Value, E>
        where
            E: de::Error,
        {
            Ok(Default::default())
        }
    };
}

/// Drain an unexpected map fully (must consume before the parent cursor
/// advances).
fn drain_map<'de, A>(mut map: A) -> Result<(), A::Error>
where
    A: MapAccess<'de>,
{
    while let Some(_) = map.next_entry::<IgnoredAny, IgnoredAny>()? {}
    Ok(())
}

/// Drain an unexpected sequence fully.
fn drain_seq<'de, A>(mut seq: A) -> Result<(), A::Error>
where
    A: SeqAccess<'de>,
{
    while let Some(_) = seq.next_element::<IgnoredAny>()? {}
    Ok(())
}

// ---- record object ---------------------------------------------------------

struct RecordDataVisitor;

impl<'de> Visitor<'de> for RecordDataVisitor {
    type Value = RecordData;

    lenient_scalars!();

    fn visit_map<A>(self, mut map: A) -> Result<Self::Value, A::Error>
    where
        A: MapAccess<'de>,
    {
        let mut data = RecordData::default();
        while let Some(key) = map.next_key::<std::borrow::Cow<'de, str>>()? {
            match key.as_ref() {
                "text" => data.text = map.next_value::<LenStr>()?.0,
                "reply" => {
                    let reply = map.next_value::<ReplyView>()?;
                    data.reply_parent_uri = reply.parent;
                    data.reply_root_uri = reply.root;
                }
                "embed" => {
                    let embed = map.next_value::<EmbedView>()?;
                    data.embed_record_uri = embed.uri;
                }
                "facets" => data.facets = map.next_value::<FacetsView>()?.0,
                _ => {
                    map.next_value::<IgnoredAny>()?;
                }
            }
        }
        Ok(data)
    }

    fn visit_seq<A>(self, seq: A) -> Result<Self::Value, A::Error>
    where
        A: SeqAccess<'de>,
    {
        drain_seq(seq)?;
        Ok(RecordData::default())
    }
}

impl<'de> Deserialize<'de> for RecordData {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        deserializer.deserialize_any(RecordDataVisitor)
    }
}

// ---- reply: { "parent": { "uri": ... }, "root": { "uri": ... } } -----------

#[derive(Default)]
struct ReplyView {
    parent: Option<String>,
    root: Option<String>,
}

struct ReplyVisitor;

impl<'de> Visitor<'de> for ReplyVisitor {
    type Value = ReplyView;

    lenient_scalars!();

    fn visit_map<A>(self, mut map: A) -> Result<Self::Value, A::Error>
    where
        A: MapAccess<'de>,
    {
        let mut parent = None;
        let mut root = None;
        while let Some(key) = map.next_key::<std::borrow::Cow<'de, str>>()? {
            match key.as_ref() {
                "parent" => parent = map.next_value::<UriView>()?.uri,
                "root" => root = map.next_value::<UriView>()?.uri,
                _ => {
                    map.next_value::<IgnoredAny>()?;
                }
            }
        }
        Ok(ReplyView { parent, root })
    }

    fn visit_seq<A>(self, seq: A) -> Result<Self::Value, A::Error>
    where
        A: SeqAccess<'de>,
    {
        drain_seq(seq)?;
        Ok(ReplyView {
            parent: None,
            root: None,
        })
    }
}

impl<'de> Deserialize<'de> for ReplyView {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        deserializer.deserialize_any(ReplyVisitor)
    }
}

// ---- embed: { "record": { "uri": ... } } -----------------------------------

#[derive(Default)]
struct EmbedView {
    uri: Option<String>,
}

struct EmbedVisitor;

impl<'de> Visitor<'de> for EmbedVisitor {
    type Value = EmbedView;

    lenient_scalars!();

    fn visit_map<A>(self, mut map: A) -> Result<Self::Value, A::Error>
    where
        A: MapAccess<'de>,
    {
        let mut uri = None;
        while let Some(key) = map.next_key::<std::borrow::Cow<'de, str>>()? {
            match key.as_ref() {
                "record" => uri = map.next_value::<UriView>()?.uri,
                _ => {
                    map.next_value::<IgnoredAny>()?;
                }
            }
        }
        Ok(EmbedView { uri })
    }

    fn visit_seq<A>(self, seq: A) -> Result<Self::Value, A::Error>
    where
        A: SeqAccess<'de>,
    {
        drain_seq(seq)?;
        Ok(EmbedView { uri: None })
    }
}

impl<'de> Deserialize<'de> for EmbedView {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        deserializer.deserialize_any(EmbedVisitor)
    }
}

// ---- a `{ "uri": ... }` wrapper --------------------------------------------

#[derive(Default)]
struct UriView {
    uri: Option<String>,
}

struct UriVisitor;

impl<'de> Visitor<'de> for UriVisitor {
    type Value = UriView;

    lenient_scalars!();

    fn visit_map<A>(self, mut map: A) -> Result<Self::Value, A::Error>
    where
        A: MapAccess<'de>,
    {
        let mut uri = None;
        while let Some(key) = map.next_key::<std::borrow::Cow<'de, str>>()? {
            if key == "uri" {
                uri = map.next_value::<LenStr>()?.0;
            } else {
                map.next_value::<IgnoredAny>()?;
            }
        }
        Ok(UriView { uri })
    }

    fn visit_seq<A>(self, seq: A) -> Result<Self::Value, A::Error>
    where
        A: SeqAccess<'de>,
    {
        drain_seq(seq)?;
        Ok(UriView { uri: None })
    }
}

impl<'de> Deserialize<'de> for UriView {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        deserializer.deserialize_any(UriVisitor)
    }
}

// ---- facets: [ { "index": {...}, "features": [...] } ] ----------------------

#[derive(Default)]
struct FacetsView(Vec<FacetData>);

struct FacetsVisitor;

impl<'de> Visitor<'de> for FacetsVisitor {
    type Value = FacetsView;

    lenient_scalars!();

    fn visit_map<A>(self, map: A) -> Result<Self::Value, A::Error>
    where
        A: MapAccess<'de>,
    {
        drain_map(map)?;
        Ok(FacetsView(Vec::new()))
    }

    fn visit_seq<A>(self, mut seq: A) -> Result<Self::Value, A::Error>
    where
        A: SeqAccess<'de>,
    {
        let mut facets = Vec::new();
        while let Some(facet) = seq.next_element::<FacetView>()? {
            if let Some(facet) = facet.0 {
                facets.push(facet);
            }
        }
        Ok(FacetsView(facets))
    }
}

impl<'de> Deserialize<'de> for FacetsView {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        deserializer.deserialize_any(FacetsVisitor)
    }
}

#[derive(Default)]
struct FacetView(Option<FacetData>);

struct FacetVisitor;

impl<'de> Visitor<'de> for FacetVisitor {
    type Value = Option<FacetData>;

    lenient_scalars!();

    fn visit_map<A>(self, mut map: A) -> Result<Self::Value, A::Error>
    where
        A: MapAccess<'de>,
    {
        let mut byte_start = 0u32;
        let mut byte_end = 0u32;
        let mut has_index = false;
        let mut features = Vec::new();
        while let Some(key) = map.next_key::<std::borrow::Cow<'de, str>>()? {
            match key.as_ref() {
                "index" => {
                    let index = map.next_value::<IndexView>()?;
                    if let (Some(start), Some(end)) = (index.start, index.end) {
                        byte_start = start;
                        byte_end = end;
                        has_index = true;
                    }
                }
                "features" => features = map.next_value::<FeaturesView>()?.0,
                _ => {
                    map.next_value::<IgnoredAny>()?;
                }
            }
        }
        // Mirror the previous DOM lens: facets without a full index are skipped.
        if !has_index {
            return Ok(None);
        }
        Ok(Some(FacetData {
            byte_start,
            byte_end,
            features,
        }))
    }

    fn visit_seq<A>(self, seq: A) -> Result<Self::Value, A::Error>
    where
        A: SeqAccess<'de>,
    {
        drain_seq(seq)?;
        Ok(None)
    }
}

impl<'de> Deserialize<'de> for FacetView {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        deserializer
            .deserialize_any(FacetVisitor)
            .map(FacetView)
    }
}

// ---- index: { "byteStart": N, "byteEnd": N } --------------------------------

#[derive(Default)]
struct IndexView {
    start: Option<u32>,
    end: Option<u32>,
}

struct IndexVisitor;

impl<'de> Visitor<'de> for IndexVisitor {
    type Value = IndexView;

    lenient_scalars!();

    fn visit_map<A>(self, mut map: A) -> Result<Self::Value, A::Error>
    where
        A: MapAccess<'de>,
    {
        let mut start = None;
        let mut end = None;
        while let Some(key) = map.next_key::<std::borrow::Cow<'de, str>>()? {
            match key.as_ref() {
                "byteStart" => start = map.next_value::<LenU32>()?.0,
                "byteEnd" => end = map.next_value::<LenU32>()?.0,
                _ => {
                    map.next_value::<IgnoredAny>()?;
                }
            }
        }
        Ok(IndexView { start, end })
    }

    fn visit_seq<A>(self, seq: A) -> Result<Self::Value, A::Error>
    where
        A: SeqAccess<'de>,
    {
        drain_seq(seq)?;
        Ok(IndexView { start: None, end: None })
    }
}

impl<'de> Deserialize<'de> for IndexView {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        deserializer.deserialize_any(IndexVisitor)
    }
}

/// A u32 that degrades to `None` for any non-integer value.
struct LenU32(Option<u32>);

struct LenU32Visitor;

impl<'de> Visitor<'de> for LenU32Visitor {
    type Value = LenU32;

    fn expecting(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("a JSON value")
    }

    fn visit_unit<E>(self) -> Result<Self::Value, E>
    where
        E: de::Error,
    {
        Ok(LenU32(None))
    }

    fn visit_bool<E>(self, _v: bool) -> Result<Self::Value, E>
    where
        E: de::Error,
    {
        Ok(LenU32(None))
    }

    fn visit_i64<E>(self, v: i64) -> Result<Self::Value, E>
    where
        E: de::Error,
    {
        Ok(LenU32(u32::try_from(v).ok()))
    }

    fn visit_u64<E>(self, v: u64) -> Result<Self::Value, E>
    where
        E: de::Error,
    {
        Ok(LenU32(u32::try_from(v).ok()))
    }

    fn visit_f64<E>(self, _v: f64) -> Result<Self::Value, E>
    where
        E: de::Error,
    {
        Ok(LenU32(None))
    }

    fn visit_str<E>(self, _v: &str) -> Result<Self::Value, E>
    where
        E: de::Error,
    {
        Ok(LenU32(None))
    }

    fn visit_string<E>(self, _v: String) -> Result<Self::Value, E>
    where
        E: de::Error,
    {
        Ok(LenU32(None))
    }

    fn visit_seq<A>(self, seq: A) -> Result<Self::Value, A::Error>
    where
        A: SeqAccess<'de>,
    {
        drain_seq(seq)?;
        Ok(LenU32(None))
    }

    fn visit_map<A>(self, map: A) -> Result<Self::Value, A::Error>
    where
        A: MapAccess<'de>,
    {
        drain_map(map)?;
        Ok(LenU32(None))
    }
}

impl<'de> Deserialize<'de> for LenU32 {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        deserializer.deserialize_any(LenU32Visitor)
    }
}

// ---- features: [ { "$type", tag|uri|did } ] ---------------------------------

#[derive(Default)]
struct FeaturesView(Vec<FeatureData>);

struct FeaturesVisitor;

impl<'de> Visitor<'de> for FeaturesVisitor {
    type Value = FeaturesView;

    lenient_scalars!();

    fn visit_map<A>(self, map: A) -> Result<Self::Value, A::Error>
    where
        A: MapAccess<'de>,
    {
        drain_map(map)?;
        Ok(FeaturesView(Vec::new()))
    }

    fn visit_seq<A>(self, mut seq: A) -> Result<Self::Value, A::Error>
    where
        A: SeqAccess<'de>,
    {
        let mut features = Vec::new();
        while let Some(feature) = seq.next_element::<FeatureView>()? {
            if let Some(feature) = feature.0 {
                features.push(feature);
            }
        }
        Ok(FeaturesView(features))
    }
}

impl<'de> Deserialize<'de> for FeaturesView {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        deserializer.deserialize_any(FeaturesVisitor)
    }
}

struct FeatureVisitor;

/// Marker newtype for lenient feature deserialization (`Option<FeatureData>`
/// has a blanket impl conflict, so a newtype wraps it).
struct FeatureView(Option<FeatureData>);

impl<'de> Visitor<'de> for FeatureVisitor {
    type Value = Option<FeatureData>;

    lenient_scalars!();

    fn visit_map<A>(self, mut map: A) -> Result<Self::Value, A::Error>
    where
        A: MapAccess<'de>,
    {
        let mut feature_type: Option<String> = None;
        let mut tag: Option<String> = None;
        let mut uri: Option<String> = None;
        let mut did: Option<String> = None;
        while let Some(key) = map.next_key::<std::borrow::Cow<'de, str>>()? {
            match key.as_ref() {
                "$type" => feature_type = map.next_value::<LenStr>()?.0,
                "tag" => tag = map.next_value::<LenStr>()?.0,
                "uri" => uri = map.next_value::<LenStr>()?.0,
                "did" => did = map.next_value::<LenStr>()?.0,
                _ => {
                    map.next_value::<IgnoredAny>()?;
                }
            }
        }
        Ok(match feature_type.as_deref() {
            Some("app.bsky.richtext.facet#tag") => tag.map(|tag| FeatureData::Tag { tag }),
            Some("app.bsky.richtext.facet#link") => uri.map(|uri| FeatureData::Link { uri }),
            Some("app.bsky.richtext.facet#mention") => did.map(|did| FeatureData::Mention { did }),
            _ => None,
        })
    }

    fn visit_seq<A>(self, seq: A) -> Result<Self::Value, A::Error>
    where
        A: SeqAccess<'de>,
    {
        drain_seq(seq)?;
        Ok(None)
    }
}

impl<'de> Deserialize<'de> for FeatureView {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        deserializer
            .deserialize_any(FeatureVisitor)
            .map(FeatureView)
    }
}
