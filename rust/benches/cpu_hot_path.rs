//! Tier 1 — CPU hot-path microbenchmarks.
//!
//! These benchmarks measure the production CPU-bound stages in isolation:
//! simd-json message parsing, `RecordView` reference extraction, simd-json
//! record serialization, and AT-URI string building.

use criterion::{criterion_group, criterion_main, Criterion};
use jetstream_turbo_rs::client::JetstreamClient;
use jetstream_turbo_rs::models::enriched::HydratedMetadata;
use jetstream_turbo_rs::models::jetstream::{
    CommitData, JetstreamMessage, MessageKind, OperationType,
};
use jetstream_turbo_rs::models::jetstream::owned_record;
use jetstream_turbo_rs::models::record_view::{FacetFeature, RecordView};
use jetstream_turbo_rs::testing::create_profile;
use std::hint::black_box;
use std::sync::Arc;

/// A realistic Bluesky post record with reply, embed, and facet references.
fn realistic_record() -> simd_json::OwnedValue {
    owned_record(serde_json::json!({
        "$type": "app.bsky.feed.post",
        "createdAt": "2026-02-13T02:20:02.89585500Z",
        "text": "Hello world #testing with a mention and a link",
        "langs": ["en"],
        "reply": {
            "parent": { "cid": "bafyreiparent", "uri": "at://did:plc:parent123/app.bsky.feed.post/parent456" },
            "root": { "cid": "bafyreiroot", "uri": "at://did:plc:root789/app.bsky.feed.post/root000" }
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

/// A realistic Jetstream commit message wrapping the post record.
fn realistic_message() -> JetstreamMessage {
    JetstreamMessage {
        did: "did:plc:author123".to_string(),
        time_us: Some(1770949213790196),
        seq: Some(100000),
        kind: MessageKind::Commit,
        commit: Some(CommitData {
            rev: Some("3mepgzgimkv23".to_string()),
            operation_type: OperationType::Create,
            collection: Some("app.bsky.feed.post".to_string()),
            rkey: Some("3mepgzgiatv23".to_string()),
            record: Some(realistic_record()),
            cid: Some("bafyreiassbuahzdwy64xwlefqcwh6zk4stb4lhht24oozhxn3fhzomrxg4".to_string()),
        }),
    }
}

/// The raw JSON wire form of the message, as received from the Jetstream socket.
fn realistic_message_json() -> String {
    serde_json::to_string(&realistic_message()).unwrap()
}

/// A realistic hydrated-metadata payload for serialization benchmarks.
fn realistic_metadata() -> HydratedMetadata {
    HydratedMetadata {
        hydration_quality: jetstream_turbo_rs::models::enriched::HydrationQuality::Complete,
        degradation_summaries: vec![],
        author_profile: Some(Arc::new(create_profile("did:plc:author123"))),
        mentioned_profiles: vec![Arc::new(create_profile("did:plc:mentioned"))],
        referenced_posts: vec![],
        hashtags: vec!["testing".to_string()],
        urls: vec!["https://example.com".to_string()],
        mentions: vec![],
        detected_language: Some("en".to_string()),
    }
}

fn bench_parse_message_simd_json(c: &mut Criterion) {
    let raw = realistic_message_json();
    let client = JetstreamClient::new(vec![], String::new());

    c.bench_function("parse_message_simd_json", |b| {
        b.iter(|| {
            // Production path: copies the &str into a String, then parses with simd-json.
            let message = client.parse_message(black_box(raw.as_str())).unwrap();
            black_box(message);
        });
    });
}

fn bench_parse_message_simd_json_owned(c: &mut Criterion) {
    let raw = realistic_message_json();

    c.bench_function("parse_message_simd_json_owned", |b| {
        b.iter(|| {
            // String-by-value variant: the caller already owns the buffer, so no
            // input copy is needed. The clone stands in for that owned buffer.
            let mut owned = raw.clone();
            let message: JetstreamMessage = unsafe { simd_json::from_str(&mut owned).unwrap() };
            black_box(message);
        });
    });
}

fn bench_record_view_extract_refs(c: &mut Criterion) {
    let record = realistic_record();

    c.bench_function("record_view_extract_refs", |b| {
        b.iter(|| {
            let rv = RecordView::new(black_box(&record));
            let reply = rv.reply_refs();
            let embed = rv.embed_record_uri();
            let mut facet_count = 0usize;
            let mut mention_count = 0usize;
            for facet in rv.facets() {
                facet_count += 1;
                for feature in facet.features() {
                    if let FacetFeature::Mention { .. } = feature {
                        mention_count += 1;
                    }
                }
            }
            black_box((reply, embed, facet_count, mention_count));
        });
    });
}

fn bench_simd_json_serialize_record(c: &mut Criterion) {
    let message = realistic_message();
    let metadata = realistic_metadata();

    c.bench_function("simd_json_serialize_record", |b| {
        b.iter(|| {
            // Mirrors the storage path: message and metadata serialized separately.
            let message_json = simd_json::to_string(black_box(&message)).unwrap();
            let metadata_json = simd_json::to_string(black_box(&metadata)).unwrap();
            black_box((message_json, metadata_json));
        });
    });
}

fn bench_extract_at_uri(c: &mut Criterion) {
    let message = realistic_message();

    c.bench_function("extract_at_uri", |b| {
        b.iter(|| {
            let uri = message.extract_at_uri();
            black_box(uri);
        });
    });
}

criterion_group!(
    benches,
    bench_parse_message_simd_json,
    bench_parse_message_simd_json_owned,
    bench_record_view_extract_refs,
    bench_simd_json_serialize_record,
    bench_extract_at_uri,
);
criterion_main!(benches);
