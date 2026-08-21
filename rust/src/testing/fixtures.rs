use crate::client::JetstreamClient;
use crate::models::bluesky::BlueskyProfile;
use crate::models::jetstream::JetstreamMessage;
use std::sync::Arc;

/// Parse a wire-form JSON string into a `JetstreamMessage` (capturing
/// `raw_json`), so fixtures behave exactly like messages received from the
/// socket.
fn parse_wire(raw: String) -> JetstreamMessage {
    JetstreamClient::new(vec![], String::new())
        .parse_message(&raw)
        .expect("fixture wire JSON parses")
}

/// Create a realistic Bluesky post creation message.
pub fn create_post_message(index: usize) -> JetstreamMessage {
    let did: Arc<str> = format!("did:plc:user{index:04}").into();
    let rkey = format!("3mepgzgia{index:04}");
    let text = sample_post_text(index);

    let wire = serde_json::json!({
        "did": did,
        "time_us": 1770949213790196 + (index as u64 * 1000),
        "seq": 100000 + index as u64,
        "kind": "commit",
        "commit": {
            "rev": format!("3mepgzgimkv{index:04}"),
            "operation": "create",
            "collection": "app.bsky.feed.post",
            "rkey": rkey,
            "record": {
                "$type": "app.bsky.feed.post",
                "createdAt": format!("2026-02-13T02:20:{:02}.895Z", index % 60),
                "text": text,
                "langs": ["en"]
            },
            "cid": format!("bafyreia{}", &format!("{index:032x}")[..32])
        }
    });
    parse_wire(serde_json::to_string(&wire).expect("fixture wire serializes"))
}

/// Create a post message that includes a reply reference.
pub fn create_reply_message(index: usize, parent_did: &str, parent_rkey: &str) -> JetstreamMessage {
    let did: Arc<str> = format!("did:plc:replier{index:04}").into();
    let rkey = format!("3reply{index:06}");
    let parent_uri = format!("at://{parent_did}/app.bsky.feed.post/{parent_rkey}");

    let wire = serde_json::json!({
        "did": did,
        "time_us": 1770946213800000 + (index as u64 * 1000),
        "seq": 200000 + index as u64,
        "kind": "commit",
        "commit": {
            "rev": format!("3replyrev{index:06}"),
            "operation": "create",
            "collection": "app.bsky.feed.post",
            "rkey": rkey,
            "record": {
                "$type": "app.bsky.feed.post",
                "createdAt": format!("2026-02-13T02:21:{:02}.000Z", index % 60),
                "text": format!("Replying to the post #{}", index),
                "reply": {
                    "parent": {
                        "cid": "bafyreiaparent",
                        "uri": parent_uri
                    },
                    "root": {
                        "cid": "bafyreiaroot",
                        "uri": parent_uri
                    }
                }
            },
            "cid": format!("bafyreireply{index:06}")
        }
    });
    parse_wire(serde_json::to_string(&wire).expect("fixture wire serializes"))
}

/// Create a batch of N realistic post messages.
pub fn create_message_batch(count: usize) -> Vec<JetstreamMessage> {
    (0..count).map(create_post_message).collect()
}

/// Create a realistic BlueskyProfile fixture.
pub fn create_profile(did: &str) -> BlueskyProfile {
    let handle = did
        .strip_prefix("did:plc:")
        .unwrap_or("unknown")
        .to_string();
    BlueskyProfile {
        did: Arc::from(did),
        handle: format!("{handle}.bsky.social"),
        display_name: Some(format!("User {handle}")),
        description: Some("A test user on Bluesky".to_string()),
        avatar: Some("https://cdn.bsky.social/avatar/test.jpg".to_string()),
        banner: None,
        followers_count: Some(42),
        follows_count: Some(100),
        posts_count: Some(256),
        indexed_at: None,
        created_at: None,
        labels: None,
    }
}

fn sample_post_text(index: usize) -> String {
    let texts = [
        "Just shipped a new feature! Really excited about the progress we're making.",
        "The sunset tonight was absolutely beautiful. Nature never disappoints.",
        "Anyone else following the latest developments in AI? Fascinating stuff happening.",
        "Great coffee, good book, perfect morning. What more could you ask for?",
        "Hot take: pineapple on pizza is actually delicious. Fight me.",
        "Just finished reading an incredible book. Highly recommend it!",
        "Working from home today. The cat has claimed half my desk as usual.",
        "New recipe experiment turned out amazing. Homemade pasta from scratch!",
        "The city looks so different at 5am. There's a special kind of peace.",
        "Learning Rust has been one of the best decisions for my career.",
        "Grateful for the amazing community here. You all make this platform special.",
        "Sometimes the simplest solutions are the best ones. Keep it minimal.",
        "Mountain hiking this weekend was exactly what I needed. Fresh air and views.",
        "TIL about a really cool optimization technique for database queries.",
        "Music recommendation: been listening to jazz all week and loving it.",
        "The best debugging technique is explaining the problem to a rubber duck.",
        "Just adopted a rescue dog! Meet Max, the goodest boy.",
        "Conference talk went well today! Thanks everyone who attended.",
        "Rainy day coding session with lo-fi beats. Peak productivity right here.",
        "Reminder: take breaks, drink water, and be kind to yourself.",
        "The stars are incredibly bright tonight. No light pollution out here.",
        "Finally organized my desk. It lasted about 30 minutes before the chaos returned.",
        "Sourdough bread attempt #47. This time it actually rose properly!",
        "Weekend project: built a small weather station with a Raspberry Pi.",
        "Nothing beats the feeling of all tests passing on the first try.",
    ];

    texts[index % texts.len()].to_string()
}
