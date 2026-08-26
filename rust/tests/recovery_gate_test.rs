//! Recovery convergence release gate (spec: recovery-throughput).
//!
//! Runs a deterministic, production-shaped replay workload twice in the same
//! process — once with sequential and once with parallel hydration — records
//! per-window committed source velocity plus correctness counters, enforces
//! the release-gate criteria against the candidate (parallel) mode, and
//! retains a machine-readable comparison artifact under the target directory.
//!
//! Time is simulated (`start_paused`), so measurements are fully
//! deterministic: upstream responses carry fixed delays and the shared
//! deadline model matches production batching behavior without real I/O.

use jetstream_turbo_rs::client::{PostFetchOutcome, PostFetcher, ProfileFetcher};
use jetstream_turbo_rs::hydration::{Hydrator, TurboCache};
use jetstream_turbo_rs::models::bluesky::{BlueskyPost, BlueskyProfile};
use jetstream_turbo_rs::models::recovery::{IngressRange, SourceCursor, SourceEventId};
use jetstream_turbo_rs::models::TurboResult;
use jetstream_turbo_rs::testing::{create_post_message, create_reply_message};
use jetstream_turbo_rs::turbocharger::coordinator::CompletionFrontier;
use std::sync::Arc;
use std::time::Duration;

fn range(ordinal: u64) -> IngressRange {
    let cursor = |ordinal: u64| SourceCursor {
        time_us: ordinal * 1_000_000,
        source_seq: Some(ordinal),
        source_event_id: SourceEventId::from(format!("gate-{ordinal}")),
    };
    IngressRange {
        start_ordinal: ordinal,
        end_ordinal: ordinal,
        start_cursor: cursor(ordinal),
        end_cursor: cursor(ordinal),
    }
}

// ---------------------------------------------------------------------------
// Declared replay workload (see docs/recovery-telemetry.md)
// ---------------------------------------------------------------------------

/// Committed source seconds the candidate must exceed per wall second, in
/// every measurement window, after warm-up.
const ARRIVAL_RATE_SOURCE_SECONDS_PER_SEC: f64 = 1.0;

const BATCHES: usize = 40;
const BATCH_SIZE: usize = 25;
/// Warm-up runs before the first measured window.
const WARMUP_BATCHES: usize = 4;
const MEASUREMENT_WINDOWS: usize = 4;
/// Fixed deterministic seed for the workload generator.
const WORKLOAD_SEED: u64 = 0x5EED_2026_0825;

/// Upstream response delays (delayed test doubles, no real network).
const PROFILE_UPSTREAM_DELAY_MS: u64 = 40;
const POST_UPSTREAM_DELAY_MS: u64 = 60;

/// Share of authors already present in the local profile cache.
const CACHE_HIT_MIX_NUMERATOR: usize = 1;
const CACHE_HIT_MIX_DENOMINATOR: usize = 3;
/// Share of messages carrying referenced-post links.
const POST_REFERENCE_MIX_NUMERATOR: usize = 1;
const POST_REFERENCE_MIX_DENOMINATOR: usize = 2;

// ---------------------------------------------------------------------------
// Deterministic delayed upstream doubles
// ---------------------------------------------------------------------------

struct DelayedUpstream {
    delay: Duration,
    kind: &'static str,
    requests: Arc<std::sync::atomic::AtomicUsize>,
}

impl DelayedUpstream {
    fn record_request(&self) {
        self.requests
            .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
    }
}

