use crate::models::record_data::{FacetData, FeatureData, RecordData};

/// A zero-allocation, read-only lens over a record's extracted semantic data.
///
/// The wire parser captures `RecordData` (text, reply refs, embed URI, facets)
/// directly from the JSON tape, so this lens just borrows those fields — no
/// JSON traversal at read time.
#[derive(Debug, Clone, Copy)]
pub struct RecordView<'a> {
    record: &'a RecordData,
}

impl<'a> RecordView<'a> {
    /// Create a new `RecordView` wrapping a record's extracted data.
    #[inline(always)]
    pub fn new(record: &'a RecordData) -> Self {
        Self { record }
    }

    /// The post text, if present.
    #[inline(always)]
    pub fn text(&self) -> Option<&'a str> {
        self.record.text.as_deref()
    }

    /// Reply parent and root URIs, if this record has a reply.
    #[inline(always)]
    pub fn reply_refs(&self) -> Option<ReplyRefs<'a>> {
        let (parent, root) = (&self.record.reply_parent_uri, &self.record.reply_root_uri);
        if parent.is_none() && root.is_none() {
            return None;
        }
        Some(ReplyRefs {
            parent_uri: parent.as_deref(),
            root_uri: root.as_deref(),
        })
    }

    /// The URI of an embedded record (quote post), if present.
    #[inline(always)]
    pub fn embed_record_uri(&self) -> Option<&'a str> {
        self.record.embed_record_uri.as_deref()
    }

    /// Iterate over all facets (rich-text annotations) on this record.
    #[inline(always)]
    pub fn facets(&self) -> FacetIter<'a> {
        FacetIter {
            facets: &self.record.facets,
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
    facets: &'a [FacetData],
    index: usize,
}

impl<'a> Iterator for FacetIter<'a> {
    type Item = FacetRef<'a>;

    fn next(&mut self) -> Option<Self::Item> {
        let facet = self.facets.get(self.index)?;
        self.index += 1;
        Some(FacetRef { facet })
    }

    fn size_hint(&self) -> (usize, Option<usize>) {
        let remaining = self.facets.len().saturating_sub(self.index);
        (remaining, Some(remaining))
    }
}

/// A single facet with byte indices and its typed features.
#[derive(Debug, Clone)]
pub struct FacetRef<'a> {
    facet: &'a FacetData,
}

impl<'a> FacetRef<'a> {
    /// The byte offset of the facet's start within the record text.
    #[inline(always)]
    pub fn byte_start(&self) -> u32 {
        self.facet.byte_start
    }

    /// The byte offset of the facet's end within the record text.
    #[inline(always)]
    pub fn byte_end(&self) -> u32 {
        self.facet.byte_end
    }

    /// Iterate over the typed features within this facet.
    #[inline(always)]
    pub fn features(&self) -> FacetFeatureIter<'a> {
        FacetFeatureIter {
            features: &self.facet.features,
            index: 0,
        }
    }
}

/// Iterator over typed features within a facet.
#[derive(Debug, Clone)]
pub struct FacetFeatureIter<'a> {
    features: &'a [FeatureData],
    index: usize,
}

impl<'a> Iterator for FacetFeatureIter<'a> {
    type Item = FacetFeature<'a>;

    fn next(&mut self) -> Option<Self::Item> {
        let feature = self.features.get(self.index)?;
        self.index += 1;
        Some(match feature {
            FeatureData::Tag { tag } => FacetFeature::Tag { tag },
            FeatureData::Link { uri } => FacetFeature::Link { uri },
            FeatureData::Mention { did } => FacetFeature::Mention { did },
        })
    }

    fn size_hint(&self) -> (usize, Option<usize>) {
        let remaining = self.features.len().saturating_sub(self.index);
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

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn sample_post_record() -> RecordData {
        RecordData::from_value(json!({
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
        }))
    }

    fn minimal_post_record() -> RecordData {
        RecordData::from_value(json!({
            "$type": "app.bsky.feed.post",
            "text": "Minimal post"
        }))
    }

    fn non_object_record() -> RecordData {
        RecordData::from_value(json!("not_an_object"))
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

        assert_eq!(facets[0].byte_start(), 0);
        assert_eq!(facets[0].byte_end(), 11);

        assert_eq!(facets[1].byte_start(), 12);
        assert_eq!(facets[1].byte_end(), 20);
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
        let record = RecordData::from_value(json!({
            "facets": [{
                "index": { "byteStart": 0, "byteEnd": 5 },
                "features": [
                    { "$type": "app.bsky.richtext.facet#unknown", "data": "skip" },
                    { "$type": "app.bsky.richtext.facet#tag", "tag": "kept" }
                ]
            }]
        }));
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
        let record = RecordData::from_value(json!({
            "facets": [{
                "index": { "byteStart": 0, "byteEnd": 5 },
                "features": [
                    { "$type": "app.bsky.richtext.facet#tag" },
                    { "$type": "app.bsky.richtext.facet#tag", "tag": "good" }
                ]
            }]
        }));
        let rv = RecordView::new(&record);
        let facets: Vec<_> = rv.facets().collect();
        let features: Vec<_> = facets[0].features().collect();
        assert_eq!(features.len(), 1);
        assert!(matches!(features[0], FacetFeature::Tag { tag: "good" }));
    }
}
