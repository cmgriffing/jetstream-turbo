//! Throwaway diagnostic: moka get vs ahash HashMap get cost.
use jetstream_turbo_rs::models::bluesky::BlueskyProfile;
use jetstream_turbo_rs::testing::{create_message_batch, create_profile};
use ahash::RandomState as AHashState;
use moka::sync::Cache as MokaCache;
use std::collections::HashMap;
use std::sync::Arc;
use std::time::Instant;

fn main() {
    let batch_size: usize = 10_000;
    let messages = create_message_batch(batch_size);

    let profiles: Vec<Arc<BlueskyProfile>> = messages.iter().map(|m| Arc::new(create_profile(&m.did))).collect();

    // moka
    let moka: MokaCache<String, Arc<BlueskyProfile>, AHashState> = MokaCache::builder()
        .max_capacity(batch_size as u64)
        .build_with_hasher(AHashState::default());
    for (m, p) in messages.iter().zip(&profiles) {
        moka.insert(m.did.to_string(), Arc::clone(p));
    }
    let keys: Vec<String> = messages.iter().map(|m| m.did.to_string()).collect();
    let mut sink = 0usize;
    for _ in 0..2 {
        for k in &keys { sink += moka.get(k).map(|_| 1).unwrap_or(0); }
    }
    let start = Instant::now();
    for k in &keys { sink += moka.get(k).map(|_| 1).unwrap_or(0); }
    let el = start.elapsed();
    println!("moka get 10k: {:.3} ms ({:.1} ns/get)", el.as_secs_f64() * 1000.0, el.as_secs_f64() * 1e6);

    // Mutex<HashMap<String>>
    use std::sync::Mutex;
    let mutex_map: Mutex<HashMap<String, Arc<BlueskyProfile>, AHashState>> =
        Mutex::new(HashMap::with_hasher(AHashState::default()));
    for (m, p) in messages.iter().zip(&profiles) {
        mutex_map.lock().unwrap().insert(m.did.to_string(), Arc::clone(p));
    }
    for _ in 0..5 {
        let g = mutex_map.lock().unwrap();
        for k in &keys { sink += g.get(k).map(|_| 1).unwrap_or(0); }
    }
    let start = Instant::now();
    let g = mutex_map.lock().unwrap();
    for k in &keys { sink += g.get(k).map(|_| 1).unwrap_or(0); }
    drop(g);
    let el = start.elapsed();
    println!("Mutex<HashMap> get 10k: {:.3} ms ({:.1} ns/get)", el.as_secs_f64() * 1000.0, el.as_secs_f64() * 1e3);

    // RwLock<HashMap<Arc<str>>> (single-threaded, lock held)
    use std::sync::RwLock;
    let rw: RwLock<HashMap<Arc<str>, Arc<BlueskyProfile>, AHashState>> =
        RwLock::new(HashMap::with_capacity_and_hasher(batch_size, AHashState::default()));
    for (m, p) in messages.iter().zip(&profiles) {
        rw.write().unwrap().insert(Arc::clone(&m.did), Arc::clone(p));
    }
    let r = rw.read().unwrap();
    for _ in 0..5 {
        for m in &messages { sink += r.get(m.did.as_ref()).map(|_| 1).unwrap_or(0); }
    }
    let start = Instant::now();
    for m in &messages { sink += r.get(m.did.as_ref()).map(|_| 1).unwrap_or(0); }
    let el = start.elapsed();
    println!("RwLock<HashMap<Arc>> get 10k: {:.3} ms ({:.1} ns/op)", el.as_secs_f64() * 1000.0, el.as_secs_f64() * 1e3 / batch_size as f64);

    println!("sink {sink}");
}
