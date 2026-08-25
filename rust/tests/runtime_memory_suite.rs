use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use jetstream_turbo_rs::client::{BlueskyOperation, HydrationFailure, UpstreamFailureCategory};
use jetstream_turbo_rs::hydration::{Hydrator, TurboCache};
use jetstream_turbo_rs::models::bluesky::BlueskyPost;
use jetstream_turbo_rs::models::enriched::EnrichedRecord;
use jetstream_turbo_rs::storage::{RecordStore, SQLitePragmaConfig, SQLiteStore};
use jetstream_turbo_rs::testing::{
    create_message_batch, create_profile, MockPostFetcher, MockProfileFetcher,
};
use jetstream_turbo_rs::turbocharger::{
    CgroupMemoryDiagnostics, MemoryComponentDiagnostics, MemoryRunArtifact, MemoryRunBaseline,
    MemoryRunComparison, MemoryRunConfiguration, ProcessMemoryBreakdown, RuntimeMemorySample,
    WorkloadPhase,
};
use sqlx::sqlite::{SqliteConnectOptions, SqlitePoolOptions};

const MIB: u64 = 1024 * 1024;

#[tokio::test]
async fn production_shaped_memory_suite_smoke() {
    run_suite(500, 128, "smoke").await;
}

#[tokio::test]
#[ignore = "scheduled/release production-scale memory gate"]
async fn production_scale_memory_suite() {
    let event_volume = std::env::var("MEMORY_SUITE_EVENT_VOLUME")
        .ok()
        .and_then(|value| value.parse().ok())
        .unwrap_or(10_000);
    run_suite(event_volume, 4_096, "production").await;
}

