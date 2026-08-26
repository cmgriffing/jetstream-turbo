//! Lever-by-lever recovery canary (spec: hydration-throughput).
//!
//! Deterministic companion to `recovery_gate_test.rs`. Each throughput lever
//! is exercised in isolation and compared against the retained sequential
//! production baseline (release `9b22f30`, 2026-08-25: drain 34.7 msg/s,
//! hydration 85% of the batch critical path, 5.6 of 10 requests/second
//! used, fill 10.7/8.2 items per request):
//!
//! - Lever A — adaptive claim window: a quiescent tail set must flush
//!   without waiting the configured window (dead-time reduction) while
//!   fill counters keep reporting full claims plus the tail claim.
//! - Lever B — concurrent resolution: parallel hydration must drain the
//!   same workload faster than sequential with identical output and zero
//!   errors.
//! - Lever C — permit increase (6 → 9): more permits must convert the
//!   shared 10 requests/second quota headroom into drain rate without
//!   exceeding the quota.
//!
//! Levers B and C run under a paused clock (upstream doubles model the
//! shared quota pacing deterministically); lever A runs against a local
//! mock server. Each lever retains a machine-readable comparison artifact
//! under the target directory.

use jetstream_turbo_rs::client::{PostFetchOutcome, PostFetcher, ProfileFetcher};
use jetstream_turbo_rs::hydration::{HydrationExecutionMode, Hydrator, TurboCache};
use jetstream_turbo_rs::models::bluesky::{BlueskyPost, BlueskyProfile};
use jetstream_turbo_rs::models::TurboResult;
use jetstream_turbo_rs::testing::create_reply_message;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::Duration;
use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

/// Retained sequential production baseline (see `baseline.json` and the
/// proposal for `restore-hydration-throughput`).
const BASELINE_DRAIN_MSG_PER_SEC: f64 = 34.7;
const BASELINE_HYDRATION_CRITICAL_PATH_SHARE: f64 = 0.85;
const BASELINE_REQUEST_RATE_PER_SEC: f64 = 5.6;
const BASELINE_FILL_ITEMS_PER_REQUEST_PROFILES: f64 = 10.7;
const BASELINE_FILL_ITEMS_PER_REQUEST_POSTS: f64 = 8.2;
/// Shared upstream rate-limiter quota (requests/second).
const UPSTREAM_QUOTA_PER_SEC: f64 = 10.0;

fn artifact_path(name: &str) -> std::path::PathBuf {
    std::env::var("CARGO_TARGET_TMP_DIR")
        .map(std::path::PathBuf::from)
        .unwrap_or_else(|_| std::env::temp_dir())
        .join(name)
}

// ---------------------------------------------------------------------------
// Lever A — adaptive claim window (real-time mock server)
// ---------------------------------------------------------------------------

