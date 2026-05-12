use serde_json::Value;

/// A zero-allocation, read-only lens over a Bluesky record's raw JSON.
///
/// Exposes semantic accessors for facets, reply references, embed URIs, and text
/// without duplicating JSON traversal across callers.
#[derive(Debug, Clone, Copy)]
pub struct RecordView<'a> {
    record: &'a Value,
}

impl<'a> RecordView<'a> {
    /// Create a new `RecordView` wrapping a record JSON value.
    #[inline(always)]
    pub fn new(record: &'a Value) -> Self {
        Self { record }
    }

    /// The post text, if present.
    #[inline(always)]
    pub fn text(&self) -> Option<&'a str> {
        self.record.get("text")?.as_str()
    }

    /// Reply parent and root URIs, if this record has a reply.
    #[inline(always)]
    pub fn reply_refs(&self) -> Option<ReplyRefs<'a>> {
        let reply = self.record.get("reply")?;
        Some(ReplyRefs {
            parent_uri: reply
                .get("parent")
                .and_then(|p| p.get("uri"))
                .and_then(|v| v.as_str()),
            root_uri: reply
                .get("root")
                .and_then(|r| r.get("uri"))
                .and_then(|v| v.as_str()),
        })
    }

    /// The URI of an embedded record (quote post), if present.
    #[inline(always)]
    pub fn embed_record_uri(&self) -> Option<&'a str> {
        self.record
            .get("embed")?
            .get("record")?
            .get("uri")?
            .as_str()
    }

    /// Iterate over all facets (rich-text annotations) on this record.
    #[inline(always)]
    pub fn facets(&self) -> FacetIter<'a> {
        FacetIter {
            facets: self.record.get("facets").and_then(|v| v.as_array()),
            index: 0,
        }
    }
}

/// Borrowed reply reference URIs extracted from a record.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct ReplyRefs<'a> {
    pub parent_uri: Option<&'a str>,
    pub root_uri: Option<&'a str>,
}

/// Iterator over facets in a record.
#[derive(Debug, Clone)]
pub struct FacetIter<'a> {
    facets: Option<&'a Vec<Value>>,
    index: usize,
}

impl<'a> Iterator for FacetIter<'a> {
    type Item = FacetRef<'a>;

    fn next(&mut self) -> Option<Self::Item> {
        let facets = self.facets?;
        if self.index >= facets.len() {
            return None;
        }
        let facet = &facets[self.index];
        self.index += 1;

        let index = facet.get("index")?;
        let byte_start = index.get("byteStart")?.as_u64()? as u32;
        let byte_end = index.get("byteEnd")?.as_u64()? as u32;

        Some(FacetRef {
            byte_start,
            byte_end,
            features: facet.get("features").and_then(|f| f.as_array()),
        })
    }

    fn size_hint(&self) -> (usize, Option<usize>) {
        let remaining = self
            .facets
            .map(|f| f.len().saturating_sub(self.index))
            .unwrap_or(0);
        (remaining, Some(remaining))
    }
}

/// A single facet with byte indices and its typed features.
#[derive(Debug, Clone)]
pub struct FacetRef<'a> {
    pub byte_start: u32,
    pub byte_end: u32,
    features: Option<&'a Vec<Value>>,
}

impl<'a> FacetRef<'a> {
    /// Iterate over the typed features within this facet.
    #[inline(always)]
    pub fn features(&self) -> FacetFeatureIter<'a> {
        FacetFeatureIter {
            features: self.features,
            index: 0,
        }
    }
}

/// Iterator over typed features within a facet.
#[derive(Debug, Clone)]
pub struct FacetFeatureIter<'a> {
    features: Option<&'a Vec<Value>>,
    index: usize,
}

impl<'a> Iterator for FacetFeatureIter<'a> {
    type Item = FacetFeature<'a>;

    fn next(&mut self) -> Option<Self::Item> {
        let features = self.features?;
        while self.index < features.len() {
            let feature = &features[self.index];
            self.index += 1;
            if let Some(parsed) = parse_facet_feature(feature) {
                return Some(parsed);
            }
        }
        None
    }

    fn size_hint(&self) -> (usize, Option<usize>) {
        let remaining = self
            .features
            .map(|f| f.len().saturating_sub(self.index))
            .unwrap_or(0);
        (0, Some(remaining))
    }
}

