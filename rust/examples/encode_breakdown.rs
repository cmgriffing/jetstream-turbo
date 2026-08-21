//! Throwaway diagnostic: break down the encode phase into message vs metadata
//! serialization, and compare simd_json::to_writer vs serde_json::to_writer.
use jetstream_turbo_rs::client::JetstreamClient;
use jetstream_turbo_rs::hydration::{Hydrator, TurboCache};
use jetstream_turbo_rs::testing::{
    create_message_batch, create_profile, MockPostFetcher, MockProfileFetcher,
};
use std::hint::black_box;
use std::sync::Arc;
use std::time::Instant;

fn main() {
    let batch_size: usize = std::env::var("THROUGHPUT_BATCH_SIZE")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(10_000);

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
    let hydrator = Hydrator::new(cache, Arc::new(MockProfileFetcher::new()), Arc::new(MockPostFetcher::new()));
    let client = JetstreamClient::new(vec![], String::new());

    let parsed = client.parse_message_batch(raw_jsons).unwrap();
    let enriched = rt.block_on(hydrator.hydrate_batch(parsed)).unwrap();

    // Warm up
    for _ in 0..3 {
        let mut total = 0usize;
        for record in &enriched {
            let mut buf = Vec::with_capacity(1024);
            simd_json::to_writer(&mut buf, &record.message).unwrap();
            simd_json::to_writer(&mut buf, &record.hydrated_metadata).unwrap();
            total += buf.len();
        }
        black_box(total);
    }

    // Breakdown: message vs metadata with simd_json
    let fns: Vec<(&str, fn(&jetstream_turbo_rs::models::enriched::EnrichedRecord, &mut Vec<u8>))> = vec![
        ("simd message", |r, buf| {
            simd_json::to_writer(buf, &r.message).unwrap()
        }),
        ("simd metadata", |r, buf| {
            simd_json::to_writer(buf, &r.hydrated_metadata).unwrap()
        }),
    ];
    for (name, f) in fns {
        let start = Instant::now();
        let mut bytes = 0usize;
        for record in &enriched {
            let mut buf = Vec::with_capacity(1024);
            f(record, &mut buf);
            bytes += buf.len();
        }
        let el = start.elapsed();
        println!("{name}: {:.3} ms, {} bytes", el.as_secs_f64() * 1000.0, bytes);
    }

    // Compare serde_json on the same records (message only).
    let start = Instant::now();
    let mut bytes = 0usize;
    for record in &enriched {
        let s = serde_json::to_string(&record.message).unwrap();
        bytes += s.len();
    }
    let el = start.elapsed();
    println!("serde_json message: {:.3} ms, {} bytes", el.as_secs_f64() * 1000.0, bytes);

    let start = Instant::now();
    let mut bytes = 0usize;
    for record in &enriched {
        let s = serde_json::to_string(&record.hydrated_metadata).unwrap();
        bytes += s.len();
    }
    let el = start.elapsed();
    println!("serde_json metadata: {:.3} ms, {} bytes", el.as_secs_f64() * 1000.0, bytes);

    // Record re-encode share: serialize message with record vs without
    let start = Instant::now();
    let mut bytes = 0usize;
    for record in &enriched {
        let mut buf = Vec::with_capacity(1024);
        // copy message without record
        let m = &record.message;
        let mut msg = m.clone();
        if let Some(c) = msg.commit.as_mut() {
            c.record = None;
        }
        simd_json::to_writer(&mut buf, &msg).unwrap();
        bytes += buf.len();
    }
    let el = start.elapsed();
    println!("simd message WITHOUT record: {:.3} ms, {} bytes", el.as_secs_f64() * 1000.0, bytes);

    // ---- Parse breakdown: with vs without building the record DOM ----
    use serde::Deserialize;
    #[derive(Deserialize)]
    struct MsgNoRecord {
        did: String,
        #[serde(default)]
        time_us: Option<u64>,
        #[serde(default)]
        seq: Option<u64>,
        kind: jetstream_turbo_rs::models::jetstream::MessageKind,
        #[serde(default)]
        commit: Option<CommitNoRecord>,
    }
    #[derive(Deserialize)]
    struct CommitNoRecord {
        #[serde(default)]
        rev: Option<String>,
        operation: jetstream_turbo_rs::models::jetstream::OperationType,
        #[serde(default)]
        collection: Option<String>,
        #[serde(default)]
        rkey: Option<String>,
        #[serde(default)]
        cid: Option<String>,
    }

    let raws: Vec<String> = messages
        .iter()
        .map(|m| serde_json::to_string(m).unwrap())
        .collect();

    // warmup
    for _ in 0..3 {
        let mut b = simd_json::Buffers::new(1024);
        for raw in &raws {
            let mut t = raw.clone();
            let _: MsgNoRecord = unsafe { simd_json::serde::from_str_with_buffers(&mut t, &mut b) }.unwrap();
        }
    }

    let start = Instant::now();
    let mut b = simd_json::Buffers::new(1024);
    for raw in &raws {
        let mut t = raw.clone();
        let _: MsgNoRecord = unsafe { simd_json::serde::from_str_with_buffers(&mut t, &mut b) }.unwrap();
    }
    let el = start.elapsed();
    println!("parse WITHOUT record DOM: {:.3} ms", el.as_secs_f64() * 1000.0);

    let start = Instant::now();
    let mut b = simd_json::Buffers::new(1024);
    for raw in &raws {
        let mut t = raw.clone();
        let _: jetstream_turbo_rs::models::jetstream::JetstreamMessage =
            unsafe { simd_json::serde::from_str_with_buffers(&mut t, &mut b) }.unwrap();
    }
    let el = start.elapsed();
    println!("parse WITH record DOM: {:.3} ms", el.as_secs_f64() * 1000.0);

    // Even lighter: parse just the top-level envelope structure (all fields skipped)
    #[derive(Deserialize)]
    struct EnvelopeOnly {
        did: String,
    }
    let start = Instant::now();
    let mut b = simd_json::Buffers::new(1024);
    for raw in &raws {
        let mut t = raw.clone();
        let _: EnvelopeOnly = unsafe { simd_json::serde::from_str_with_buffers(&mut t, &mut b) }.unwrap();
    }
    let el = start.elapsed();
    println!("parse envelope-only: {:.3} ms", el.as_secs_f64() * 1000.0);
}