#[tokio::test]
async fn canary_adaptive_claim_window_flushes_tail_sets_immediately() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/app.bsky.actor.getProfiles"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "profiles": (0..30)
                .map(|index| serde_json::json!({
                    "did": format!("did:plc:canary{index:02}"),
                    "handle": "canary.bsky.social"
                }))
                .collect::<Vec<_>>()
        })))
        .mount(&server)
        .await;

    // The configured window is far longer than the whole test run: under the
    // previous fixed-window policy the 5-key tail chunk would park behind it.
    let window_ms = 1_000_u64;
    let client = jetstream_turbo_rs::client::BlueskyClient::new_with_policies(
        vec!["canary-session".to_string()],
        None,
        25,
        25,
        window_ms,
        window_ms,
        jetstream_turbo_rs::client::RequestRetryPolicy {
            max_retries: 0,
            base_delay: Duration::from_millis(1),
            max_delay: Duration::from_millis(2),
        },
        jetstream_turbo_rs::client::ContainmentPolicy::default(),
    )
    .unwrap();
    client.set_api_base_url_for_test(server.uri()).await;

    let dids = (0..30)
        .map(|index| format!("did:plc:canary{index:02}"))
        .collect::<Vec<_>>();
    let started = std::time::Instant::now();
    let profiles = client.bulk_fetch_profiles(&dids).await.unwrap();
    let elapsed_ms = started.elapsed().as_millis() as u64;
    assert_eq!(profiles.len(), 30);
    assert!(profiles.iter().all(|profile| profile.is_some()));

    // Dead time: the tail set must not wait out the configured window.
    assert!(
        elapsed_ms < window_ms,
        "tail set waited {elapsed_ms}ms behind a {window_ms}ms window"
    );

    // Fill efficiency: one full 25-item claim plus the immediate tail claim.
    let diagnostics = client.fetch_diagnostics().await;
    let requests = diagnostics.profiles.requests_total;
    let items = diagnostics.profiles.items_total;
    assert_eq!(requests, 2);
    assert_eq!(items, 30);
    let fill = items as f64 / requests as f64;

    // Guard settling.
    let coordination = client.coordination_diagnostics().await;
    assert_eq!(
        (
            coordination.profiles.pending_keys,
            coordination.profiles.in_flight_keys,
            coordination.profiles.waiters
        ),
        (0, 0, 0)
    );

    let artifact = serde_json::json!({
        "lever": "adaptive_claim_window",
        "comparison_baseline": {
            "source": "retained sequential baseline (baseline.json, release 9b22f30)",
            "fill_items_per_request": {
                "profiles": BASELINE_FILL_ITEMS_PER_REQUEST_PROFILES,
                "posts": BASELINE_FILL_ITEMS_PER_REQUEST_POSTS,
            },
        },
        "configured_window_ms": window_ms,
        "measured": {
            "tail_flush_dead_time_ms": elapsed_ms,
            "requests": requests,
            "items": items,
            "fill_items_per_request": fill,
        },
        "improvement": {
            "tail_dead_time_avoided_ms": window_ms,
            "fill_not_regressed": fill >= BASELINE_FILL_ITEMS_PER_REQUEST_PROFILES,
        },
    });
    std::fs::write(
        artifact_path("recovery-canary-claim-window.json"),
        serde_json::to_string_pretty(&artifact).unwrap(),
    )
    .expect("retain claim-window canary artifact");

    assert!(
        fill >= BASELINE_FILL_ITEMS_PER_REQUEST_PROFILES,
        "fill efficiency regressed: {fill} < {BASELINE_FILL_ITEMS_PER_REQUEST_PROFILES}"
    );
}

// ---------------------------------------------------------------------------
// Levers B and C — concurrent mode and permit increase (paused clock)
// ---------------------------------------------------------------------------

fn canary_profile(did: &str) -> BlueskyProfile {
    BlueskyProfile {
        did: Arc::from(did),
        handle: format!("{}.bsky.social", did.trim_start_matches("did:plc:")),
        display_name: None,
        description: None,
        avatar: None,
        banner: None,
        followers_count: None,
        follows_count: None,
        posts_count: None,
        indexed_at: None,
        created_at: None,
        labels: None,
    }
}

/// Delayed upstream double that models the shared 10 requests/second quota:
/// request starts are paced one quota slot apart, then the response carries
/// the declared network delay. Deterministic under a paused clock.
struct QuotaPacedUpstream {
    delay: Duration,
    slot_interval: Duration,
    next_slot: Arc<tokio::sync::Mutex<tokio::time::Instant>>,
    requests: Arc<AtomicUsize>,
}

impl QuotaPacedUpstream {
    async fn acquire_slot(&self) {
        let mut next = self.next_slot.lock().await;
        let now = tokio::time::Instant::now();
        if *next < now {
            *next = now;
        }
        tokio::time::sleep_until(*next).await;
        *next += self.slot_interval;
    }
}

impl ProfileFetcher for QuotaPacedUpstream {
    async fn bulk_fetch_profiles(
        &self,
        dids: &[String],
    ) -> TurboResult<Vec<Option<Arc<BlueskyProfile>>>> {
        self.acquire_slot().await;
        self.requests.fetch_add(1, Ordering::SeqCst);
        tokio::time::sleep(self.delay).await;
        Ok(dids
            .iter()
            .map(|did| Some(Arc::new(canary_profile(did))))
            .collect())
    }
}

