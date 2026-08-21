//! Throwaway: what dominates hydrate_one's per-message cost (~180ns/msg)?
use jetstream_turbo_rs::client::JetstreamClient;
use jetstream_turbo_rs::hydration::{Hydrator, TurboCache};
use jetstream_turbo_rs::models::enriched::{EnrichedRecord, HydrationQuality};
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
    let parsed = client.parse_message_batch(raw_jsons.clone()).unwrap();

    // hydrate_one essentials (moved-message model):
    let processed_at = chrono::Utc::now();
    for round in 0..4 {
        let parsed = client.parse_message_batch(raw_jsons.clone()).unwrap();
        let mut enriched = Vec::with_capacity(batch_size);
        let start = Instant::now();
        let mut t = Instant::now();
        for m in parsed {
            let mut e = EnrichedRecord::new_with_timestamp(m, processed_at);
            e.hydrated_metadata.hydration_quality = HydrationQuality::Complete;
            // simulate: span record + instant + map-ish get
            let _ = e.message.extract_did();
            let end = Instant::now();
            e.metrics.hydration_time_ms = end.duration_since(t).as_millis() as u64;
            t = end;
            enriched.push(e);
        }
        let el = start.elapsed();
        if round >= 1 { println!("minimal hydrate_one mimic (no map, no span): {:.3} ms", el.as_secs_f64() * 1000.0); }
    }

    // Same but with the real hydrator for reference
    for round in 0..3 {
        let parsed = client.parse_message_batch(raw_jsons.clone()).unwrap();
        let start = Instant::now();
        let enriched = rt.block_on(hydrator.hydrate_batch(parsed)).unwrap();
        let el = start.elapsed();
        if round >= 1 { println!("full hydrate_batch: {:.3} ms", el.as_secs_f64() * 1000.0); }
    }

    // Span record cost: tracing::Span::current().record() x2 with no subscriber
    for round in 0..3 {
        let parsed = client.parse_message_batch(raw_jsons.clone()).unwrap();
        let start = Instant::now();
        for m in &parsed {
            tracing::Span::current().record("did", m.extract_did());
            tracing::Span::current().record("cache_hit", true);
        }
        let el = start.elapsed();
        if round >= 1 { println!("span records x2 per msg: {:.3} ms", el.as_secs_f64() * 1000.0); }
    }

    // Hash map get with Arc<str> key via Borrow<str> (per message)
    use ahash::RandomState as AHashState;
    use jetstream_turbo_rs::models::bluesky::BlueskyProfile;
    use std::collections::HashMap;
    let mut pmap: HashMap<Arc<str>, Arc<BlueskyProfile>, AHashState> = HashMap::with_capacity_and_hasher(batch_size, AHashState::default());
    for message in &messages {
        pmap.insert(Arc::clone(&message.did), Arc::new(create_profile(&message.did)));
    }
    for round in 0..3 {
        let start = Instant::now();
        let mut n = 0usize;
        for m in &parsed {
            if pmap.get(m.did.as_ref()).is_some() { n += 1; }
        }
        let el = start.elapsed();
        if round >= 1 { println!("map get per msg (10k): {:.3} ms ({n})", el.as_secs_f64() * 1000.0); }
    }
}
