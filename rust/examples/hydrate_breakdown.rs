//! Throwaway diagnostic: break down hydrate_batch phase costs.
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

    let cache = TurboCache::new(batch_size, batch_size);
    for message in &messages {
        cache.set_user_profile(message.did.to_string(), Arc::new(create_profile(&message.did)));
    }
    let hydrator = Hydrator::new(cache.clone(), Arc::new(MockProfileFetcher::new()), Arc::new(MockPostFetcher::new()));
    let client = JetstreamClient::new(vec![], String::new());

    let parsed = client.parse_message_batch(raw_jsons.clone()).unwrap();

    // Detailed: dedup + moka bulk get + map build + per-message mimic
    use ahash::RandomState as AHashState;
    use jetstream_turbo_rs::models::bluesky::BlueskyProfile;
    use std::collections::HashMap;
    use std::collections::HashSet;
    let mut unique_dids: HashSet<Arc<str>, AHashState> = HashSet::with_hasher(AHashState::default());
    let start = Instant::now();
    for m in &parsed {
        unique_dids.insert(Arc::clone(&m.did));
    }
    let el = start.elapsed();
    println!("dedup HashSet: {:.3} ms", el.as_secs_f64() * 1000.0);
    let dids: Vec<Arc<str>> = unique_dids.into_iter().collect();

    let start = Instant::now();
    let profiles = cache.get_user_profiles(&dids);
    let el = start.elapsed();
    println!("moka bulk get (10k): {:.3} ms", el.as_secs_f64() * 1000.0);

    let start = Instant::now();
    let mut profiles_by_did: HashMap<Arc<str>, Arc<BlueskyProfile>, AHashState> =
        HashMap::with_capacity_and_hasher(dids.len(), AHashState::default());
    for profile in profiles.into_iter().flatten() {
        profiles_by_did.insert(Arc::clone(&profile.did), profile);
    }
    let el = start.elapsed();
    println!("profiles_by_did build: {:.3} ms", el.as_secs_f64() * 1000.0);

    let start = Instant::now();
    let mut sink = 0usize;
    for m in &parsed {
        let p = profiles_by_did.get(m.did.as_ref());
        sink += p.map(|_| 1).unwrap_or(0);
    }
    let el = start.elapsed();
    println!("per-message map get (10k): {:.3} ms", el.as_secs_f64() * 1000.0);
    println!("sink {sink}");

    // Phase breakdown across 3 repeats (warmup first)
    for round in 0..4 {
        let parsed = client.parse_message_batch(raw_jsons.clone()).unwrap();

        // extract-refs + context build (what hydrate_batch does first)
        use jetstream_turbo_rs::models::record_view::RecordView;
        let start = Instant::now();
        let mut refs = Vec::with_capacity(batch_size);
        for m in &parsed {
            let r = m.commit.as_ref().and_then(|c| c.record.as_ref()).map(|r| {
                let rv = RecordView::new(r);
                (rv.reply_refs().is_some(), rv.embed_record_uri().is_some(), rv.facets().count())
            });
            refs.push(r);
        }
        let el = start.elapsed();
        if round >= 1 { println!("extract_refs pass: {:.3} ms", el.as_secs_f64() * 1000.0); }

        let start = Instant::now();
        let enriched = rt.block_on(hydrator.hydrate_batch(parsed)).unwrap();
        let el = start.elapsed();
        if round >= 1 { println!("full hydrate: {:.3} ms", el.as_secs_f64() * 1000.0); }
    }
}