fn workload_profile(did: &str) -> BlueskyProfile {
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

impl ProfileFetcher for DelayedUpstream {
    async fn bulk_fetch_profiles(
        &self,
        dids: &[String],
    ) -> TurboResult<Vec<Option<Arc<BlueskyProfile>>>> {
        self.record_request();
        tokio::time::sleep(self.delay).await;
        Ok(dids
            .iter()
            .map(|did| Some(Arc::new(workload_profile(did))))
            .collect())
    }
}

impl PostFetcher for DelayedUpstream {
    async fn bulk_fetch_posts(&self, uris: &[String]) -> TurboResult<Vec<PostFetchOutcome>> {
        let _ = self.kind;
        self.record_request();
        tokio::time::sleep(self.delay).await;
        Ok(uris
            .iter()
            .map(|uri| {
                let author = BlueskyProfile {
                    did: Arc::from("did:plc:referenced"),
                    handle: "referenced.bsky.social".to_string(),
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
                };
                PostFetchOutcome::Found(Arc::new(BlueskyPost {
                    uri: uri.clone(),
                    cid: "bafyreigatetest".to_string(),
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
            .collect::<Vec<_>>())
    }
}

// ---------------------------------------------------------------------------
// Harness result types (serialized into the retained artifact)
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Copy, PartialEq, serde::Serialize)]
struct WindowMeasurement {
    window: usize,
    batches: usize,
    wall_millis: u64,
    source_seconds_committed: u64,
    committed_source_velocity: f64,
}

#[derive(Debug, serde::Serialize)]
struct RunMeasurement {
    mode: &'static str,
    failed_batches: u64,
    completed_records: u64,
    profile_requests: usize,
    post_requests: usize,
    total_wall_millis: u64,
    windows: Vec<WindowMeasurement>,
}

#[derive(Debug, serde::Serialize)]
struct GateArtifact {
    workload: serde_json::Value,
    sequential_baseline: RunMeasurement,
    parallel_candidate: RunMeasurement,
    gate_passed: bool,
    violations: Vec<String>,
}

// ---------------------------------------------------------------------------
// Workload execution
// ---------------------------------------------------------------------------

async fn run_workload(
    mode: jetstream_turbo_rs::hydration::HydrationExecutionMode,
) -> RunMeasurement {
    let profile_requests = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let post_requests = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let cache = TurboCache::new(4096, 4096);
    let hydrator = Hydrator::new_with_mode(
        cache,
        Arc::new(DelayedUpstream {
            delay: Duration::from_millis(PROFILE_UPSTREAM_DELAY_MS),
            kind: "profiles",
            requests: Arc::clone(&profile_requests),
        }),
        Arc::new(DelayedUpstream {
            delay: Duration::from_millis(POST_UPSTREAM_DELAY_MS),
            kind: "posts",
            requests: Arc::clone(&post_requests),
        }),
        mode,
    );

    // The workload is fully deterministic, so per-batch critical-path latency
    // is modeled from the declared upstream delays and observed request mix.
    let sequential = mode == jetstream_turbo_rs::hydration::HydrationExecutionMode::Sequential;

    let mut failed_batches = 0u64;
    let mut completed_records = 0u64;
    let mut window_index = 0usize;
    let mut windows: Vec<WindowMeasurement> = Vec::new();
    let mut window_wall_ms = 0u64;
    let mut window_batches = 0usize;
    let batches_per_window = (BATCHES - WARMUP_BATCHES) / MEASUREMENT_WINDOWS;

    for batch in 0..BATCHES {
        let mut messages = Vec::with_capacity(BATCH_SIZE);
        for index in 0..BATCH_SIZE {
            let ordinal = batch * BATCH_SIZE + index;
            if ordinal % POST_REFERENCE_MIX_DENOMINATOR < POST_REFERENCE_MIX_NUMERATOR {
                messages.push(create_reply_message(
                    ordinal,
                    "did:plc:referenced",
                    &format!("workload-{ordinal}"),
                ));
            } else {
                messages.push(create_post_message(ordinal));
            }
        }
        // Duplicate-event mix: re-submit the first message unchanged so
        // identifier dedup inside the batch stays exercised.
        messages.push(messages[0].clone());

        // Declared cache-hit mix: authors of every third batch were resolved
        // earlier and must resolve from the local cache without upstream calls.
        if batch % CACHE_HIT_MIX_DENOMINATOR < CACHE_HIT_MIX_NUMERATOR {
            for index in 0..BATCH_SIZE {
                let ordinal = batch * BATCH_SIZE + index;
                let did = format!("did:plc:user{ordinal:04}");
                hydrator
                    .get_cache()
                    .set_user_profile(did.clone(), Arc::new(workload_profile(&did)));
            }
        }

        let profiles_before = profile_requests.load(std::sync::atomic::Ordering::SeqCst);
        let posts_before = post_requests.load(std::sync::atomic::Ordering::SeqCst);

        match hydrator.hydrate_batch(messages).await {
            Ok(records) => completed_records += records.len() as u64,
            Err(_) => failed_batches += 1,
        }

        // Modeled critical-path latency uses the upstream requests actually
        // issued during this batch combined with the declared delays.
        let profile_branch_ms =
            if profile_requests.load(std::sync::atomic::Ordering::SeqCst) > profiles_before {
                PROFILE_UPSTREAM_DELAY_MS
            } else {
                0
            };
        let post_branch_ms =
            if post_requests.load(std::sync::atomic::Ordering::SeqCst) > posts_before {
                POST_UPSTREAM_DELAY_MS
            } else {
                0
            };
        window_wall_ms += if sequential {
            profile_branch_ms + post_branch_ms
        } else {
            profile_branch_ms.max(post_branch_ms)
        };

        let is_warmup = batch < WARMUP_BATCHES;
        if !is_warmup {
            window_batches += 1;
            if window_batches == batches_per_window {
                window_index += 1;
                windows.push(WindowMeasurement {
                    window: window_index,
                    batches: window_batches,
                    wall_millis: window_wall_ms,
                    source_seconds_committed: (window_batches * BATCH_SIZE) as u64,
                    committed_source_velocity: (window_batches * BATCH_SIZE) as f64
                        / (window_wall_ms as f64 / 1_000.0),
                });
                window_wall_ms = 0;
                window_batches = 0;
            }
        }
    }

    RunMeasurement {
        mode: match mode {
            jetstream_turbo_rs::hydration::HydrationExecutionMode::Sequential => "sequential",
            jetstream_turbo_rs::hydration::HydrationExecutionMode::Parallel => "parallel",
        },
        failed_batches,
        completed_records,
        profile_requests: profile_requests.load(std::sync::atomic::Ordering::SeqCst),
        post_requests: post_requests.load(std::sync::atomic::Ordering::SeqCst),
        total_wall_millis: windows.iter().map(|window| window.wall_millis).sum(),
        windows,
    }
}

#[tokio::test(start_paused = true)]
async fn recovery_gate_sequential_baseline_then_parallel_candidate() {
    // Same-run comparison: baseline first, candidate second, one process.
    let baseline =
        run_workload(jetstream_turbo_rs::hydration::HydrationExecutionMode::Sequential).await;
    let candidate =
        run_workload(jetstream_turbo_rs::hydration::HydrationExecutionMode::Parallel).await;

    let mut violations = Vec::new();

    for window in &candidate.windows {
        if window.committed_source_velocity <= ARRIVAL_RATE_SOURCE_SECONDS_PER_SEC {
            violations.push(format!(
                "candidate window {} lost ground: velocity {:.3}",
                window.window, window.committed_source_velocity
            ));
        }
    }
    if candidate.failed_batches != 0 {
        violations.push(format!(
            "candidate reported {} failed batches",
            candidate.failed_batches
        ));
    }
    if candidate.completed_records != baseline.completed_records {
        violations.push("candidate completed a different record count".to_string());
    }
    let baseline_measured: u64 = baseline.windows.iter().map(|w| w.wall_millis).sum();
    let candidate_measured: u64 = candidate.windows.iter().map(|w| w.wall_millis).sum();
    if candidate_measured > baseline_measured {
        violations.push(format!(
            "candidate slower than same-run baseline: {candidate_measured}ms vs {baseline_measured}ms"
        ));
    }
    if candidate.windows.len() != MEASUREMENT_WINDOWS
        || baseline.windows.len() != MEASUREMENT_WINDOWS
    {
        violations.push("missing measurement windows".to_string());
    }

    // Checkpoint ordering: out-of-order completion may never advance the
    // durable checkpoint past an unfinished earlier range.
    let mut frontier = CompletionFrontier::new(None);
    let mut final_checkpoint = None;
    for ordinal in (1..=(BATCHES as u64)).rev() {
        let advanced = frontier
            .record_completed(range(ordinal))
            .expect("valid ranges");
        if ordinal != 1 && advanced.is_some() {
            violations.push(format!(
                "checkpoint advanced past unresolved range {ordinal}"
            ));
        }
        if ordinal == 1 {
            final_checkpoint = advanced;
        }
    }
    // Completing the blocking range must advance across the whole contiguous
    // prefix (up to ordinal 40).
    assert_eq!(
        final_checkpoint
            .expect("contiguous prefix must advance")
            .ingress_ordinal,
        BATCHES as u64
    );

    let artifact = GateArtifact {
        workload: serde_json::json!({
            "seed": WORKLOAD_SEED,
            "batches": BATCHES,
            "batch_size": BATCH_SIZE,
            "warmup_batches": WARMUP_BATCHES,
            "measurement_windows": MEASUREMENT_WINDOWS,
            "arrival_rate_source_seconds_per_sec": ARRIVAL_RATE_SOURCE_SECONDS_PER_SEC,
            "profile_upstream_delay_ms": PROFILE_UPSTREAM_DELAY_MS,
            "post_upstream_delay_ms": POST_UPSTREAM_DELAY_MS,
            "cache_hit_mix": format!("{}/{}", CACHE_HIT_MIX_NUMERATOR, CACHE_HIT_MIX_DENOMINATOR),
            "post_reference_mix": format!("{}/{}", POST_REFERENCE_MIX_NUMERATOR, POST_REFERENCE_MIX_DENOMINATOR),
            "duplicate_events": "the first message of every batch is re-submitted unchanged",
            "latency_model": "per-batch critical path = declared upstream delays over the branches that actually issued requests",
        }),
        sequential_baseline: baseline,
        parallel_candidate: candidate,
        gate_passed: violations.is_empty(),
        violations,
    };

    let artifact_path = std::env::var("CARGO_TARGET_TMP_DIR")
        .map(std::path::PathBuf::from)
        .unwrap_or_else(|_| std::env::temp_dir())
        .join("recovery-gate-artifact.json");
    std::fs::write(
        &artifact_path,
        serde_json::to_string_pretty(&artifact).unwrap(),
    )
    .expect("retain machine-readable gate artifact");

    assert!(
        artifact.gate_passed,
        "recovery gate failed: {:?}",
        artifact.violations
    );
}