async fn run_suite(event_volume: usize, smoke_cache_entries: usize, scale: &str) {
    let settings = jetstream_turbo_rs::Settings::default();
    let (user_cache_entries, post_cache_entries, negative_cache_entries) = if scale == "production"
    {
        (
            settings.cache_size_users,
            settings.cache_size_posts,
            settings.negative_post_cache_capacity,
        )
    } else {
        (
            smoke_cache_entries,
            smoke_cache_entries,
            smoke_cache_entries,
        )
    };
    let configuration = MemoryRunConfiguration {
        envelope: settings.memory_envelope(),
        user_cache_entries,
        post_cache_entries,
        negative_cache_entries,
        max_concurrent_requests: settings.max_concurrent_requests,
        channel_capacity: settings.channel_capacity,
        max_ingress_event_bytes: settings.max_ingress_event_bytes,
        monitor_broadcast_capacity: settings.monitor_broadcast_capacity,
        in_flight_payload_limit_bytes: settings.in_flight_payload_limit_mb.saturating_mul(MIB),
        sqlite_max_connections: settings.sqlite_max_connections,
        sqlite_cache_bytes_per_connection: u64::from(settings.sqlite_cache_size_kib) * 1024,
        database_size_bytes: settings.max_db_size_mb.saturating_mul(MIB),
        event_volume,
        settling_window_seconds: 60,
        allowed_warmed_growth_bytes: 64 * MIB,
        conservative_working_set_bytes: settings.conservative_memory_working_set_bytes(),
    };
    let started_at_unix_millis = unix_millis();
    let started = Instant::now();
    let temp = tempfile::tempdir().expect("memory suite temp directory");
    let db_path = temp.path().join("runtime-memory.sqlite");
    let pragma = SQLitePragmaConfig {
        cache_size_kib: settings.sqlite_cache_size_kib,
        mmap_size_mb: settings.sqlite_mmap_size_mb,
        journal_size_limit_mb: settings.sqlite_journal_size_limit_mb,
    };
    SQLiteStore::maintain_schema(&db_path, pragma, Duration::from_secs(5))
        .await
        .expect("fixture schema maintenance");
    let store = Arc::new(
        SQLiteStore::new_with_pool_limit(&db_path, pragma, settings.sqlite_max_connections)
            .await
            .expect("fixture SQLite store"),
    );
    let profile_fetcher = Arc::new(MockProfileFetcher::new());
    let post_fetcher = Arc::new(MockPostFetcher::new());
    let cache = TurboCache::new_with_memory_limits(
        user_cache_entries,
        post_cache_entries,
        negative_cache_entries,
        Duration::from_secs(300),
        settings.user_cache_limit_mb.saturating_mul(MIB),
        settings.post_cache_limit_mb.saturating_mul(MIB),
        settings.negative_post_cache_limit_mb.saturating_mul(MIB),
    );
    let hydrator = Hydrator::new(cache.clone(), profile_fetcher.clone(), post_fetcher);
    let mut samples = Vec::new();

    let batch_width = 25;
    let mut checkpoint = 0_u64;
    let mut api_requests = 0_u64;
    let fixture_messages = create_message_batch(event_volume);
    for message in &fixture_messages {
        profile_fetcher
            .add_profile(create_profile(&message.did))
            .await;
    }
    for batch in fixture_messages.chunks(batch_width) {
        let enriched = hydrator
            .hydrate_batch(batch.to_vec())
            .await
            .expect("warm ingestion hydration");
        store
            .store_batch(&enriched)
            .await
            .expect("warm ingestion SQLite write");
        checkpoint = checkpoint.saturating_add(enriched.len() as u64);
        api_requests = profile_fetcher
            .call_count
            .load(std::sync::atomic::Ordering::Relaxed) as u64;
    }
    samples.push(sample(
        WorkloadPhase::LiveIngestion,
        checkpoint,
        &cache,
        &store,
        0,
        0,
    ));

    // Cursor replay uses the same deterministic source records. Duplicate
    // storage semantics and checkpoint monotonicity are exercised separately
    // from the collector lifetime-churn test invoked by the suite script.
    for batch in fixture_messages
        .iter()
        .take(event_volume.min(250))
        .collect::<Vec<_>>()
        .chunks(batch_width)
    {
        let replay = batch
            .iter()
            .map(|message| (*message).clone())
            .collect::<Vec<_>>();
        let _ = hydrator
            .hydrate_batch(replay)
            .await
            .expect("cursor replay hydration");
    }
    samples.push(sample(
        WorkloadPhase::Replay,
        checkpoint,
        &cache,
        &store,
        0,
        0,
    ));

    // Fill and evict the hydration cache beyond both count and byte working sets.
    for wave in 0..1 {
        for index in 0..user_cache_entries.saturating_add(16) {
            let did = format!("did:plc:memory-{wave}-{index}");
            cache.set_user_profile(did.clone(), Arc::new(create_profile(&did)));
        }
    }
    for index in 0..post_cache_entries.saturating_add(16) {
        let uri = format!("at://did:plc:memory/app.bsky.feed.post/{index}");
        cache.set_post(
            uri.clone(),
            Arc::new(BlueskyPost {
                uri,
                cid: format!("cid-{index}"),
                author: create_profile("did:plc:memory-author"),
                text: "bounded production cache fixture".repeat(4),
                created_at: chrono::Utc::now(),
                embed: None,
                reply: None,
                facets: None,
                labels: None,
                like_count: Some(0),
                repost_count: Some(0),
                reply_count: Some(0),
            }),
        );
    }
    for index in 0..negative_cache_entries.saturating_add(16) {
        cache.set_unavailable_post(
            format!("at://did:plc:memory/app.bsky.feed.post/unavailable-{index}"),
            HydrationFailure {
                operation: BlueskyOperation::Posts,
                category: UpstreamFailureCategory::ServerError,
                status_class: Some("5xx".to_string()),
                attempts: 1,
                request_fingerprint: "memory-suite".to_string(),
                isolation: None,
            },
        );
    }
    let filled_cache = cache.memory_snapshot();
    assert_eq!(filled_cache.user_entries, user_cache_entries);
    assert_eq!(filled_cache.post_entries, post_cache_entries);
    assert_eq!(filled_cache.negative_post_entries, negative_cache_entries);
    assert!(filled_cache.user_evictions > 0);
    assert!(filled_cache.post_evictions > 0);
    assert!(filled_cache.negative_post_evictions > 0);
    samples.push(sample(
        WorkloadPhase::LiveIngestion,
        checkpoint,
        &cache,
        &store,
        0,
        0,
    ));

    // Once all caches are warm, churn another bounded wave. The evaluator
    // compares only these fully-warmed points, excluding one-time fill growth.
    for index in 0..1_024 {
        let did = format!("did:plc:warmed-churn-{index}");
        cache.set_user_profile(did.clone(), Arc::new(create_profile(&did)));
        let uri = format!("at://did:plc:warmed/app.bsky.feed.post/{index}");
        cache.set_post(
            uri.clone(),
            Arc::new(BlueskyPost {
                uri,
                cid: format!("warmed-cid-{index}"),
                author: create_profile("did:plc:warmed-author"),
                text: "bounded warmed churn".repeat(4),
                created_at: chrono::Utc::now(),
                embed: None,
                reply: None,
                facets: None,
                labels: None,
                like_count: Some(0),
                repost_count: Some(0),
                reply_count: Some(0),
            }),
        );
        cache.set_unavailable_post(
            format!("at://did:plc:warmed/app.bsky.feed.post/unavailable-{index}"),
            HydrationFailure {
                operation: BlueskyOperation::Posts,
                category: UpstreamFailureCategory::ServerError,
                status_class: Some("5xx".to_string()),
                attempts: 1,
                request_fingerprint: "memory-suite-warmed".to_string(),
                isolation: None,
            },
        );
    }
    samples.push(sample(
        WorkloadPhase::LiveIngestion,
        checkpoint,
        &cache,
        &store,
        0,
        0,
    ));

    // Hold a separate IMMEDIATE transaction while a production store write is
    // waiting. Sampling occurs during the wait to prove phase evidence does not
    // depend on ingestion or SQLite progress.
    let lock_pool = SqlitePoolOptions::new()
        .max_connections(1)
        .connect_with(
            SqliteConnectOptions::new()
                .filename(&db_path)
                .create_if_missing(false),
        )
        .await
        .expect("contention lock pool");
    let mut lock = lock_pool.acquire().await.expect("contention connection");
    sqlx::query("BEGIN IMMEDIATE")
        .execute(&mut *lock)
        .await
        .expect("contention transaction");
    let blocked_store = Arc::clone(&store);
    let blocked_records = vec![EnrichedRecord::new(
        fixture_messages
            .first()
            .expect("fixture has records")
            .clone(),
    )];
    let blocked = tokio::spawn(async move { blocked_store.store_batch(&blocked_records).await });
    tokio::time::sleep(Duration::from_millis(50)).await;
    samples.push(sample(
        WorkloadPhase::DatabaseContention,
        checkpoint,
        &cache,
        &store,
        1,
        1,
    ));
    sqlx::query("ROLLBACK")
        .execute(&mut *lock)
        .await
        .expect("release contention transaction");
    drop(lock);
    lock_pool.close().await;
    blocked
        .await
        .expect("blocked task joins")
        .expect("blocked write resumes");
    checkpoint = checkpoint.saturating_add(1);
    samples.push(sample(
        WorkloadPhase::DatabaseContention,
        checkpoint,
        &cache,
        &store,
        0,
        0,
    ));

    let max_size = settings.max_db_size_mb.saturating_mul(MIB) as i64;
    store
        .cleanup_with_vacuum(0, max_size, 0.0, 500, 0)
        .await
        .expect("bounded cleanup phase");
    samples.push(sample(
        WorkloadPhase::Cleanup,
        checkpoint,
        &cache,
        &store,
        0,
        0,
    ));
    store.run_vacuum(max_size).await.expect("VACUUM phase");
    samples.push(sample(
        WorkloadPhase::Vacuum,
        checkpoint,
        &cache,
        &store,
        0,
        0,
    ));

    cache.clear();
    samples.push(sample(
        WorkloadPhase::LiveIngestion,
        checkpoint,
        &cache,
        &store,
        0,
        0,
    ));
    let elapsed = started.elapsed().as_secs_f64().max(0.001);
    let throughput = event_volume as f64 / elapsed;
    for sample in &mut samples {
        sample.throughput_per_second = throughput;
    }
    let evaluation = MemoryRunArtifact::evaluate(&configuration, &samples);
    let mut baseline: MemoryRunBaseline = serde_json::from_str(include_str!(
        "fixtures/runtime-memory-pre-change-baseline.json"
    ))
    .expect("valid checked-in pre-change baseline");
    baseline.bluesky_api_requests = baseline
        .bluesky_api_requests
        .saturating_mul(u64::try_from(event_volume).unwrap_or(u64::MAX))
        .div_ceil(500);
    let candidate = MemoryRunBaseline {
        throughput_per_second: throughput,
        committed_lag_us: Some(0),
        bluesky_api_requests: api_requests,
        hydration_complete_ratio: 1.0,
    };
    let comparison = MemoryRunComparison::compare(&baseline, candidate);
    let attribution = MemoryRunArtifact::attribution(&configuration, &samples);
    let artifact = MemoryRunArtifact {
        schema_version: 1,
        run_id: format!("runtime-memory-{scale}-{started_at_unix_millis}"),
        started_at_unix_millis,
        completed_at_unix_millis: unix_millis(),
        configuration,
        baseline: Some(baseline),
        comparison: Some(comparison),
        attribution,
        samples,
        evaluation,
    };
    let artifact_path = artifact_path(scale, temp.path());
    if let Some(parent) = artifact_path.parent() {
        std::fs::create_dir_all(parent).expect("artifact directory");
    }
    artifact
        .write_json(&artifact_path)
        .expect("memory artifact");
    eprintln!("runtime_memory_artifact={}", artifact_path.display());
    assert!(
        artifact.evaluation.passed,
        "memory suite failures: {:?}",
        artifact.evaluation.failures
    );
    assert_eq!(
        artifact
            .attribution
            .confirmed_collector_retained_bytes_after_settle,
        0,
        "collector coordination is verified separately and must settle to zero"
    );
}

