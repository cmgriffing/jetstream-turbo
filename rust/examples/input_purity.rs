//! Throwaway diagnostic: does simd-json's serde bridge leave the input intact?
//! If yes, capturing the original wire text costs nothing (the input is owned).
use jetstream_turbo_rs::testing::create_message_batch;
use jetstream_turbo_rs::models::jetstream::JetstreamMessage;

fn main() {
    let batch_size: usize = 10_000;
    let messages = create_message_batch(batch_size);
    let raw_jsons: Vec<String> = messages
        .iter()
        .map(|m| serde_json::to_string(m).unwrap())
        .collect();

    // Test 1: does from_str_with_buffers mutate the input?
    let mut t = raw_jsons[0].clone();
    let before = t.clone();
    let mut b = simd_json::Buffers::new(t.len());
    let _m: JetstreamMessage = unsafe { simd_json::serde::from_str_with_buffers(&mut t, &mut b) }.unwrap();
    println!("input unchanged: {}", t == before);
    if t != before {
        println!("len before={} after={}", before.len(), t.len());
        // find first diff
        for (i, (x, y)) in before.bytes().zip(t.bytes()).enumerate() {
            if x != y {
                println!("first diff at {i}: {:?} vs {:?}", before.as_bytes().get(i.saturating_sub(8)..i + 8).map(|s| String::from_utf8_lossy(s)), t.as_bytes().get(i.saturating_sub(8)..i + 8).map(|s| String::from_utf8_lossy(s)));
                break;
            }
        }
        if before.len() != t.len() {
            println!("length differs");
        }
    }

    // Test 2: a message with escaped characters (e.g. a quote in text)
    let mut esc = String::from(r#"{"did":"did:plc:x","time_us":1,"kind":"commit","commit":{"rev":"r","operation":"create","collection":"app.bsky.feed.post","rkey":"k","cid":"c","record":{"$type":"app.bsky.feed.post","text":"say \"hi\""}}}"#);
    let before2 = esc.clone();
    let mut b2 = simd_json::Buffers::new(esc.len());
    let _m2: JetstreamMessage = unsafe { simd_json::serde::from_str_with_buffers(&mut esc, &mut b2) }.unwrap();
    println!("escaped input unchanged: {}", esc == before2);
    if esc != before2 {
        println!("escaped len before={} after={}", before2.len(), esc.len());
        for i in 0..esc.len().min(before2.len()) {
            if before2.as_bytes()[i] != esc.as_bytes()[i] {
                println!("escaped first diff at {i}: {:?}", String::from_utf8_lossy(&before2.as_bytes()[i.saturating_sub(6)..i + 6]));
                println!("               after:    {:?}", String::from_utf8_lossy(&esc.as_bytes()[i.saturating_sub(6)..i + 6]));
                break;
            }
        }
    }
}
