fn main() {
    let msg = r#"{"did":"did:plc:user1234","time_us":1770949213790196,"seq":1234,"kind":"commit","commit":{"rev":"3mepgzgimkv0000","operation":"create","collection":"app.bsky.feed.post","rkey":"3mepgzgia0000","record":{"$type":"app.bsky.feed.post","createdAt":"2026-02-13T02:20:00.895Z","text":"Just shipped a new feature! Really excited about the progress we're making.","langs":["en"]},"cid":"bafyreia"}}"#;
    let mut bytes = msg.as_bytes().to_vec();
    let mut b = simd_json::Buffers::new(bytes.len());
    let nodes;
    {
        let tape = simd_json::to_tape_with_buffers(&mut bytes, &mut b).unwrap();
        nodes = tape.0.len();
    }
    println!("input len: {} tape nodes: {}", bytes.len(), nodes);
}
