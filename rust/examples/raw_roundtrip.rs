//! Throwaway diagnostic: verify parse→write_json round-trips byte-identically,
//! and measure new encode phase cost.
use jetstream_turbo_rs::client::JetstreamClient;
use jetstream_turbo_rs::hydration::{Hydrator, TurboCache};
use jetstream_turbo_rs::testing::{create_message_batch, create_profile, MockPostFetcher, MockProfileFetcher};
use std::sync::Arc;
use std::time::Instant;

fn main() {
    let batch_size: usize = 10_000;
    let rt = tokio::runtime::Runtime::new().unwrap();
    let messages = create_message_batch(batch_size);
    let raw_jsons: Vec<String> = messages
        .iter()
        .map(|m| serde_json::to_string(m).unwrap())
        .collect();

    // Round-trip: parse captures raw; write_json must reproduce the input bytes.
    let client = JetstreamClient::new(vec![], String::new());
    let parsed = client.parse_message_batch(raw_jsons.clone()).unwrap();
    assert_eq!(parsed.len(), batch_size);
    let mut mismatches = 0;
    for (m, raw) in parsed.iter().zip(&raw_jsons) {
        let mut buf = Vec::new();
        m.write_json(&mut buf);
        if buf.as_slice() != raw.as_bytes() {
            mismatches += 1;
            if mismatches <= 2 {
                println!("MISMATCH:\n  in : {raw}\n  out: {}", String::from_utf8_lossy(&buf));
            }
        }
        assert!(m.raw_json.is_some(), "raw should be captured");
    }
    println!("round-trip mismatches: {mismatches} / {}", batch_size);

    // Full pipeline timing with the new encode path.
    let cache = TurboCache::new(batch_size, batch_size);
    for message in &messages {
        cache.set_user_profile(message.did.to_string(), Arc::new(create_profile(&message.did)));
    }
    let hydrator = Hydrator::new(cache, Arc::new(MockProfileFetcher::new()), Arc::new(MockPostFetcher::new()));
    let client = JetstreamClient::new(vec![], String::new());

    for round in 0..5 {
        let parsed = client.parse_message_batch(raw_jsons.clone()).unwrap();
        let enriched = rt.block_on(hydrator.hydrate_batch(parsed)).unwrap();
        let start = Instant::now();
        let mut bytes = 0usize;
        for record in &enriched {
            let mut buf = Vec::with_capacity(1024);
            record.message.write_json(&mut buf);
            simd_json::to_writer(&mut buf, &record.hydrated_metadata).unwrap();
            bytes += buf.len();
        }
        let el = start.elapsed();
        if round >= 2 {
            println!("encode round {round}: {:.3} ms, {} bytes", el.as_secs_f64() * 1000.0, bytes);
        }
    }

    // Parse cost now includes the raw capture clone.
    let client = JetstreamClient::new(vec![], String::new());
    let start = Instant::now();
    let parsed2 = client.parse_message_batch(raw_jsons.clone()).unwrap();
    let el = start.elapsed();
    println!("parse 10k (with raw capture): {:.3} ms", el.as_secs_f64() * 1000.0);
    println!("sample raw len: {}", parsed2[0].raw_json.as_ref().map(|s| s.len()).unwrap_or(0));
}