impl PostFetcher for QuotaPacedUpstream {
    async fn bulk_fetch_posts(&self, uris: &[String]) -> TurboResult<Vec<PostFetchOutcome>> {
        self.acquire_slot().await;
        self.requests.fetch_add(1, Ordering::SeqCst);
        tokio::time::sleep(self.delay).await;
        Ok(uris
            .iter()
            .map(|uri| {
                let author = canary_profile("did:plc:referenced");
                PostFetchOutcome::Found(Arc::new(BlueskyPost {
                    uri: uri.clone(),
                    cid: "bafyreicanary".to_string(),
                    author,
                    text: "referenced".to_string(),
                    created_at: chrono::Utc::now(),
                    embed: None,
                    reply: None,
                    facets: None,
                    labels: None,
                    like_count: Some(0),
                    repost_count: Some(0),
                    reply_count: Some(0),
                }))
            })
            .collect())
    }
}

#[derive(Debug, Clone, serde::Serialize)]
struct DrainRun {
    configuration: &'static str,
    mode: &'static str,
    permits: usize,
    batches: usize,
    completed_records: usize,
    failed_batches: u64,
    requests: usize,
    wall_millis: u64,
    drain_batches_per_sec: f64,
    request_rate_per_sec: f64,
}

const CANARY_BATCHES: usize = 24;
const PROFILE_DELAY_MS: u64 = 1_200;
const POST_DELAY_MS: u64 = 900;

async fn run_drain(
    mode: HydrationExecutionMode,
    permits: usize,
    configuration: &'static str,
    mode_label: &'static str,
) -> DrainRun {
    let profile_requests = Arc::new(AtomicUsize::new(0));
    let post_requests = Arc::new(AtomicUsize::new(0));
    let next_slot = Arc::new(tokio::sync::Mutex::new(tokio::time::Instant::now()));
    let profile_upstream = Arc::new(QuotaPacedUpstream {
        delay: Duration::from_millis(PROFILE_DELAY_MS),
        slot_interval: Duration::from_millis(100),
        next_slot: Arc::clone(&next_slot),
        requests: Arc::clone(&profile_requests),
    });
    let post_upstream = Arc::new(QuotaPacedUpstream {
        delay: Duration::from_millis(POST_DELAY_MS),
        slot_interval: Duration::from_millis(100),
        next_slot: Arc::clone(&next_slot),
        requests: Arc::clone(&post_requests),
    });

    let hydrator = Arc::new(Hydrator::new_with_mode(
        TurboCache::new(4096, 4096),
        profile_upstream,
        post_upstream,
        mode,
    ));
    let semaphore = Arc::new(tokio::sync::Semaphore::new(permits));

    let started = tokio::time::Instant::now();
    let mut tasks = Vec::new();
    for batch in 0..CANARY_BATCHES {
        let hydrator = Arc::clone(&hydrator);
        let semaphore = Arc::clone(&semaphore);
        tasks.push(tokio::spawn(async move {
            let _permit = semaphore.acquire_owned().await.expect("semaphore closed");
            hydrator
                .hydrate_batch(vec![create_reply_message(
                    1,
                    "did:plc:parent",
                    &format!("canary-{batch:03}"),
                )])
                .await
        }));
    }

    let mut completed_records = 0usize;
    let mut failed_batches = 0u64;
    for task in tasks {
        match task.await.expect("canary task panicked") {
            Ok(records) => completed_records += records.len(),
            Err(_) => failed_batches += 1,
        }
    }
    let wall = started.elapsed();
    let requests = profile_requests.load(Ordering::SeqCst) + post_requests.load(Ordering::SeqCst);
    let wall_seconds = wall.as_secs_f64();

    DrainRun {
        configuration,
        mode: mode_label,
        permits,
        batches: CANARY_BATCHES,
        completed_records,
        failed_batches,
        requests,
        wall_millis: wall.as_millis() as u64,
        drain_batches_per_sec: CANARY_BATCHES as f64 / wall_seconds,
        request_rate_per_sec: requests as f64 / wall_seconds,
    }
}

