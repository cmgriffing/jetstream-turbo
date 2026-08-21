//! Throwaway: measure drop cost of the enriched batch (the unaccounted time in run_batch).
use jetstream_turbo_rs::client::JetstreamClient;
use jetstream_turbo_rs::hydration::{Hydrator, TurboCache};
use jetstream_turbo_rs::testing::{create_message_batch, create_profile, MockPostFetcher, MockProfileFetcher};
use std::sync::Arc;
use std::time::Instant;

fn main() {
    let batch_size: usize = 10_000;
    let rt = tokio::runtime::Runtime::new().unwrap();
    let messages = create_message_batch(batch_size);
    let raw_jsons: Vec<String> = messages.iter().map(|m| serde_json::to_string(m).unwrap()).collect();

    let cache = TurboCache::new(batch_size, batch_size);
    for message in &messages {
        cache.set_user_profile(message.did.to_string(), Arc::new(create_profile(&message.did)));
    }
    let hydrator = Hydrator::new(cache, Arc::new(MockProfileFetcher::new()), Arc::new(MockPostFetcher::new()));
    let client = JetstreamClient::new(vec![], String::new());

    for round in 0..4 {
        let parsed = client.parse_message_batch(raw_jsons.clone()).unwrap();
        let enriched = rt.block_on(hydrator.hydrate_batch(parsed)).unwrap();

        // Phase: encode (mirror)
        let start = Instant::now();
        for record in &enriched {
            let mut buf = Vec::with_capacity(1024);
            record.message.write_json(&mut buf);
            simd_json::to_writer(&mut buf, &record.hydrated_metadata).unwrap();
        }
        let el = start.elapsed();
        if round >= 1 { println!("encode: {:.3} ms", el.as_secs_f64() * 1000.0); }

        // Drop
        let start = Instant::now();
        drop(enriched);
        let el = start.elapsed();
        if round >= 1 { println!("drop enriched (10k): {:.3} ms", el.as_secs_f64() * 1000.0); }
    }

    // Drop cost of parsed-only (messages w/ record DOMs)
    for round in 0..4 {
        let parsed = client.parse_message_batch(raw_jsons.clone()).unwrap();
        let start = Instant::now();
        drop(parsed);
        let el = start.elapsed();
        if round >= 1 { println!("drop parsed msgs (10k): {:.3} ms", el.as_secs_f64() * 1000.0); }
    }
}