/// A typed rich-text feature within a facet.
#[derive(Debug, Clone, Copy)]
pub enum FacetFeature<'a> {
    Tag { tag: &'a str },
    Link { uri: &'a str },
    Mention { did: &'a str },
}

/// Parse a single feature JSON value into a typed `FacetFeature`.
/// Returns `None` for unknown or malformed feature types.
fn parse_facet_feature(feature: &Value) -> Option<FacetFeature<'_>> {
    let feature_type = feature.get("$type")?.as_str()?;
    match feature_type {
        "app.bsky.richtext.facet#tag" => {
            let tag = feature.get("tag")?.as_str()?;
            Some(FacetFeature::Tag { tag })
        }
        "app.bsky.richtext.facet#link" => {
            let uri = feature.get("uri")?.as_str()?;
            Some(FacetFeature::Link { uri })
        }
        "app.bsky.richtext.facet#mention" => {
            let did = feature.get("did")?.as_str()?;
            Some(FacetFeature::Mention { did })
        }
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn sample_post_record() -> Value {
        json!({
            "$type": "app.bsky.feed.post",
            "createdAt": "2026-02-13T02:20:02.89585500Z",
            "text": "Hello world #testing",
            "reply": {
                "parent": { "cid": "bafyrei", "uri": "at://did:plc:parent123/app.bsky.feed.post/parent456" },
                "root": { "cid": "bafyrei", "uri": "at://did:plc:root789/app.bsky.feed.post/root000" }
            },
            "embed": {
                "$type": "app.bsky.embed.record",
                "record": { "uri": "at://did:plc:embed000/app.bsky.feed.post/embed111" }
            },
            "facets": [
                {
                    "index": { "byteStart": 0, "byteEnd": 11 },
                    "features": [
                        { "$type": "app.bsky.richtext.facet#tag", "tag": "Hello" },
                        { "$type": "app.bsky.richtext.facet#link", "uri": "https://example.com" }
                    ]
                },
                {
                    "index": { "byteStart": 12, "byteEnd": 20 },
                    "features": [
                        { "$type": "app.bsky.richtext.facet#mention", "did": "did:plc:mentioned" }
                    ]
                }
            ]
        })
    }

    fn minimal_post_record() -> Value {
        json!({
            "$type": "app.bsky.feed.post",
            "text": "Minimal post"
        })
    }

    fn non_object_record() -> Value {
        json!("not_an_object")
    }

    #[test]
    fn text_returns_post_text() {
        let record = sample_post_record();
        let rv = RecordView::new(&record);
        assert_eq!(rv.text(), Some("Hello world #testing"));
    }

    #[test]
    fn text_returns_none_when_missing() {
        let record = minimal_post_record();
        let rv = RecordView::new(&record);
        assert_eq!(rv.text(), Some("Minimal post"));
    }

    #[test]
    fn reply_refs_extracts_both_uris() {
        let record = sample_post_record();
        let rv = RecordView::new(&record);
        let refs = rv.reply_refs().expect("should have reply refs");
        assert_eq!(
            refs.parent_uri,
            Some("at://did:plc:parent123/app.bsky.feed.post/parent456")
        );
        assert_eq!(
            refs.root_uri,
            Some("at://did:plc:root789/app.bsky.feed.post/root000")
        );
    }

    #[test]
    fn reply_refs_returns_none_when_no_reply() {
        let record = minimal_post_record();
        let rv = RecordView::new(&record);
        assert_eq!(rv.reply_refs(), None);
    }

    #[test]
    fn embed_record_uri_extracts_uri() {
        let record = sample_post_record();
        let rv = RecordView::new(&record);
        assert_eq!(
            rv.embed_record_uri(),
            Some("at://did:plc:embed000/app.bsky.feed.post/embed111")
        );
    }

    #[test]
    fn embed_record_uri_returns_none_when_no_embed() {
        let record = minimal_post_record();
        let rv = RecordView::new(&record);
        assert_eq!(rv.embed_record_uri(), None);
    }

    #[test]
    fn facets_iterates_all_facets() {
        let record = sample_post_record();
        let rv = RecordView::new(&record);
        let facets: Vec<_> = rv.facets().collect();
        assert_eq!(facets.len(), 2);

        assert_eq!(facets[0].byte_start, 0);
        assert_eq!(facets[0].byte_end, 11);

        assert_eq!(facets[1].byte_start, 12);
        assert_eq!(facets[1].byte_end, 20);
    }

    #[test]
    fn facets_returns_empty_when_no_facets() {
        let record = minimal_post_record();
        let rv = RecordView::new(&record);
        assert_eq!(rv.facets().count(), 0);
    }

    #[test]
    fn facet_features_parses_tag_link_mention() {
        let record = sample_post_record();
        let rv = RecordView::new(&record);
        let facets: Vec<_> = rv.facets().collect();

        let features0: Vec<_> = facets[0].features().collect();
        assert_eq!(features0.len(), 2);
        assert!(matches!(features0[0], FacetFeature::Tag { tag: "Hello" }));
        assert!(matches!(
            features0[1],
            FacetFeature::Link {
                uri: "https://example.com"
            }
        ));

        let features1: Vec<_> = facets[1].features().collect();
        assert_eq!(features1.len(), 1);
        assert!(matches!(
            features1[0],
            FacetFeature::Mention {
                did: "did:plc:mentioned"
            }
        ));
    }

    #[test]
    fn facet_features_skips_unknown_types() {
        let record = json!({
            "facets": [{
                "index": { "byteStart": 0, "byteEnd": 5 },
                "features": [
                    { "$type": "app.bsky.richtext.facet#unknown", "data": "skip" },
                    { "$type": "app.bsky.richtext.facet#tag", "tag": "kept" }
                ]
            }]
        });
        let rv = RecordView::new(&record);
        let facets: Vec<_> = rv.facets().collect();
        let features: Vec<_> = facets[0].features().collect();
        assert_eq!(features.len(), 1);
        assert!(matches!(features[0], FacetFeature::Tag { tag: "kept" }));
    }

    #[test]
    fn record_view_handles_non_object_value() {
        let record = non_object_record();
        let rv = RecordView::new(&record);
        assert_eq!(rv.text(), None);
        assert_eq!(rv.reply_refs(), None);
        assert_eq!(rv.embed_record_uri(), None);
        assert_eq!(rv.facets().count(), 0);
    }

    #[test]
    fn facet_features_skips_malformed_features() {
        let record = json!({
            "facets": [{
                "index": { "byteStart": 0, "byteEnd": 5 },
                "features": [
                    { "$type": "app.bsky.richtext.facet#tag" },
                    { "$type": "app.bsky.richtext.facet#tag", "tag": "good" }
                ]
            }]
        });
        let rv = RecordView::new(&record);
        let facets: Vec<_> = rv.facets().collect();
        let features: Vec<_> = facets[0].features().collect();
        assert_eq!(features.len(), 1);
        assert!(matches!(features[0], FacetFeature::Tag { tag: "good" }));
    }
}