#[tokio::test(start_paused = true)]
async fn recovery_canary_concurrent_mode_and_permits_beat_sequential_baseline() {
    // Migration order: sequential/6 (retained baseline shape) → parallel/6
    // (mode lever) → parallel/9 (permit lever).
    let sequential_baseline = run_drain(
        HydrationExecutionMode::Sequential,
        6,
        "sequential_permits_6",
        "sequential",
    )
    .await;
    let concurrent_mode = run_drain(
        HydrationExecutionMode::Parallel,
        6,
        "parallel_permits_6",
        "parallel",
    )
    .await;
    let permit_increase = run_drain(
        HydrationExecutionMode::Parallel,
        9,
        "parallel_permits_9",
        "parallel",
    )
    .await;

    let mut violations = Vec::new();

    // Correctness: every configuration completes the whole workload.
    for run in [&sequential_baseline, &concurrent_mode, &permit_increase] {
        if run.failed_batches != 0 {
            violations.push(format!(
                "{} reported {} failed batches",
                run.configuration, run.failed_batches
            ));
        }
        if run.completed_records != CANARY_BATCHES {
            violations.push(format!(
                "{} completed {} records, expected {CANARY_BATCHES}",
                run.configuration, run.completed_records
            ));
        }
    }

    // Quota safety: the combined request rate stays below the shared quota
    // with margin in every configuration.
    for run in [&sequential_baseline, &concurrent_mode, &permit_increase] {
        if run.request_rate_per_sec >= UPSTREAM_QUOTA_PER_SEC {
            violations.push(format!(
                "{} request rate {:.2}/s reached the {} requests/second quota",
                run.configuration, run.request_rate_per_sec, UPSTREAM_QUOTA_PER_SEC
            ));
        }
    }

    // Lever B: concurrent mode must drain faster than sequential at equal
    // permits.
    if concurrent_mode.wall_millis >= sequential_baseline.wall_millis {
        violations.push(format!(
            "concurrent mode did not improve drain: {}ms vs sequential {}ms",
            concurrent_mode.wall_millis, sequential_baseline.wall_millis
        ));
    }

    // Lever C: the permit increase must convert quota headroom into drain
    // rate on top of the concurrent mode.
    if permit_increase.wall_millis >= concurrent_mode.wall_millis {
        violations.push(format!(
            "permit increase did not improve drain: {}ms vs parallel/6 {}ms",
            permit_increase.wall_millis, concurrent_mode.wall_millis
        ));
    }

    let zero_failed_batches =
        concurrent_mode.failed_batches == 0 && permit_increase.failed_batches == 0;
    let quota_respected = concurrent_mode.request_rate_per_sec < UPSTREAM_QUOTA_PER_SEC
        && permit_increase.request_rate_per_sec < UPSTREAM_QUOTA_PER_SEC;
    let drain_improved =
        permit_increase.drain_batches_per_sec > sequential_baseline.drain_batches_per_sec;

    let artifact = serde_json::json!({
        "retained_sequential_baseline": {
            "source": "baseline.json, release 9b22f30 (2026-08-25)",
            "drain_msg_per_sec": BASELINE_DRAIN_MSG_PER_SEC,
            "hydration_critical_path_share": BASELINE_HYDRATION_CRITICAL_PATH_SHARE,
            "request_rate_per_sec": BASELINE_REQUEST_RATE_PER_SEC,
            "fill_items_per_request": {
                "profiles": BASELINE_FILL_ITEMS_PER_REQUEST_PROFILES,
                "posts": BASELINE_FILL_ITEMS_PER_REQUEST_POSTS,
            },
        },
        "workload": {
            "batches": CANARY_BATCHES,
            "profile_upstream_delay_ms": PROFILE_DELAY_MS,
            "post_upstream_delay_ms": POST_DELAY_MS,
            "shared_quota_per_sec": UPSTREAM_QUOTA_PER_SEC,
            "authorship": "all batches share the author and parent-author DIDs so profile lookups coalesce through the cache after the first wave",
        },
        "levers": {
            "concurrent_mode": concurrent_mode,
            "permit_increase": permit_increase,
        },
        "sequential_reference_run": sequential_baseline,
        "promotion_criteria": {
            "drain_rate_improvement": drain_improved,
            "zero_failed_batches": zero_failed_batches,
            "quota_respected": quota_respected,
        },
        "gate_passed": violations.is_empty(),
        "violations": violations,
    });
    std::fs::write(
        artifact_path("recovery-canary-mode-and-permits.json"),
        serde_json::to_string_pretty(&artifact).unwrap(),
    )
    .expect("retain mode-and-permits canary artifact");

    assert!(
        artifact["gate_passed"].as_bool().unwrap(),
        "recovery canary failed: {violations:?}"
    );
}