fn sample(
    phase: WorkloadPhase,
    checkpoint: u64,
    cache: &TurboCache,
    store: &SQLiteStore,
    queued_batches: usize,
    running_batches: usize,
) -> RuntimeMemorySample {
    let cache = cache.memory_snapshot();
    let pool = store.pool_memory_snapshot();
    RuntimeMemorySample {
        captured_at_unix_millis: unix_millis(),
        phase,
        process: ProcessMemoryBreakdown::collect(),
        cgroup: CgroupMemoryDiagnostics::collect(),
        components: MemoryComponentDiagnostics {
            user_cache_entries: cache.user_entries,
            user_cache_entry_limit: cache.user_entry_limit,
            user_cache_evictions: cache.user_evictions,
            user_cache_bytes: cache.user_bytes,
            user_cache_limit_bytes: cache.user_limit_bytes,
            post_cache_entries: cache.post_entries,
            post_cache_entry_limit: cache.post_entry_limit,
            post_cache_evictions: cache.post_evictions,
            post_cache_bytes: cache.post_bytes,
            post_cache_limit_bytes: cache.post_limit_bytes,
            negative_cache_entries: cache.negative_post_entries,
            negative_cache_entry_limit: cache.negative_post_entry_limit,
            negative_cache_evictions: cache.negative_post_evictions,
            negative_cache_bytes: cache.negative_post_bytes,
            negative_cache_limit_bytes: cache.negative_post_limit_bytes,
            coordination_bytes: 0,
            input_channel_bytes: 0,
            input_channel_limit_bytes: 1_000 * 256 * 1024,
            in_flight_payload_bytes: (running_batches as u64) * 25 * MIB,
            in_flight_payload_limit_bytes: 256 * MIB,
            monitor_broadcast_bytes: 0,
            monitor_broadcast_limit_bytes: 32 * 2 * MIB,
            sqlx_connections: pool.size,
            sqlx_idle_connections: pool.idle,
            sqlx_max_connections: pool.max_connections,
            sqlite_cache_bytes_per_connection: pool.cache_bytes_per_connection,
            sqlite_mmap_bytes: pool.mmap_limit_bytes,
            sqlite_temp_store: pool.temp_store.to_string(),
        },
        checkpoint_ordinal: Some(checkpoint),
        queued_batches,
        running_batches,
        active_permits: running_batches,
        maximum_permits: 6,
        ..RuntimeMemorySample::default()
    }
}

fn artifact_path(scale: &str, fallback: &std::path::Path) -> PathBuf {
    std::env::var_os("MEMORY_ARTIFACT_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(|| fallback.to_path_buf())
        .join(format!("runtime-memory-{scale}.json"))
}

fn unix_millis() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or(Duration::ZERO)
        .as_millis()
        .try_into()
        .unwrap_or(u64::MAX)
}
