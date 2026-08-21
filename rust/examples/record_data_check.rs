//! Throwaway: validate the RecordData lenient walker against the simd-json
//! bridge (fixture wire, reply, embed, facets, weird shapes).
use jetstream_turbo_rs::models::jetstream::JetstreamMessage;
use jetstream_turbo_rs::models::record_data::{FeatureData, RecordData};
use jetstream_turbo_rs::testing::create_message_batch;

fn parse_message(raw: &str) -> RecordData {
    let mut t = raw.to_string();
    let mut b = simd_json::Buffers::new(t.len());
    let m: JetstreamMessage = unsafe { simd_json::serde::from_str_with_buffers(&mut t, &mut b) }.unwrap();
    m.commit.unwrap().record.unwrap()
}

fn parse_record(raw: &str) -> RecordData {
    let wrapped = format!(
        r#"{{"did":"d","time_us":1,"seq":1,"kind":"commit","commit":{{"rev":"r","operation":"create","collection":"c","rkey":"k","record":{raw},"cid":"c"}}}}"#
    );
    parse_message(&wrapped)
}

fn main() {
    // 1. bench fixture wire (full message parse)
    let msgs = create_message_batch(3);
    for m in &msgs {
        let mut buf = Vec::new();
        m.write_json(&mut buf);
        let s = String::from_utf8(buf).unwrap();
        let r = parse_message(&s);
        println!("fixture: text={:?} reply={:?}/{:?} embed={:?} facets={}",
            r.text, r.reply_parent_uri, r.reply_root_uri, r.embed_record_uri, r.facets.len());
        assert!(r.text.is_some(), "text extracted");
        assert!(r.reply_parent_uri.is_none() && r.facets.is_empty(), "no refs on plain post");
        assert_eq!(m.raw_json.as_deref().map(str::as_bytes), Some(s.as_bytes()), "raw round-trip");
    }

    // 2. reply record
    let reply = r#"{"$type":"app.bsky.feed.post","createdAt":"2026-01-01","text":"replying","reply":{"parent":{"cid":"a","uri":"at://did:plc:parent/app.bsky.feed.post/p1"},"root":{"cid":"b","uri":"at://did:plc:root/app.bsky.feed.post/r1"}}}"#;
    let r = parse_record(reply);
    assert_eq!(r.reply_parent_uri.as_deref(), Some("at://did:plc:parent/app.bsky.feed.post/p1"));
    assert_eq!(r.reply_root_uri.as_deref(), Some("at://did:plc:root/app.bsky.feed.post/r1"));
    println!("reply ok");

    // 3. embed
    let embed = r#"{"$type":"app.bsky.feed.post","text":"quote","embed":{"$type":"app.bsky.embed.record","record":{"uri":"at://did:plc:emb/app.bsky.feed.post/e1"}}}"#;
    let r = parse_record(embed);
    assert_eq!(r.embed_record_uri.as_deref(), Some("at://did:plc:emb/app.bsky.feed.post/e1"));
    println!("embed ok");

    // 4. facets (mention + tag + link + unknown + malformed)
    let facets = r#"{"$type":"app.bsky.feed.post","text":"hello #t","facets":[
        {"index":{"byteStart":0,"byteEnd":5},"features":[{"$type":"app.bsky.richtext.facet#mention","did":"did:plc:m1"}]},
        {"index":{"byteStart":6,"byteEnd":10},"features":[{"$type":"app.bsky.richtext.facet#tag","tag":"t"},{"$type":"app.bsky.richtext.facet#link","uri":"https://x.com"}]},
        {"index":{"byteStart":11,"byteEnd":12},"features":[{"$type":"app.bsky.richtext.facet#unknown","x":1}]},
        {"index":{},"features":[{"$type":"app.bsky.richtext.facet#tag","tag":"bad"}]}
    ]}"#;
    let r = parse_record(facets);
    println!("facets: {:?}", r.facets);
    assert_eq!(r.facets.len(), 3, "malformed index facet skipped; unknown-type facet kept");
    assert_eq!(r.facets[2].features.len(), 0, "unknown feature filtered");
    assert_eq!(r.facets[0].features.len(), 1);
    assert!(matches!(&r.facets[0].features[0], FeatureData::Mention { did } if did == "did:plc:m1"));
    assert_eq!(r.facets[1].features.len(), 2, "unknown type skipped, tag+link kept");
    println!("facets ok");

    // 5. weird shapes must not fail
    let weird = [
        r#"{"$type":"x","text":123}"#,
        r#"{"text":{"nested":true}}"#,
        r#"{"reply":"string"}"#,
        r#"{"facets":{}}"#,
        r#"{"embed":[1,2]}"#,
    ];
    for w in weird {
        let r = parse_record(w);
        println!("weird ok: {:?}", (r.text, r.reply_parent_uri, r.embed_record_uri, r.facets.len()));
    }
    // record as a scalar value inside the message
    let scalar = r#"{"did":"d","kind":"commit","commit":{"operation":"create","collection":"c","record":"just a string"}}"#;
    let mut t = scalar.to_string();
    let mut b = simd_json::Buffers::new(t.len());
    let m: JetstreamMessage = unsafe { simd_json::serde::from_str_with_buffers(&mut t, &mut b) }.unwrap();
    let r = m.commit.unwrap().record.unwrap();
    assert!(r.text.is_none() && r.facets.is_empty());
    println!("scalar record ok");

    // 6. escaped strings survive
    let esc = r#"{"$type":"app.bsky.feed.post","text":"say \"hi\" #ok"}"#;
    let r = parse_record(esc);
    assert_eq!(r.text.as_deref(), Some("say \"hi\" #ok"));
    println!("escaped ok");

    println!("ALL VALIDATION OK");
}
