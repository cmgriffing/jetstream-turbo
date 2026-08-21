//! Tier 2 — end-to-end CPU throughput harness.
//!
//! Times parse → hydrate (cache-hit) → serialize over a large fixed batch and
//! prints a single `msgs/sec` number. I/O (fetchers, stores) is mocked so only
//! CPU stages are measured.

use jetstream_turbo_rs::client::JetstreamClient;
use jetstream_turbo_rs::hydration::{Hydrator, TurboCache};
use jetstream_turbo_rs::testing::{
    create_message_batch, create_profile, MockPostFetcher, MockProfileFetcher,
};
use std::hint::black_box;
use std::sync::Arc;
use std::time::Instant;

/// Number of timed batches to run; the median `msgs/sec` is reported.
const TIMED_RUNS: usize = 5;

fn main() {
    let batch_size: usize = std::env::var("THROUGHPUT_BATCH_SIZE")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(10_000);

    let rt = tokio::runtime::Runtime::new().expect("failed to build tokio runtime");

    // Build the raw wire-form JSON for each message (the parse input). The
    // pipeline consumes owned buffers (matching production: the socket hands us
    // an owned String), so pre-build one pristine copy per run up front.
    let messages = create_message_batch(batch_size);
    let raw_jsons: Vec<Vec<String>> = (0..=TIMED_RUNS)
        .map(|_| {
            messages
                .iter()
                .map(|m| serde_json::to_string(m).expect("serialize message"))
                .collect()
        })
        .collect();

    // Pre-populate the cache so hydration is a cache hit (no fetcher I/O).
    let cache = TurboCache::new(batch_size, batch_size);
    for message in &messages {
        cache.set_user_profile(
            message.did.to_string(),
            Arc::new(create_profile(&message.did)),
        );
    }

    let profile_fetcher = Arc::new(MockProfileFetcher::new());
    let post_fetcher = Arc::new(MockPostFetcher::new());
    let hydrator = Hydrator::new(cache, profile_fetcher, post_fetcher);
    let client = JetstreamClient::new(vec![], String::new());

    // Warm up once to populate allocators and caches.
    let mut raw_jsons = raw_jsons.into_iter();
    let _ = black_box(run_batch(&rt, &client, &hydrator, raw_jsons.next().unwrap()));

    // Timed runs; report the median to absorb noise. Each run consumes its own
    // pristine copy (no input copy inside the timed region).
    let mut rates = Vec::with_capacity(TIMED_RUNS);
    for _ in 0..TIMED_RUNS {
        let raw = raw_jsons.next().unwrap();
        let start = Instant::now();
        let (records, bytes, parse_ms, hydrate_ms, encode_ms) =
            run_batch(&rt, &client, &hydrator, raw);
        let elapsed = start.elapsed();
        let msgs_per_sec = batch_size as f64 / elapsed.as_secs_f64();
        rates.push(msgs_per_sec);
        println!(
            "run: {:.2} msgs/sec ({} records, {} bytes, {:.2} ms; parse {:.2} ms, hydrate {:.2} ms, encode {:.2} ms)",
            msgs_per_sec,
            records,
            bytes,
            elapsed.as_secs_f64() * 1000.0,
            parse_ms,
            hydrate_ms,
            encode_ms
        );
    }

    rates.sort_by(|a, b| a.partial_cmp(b).unwrap());
    let median = rates[rates.len() / 2];

    println!("batch_size: {batch_size}");
    println!("msgs/sec: {median:.2}");
}

fn run_batch(
    rt: &tokio::runtime::Runtime,
    client: &JetstreamClient,
    hydrator: &Hydrator<MockProfileFetcher, MockPostFetcher>,
    raw_jsons: Vec<String>,
) -> (usize, usize, f64, f64, f64) {
    // Parse (shared simd-json buffers across the batch; consumes the owned
    // buffers like production's socket path).
    let parse_start = Instant::now();
    let parsed = client.parse_message_batch(raw_jsons).expect("parse messages");
    let parse_ms = parse_start.elapsed().as_secs_f64() * 1000.0;

    // Hydrate (cache-hit).
    let hydrate_start = Instant::now();
    let enriched = rt
        .block_on(hydrator.hydrate_batch(parsed))
        .expect("hydrate batch");
    let hydrate_ms = hydrate_start.elapsed().as_secs_f64() * 1000.0;

    // Serialize (message + metadata into one buffer per record, mirroring the
    // storage path).
    let encode_start = Instant::now();
    let mut bytes = 0usize;
    for record in &enriched {
        let mut buf = Vec::with_capacity(1024);
        simd_json::to_writer(&mut buf, &record.message).expect("serialize message");
        let message_end = buf.len();
        simd_json::to_writer(&mut buf, &record.hydrated_metadata).expect("serialize metadata");
        bytes += buf.len();
        black_box((&buf[..message_end], &buf[message_end..]));
    }
    let encode_ms = encode_start.elapsed().as_secs_f64() * 1000.0;

    (enriched.len(), bytes, parse_ms, hydrate_ms, encode_ms)
}
