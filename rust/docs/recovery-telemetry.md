# Recovery Telemetry Reference and Operations Guide

This document is the operator reference for the recovery-observability and
recovery-throughput telemetry: every new health field and Prometheus metric
introduced for recovery diagnosis, its type, unit, reset behavior, allowed
(bounded) labels, plus lag-diagnosis queries, frontier-gap triage, canary
criteria, rollback steps, and the previous-termination artifact review
procedure.

Confidentiality invariant: no telemetry field or label ever contains
credentials, DIDs, AT URIs, source event IDs, request query strings, or
response bodies. Upstream identifiers appear only as fixed-width fingerprints,
never as labels.

## Process epoch and reset semantics

Every cumulative in-memory measurement resets to zero when the process
restarts. To make resets unambiguous, each scrape of `/api/v1/health` and
`/api/v1/metrics` carries the process epoch:

| Metric | Type | Unit | Reset | Labels |
|---|---|---|---|---|
| `jetstream_turbo_process_start_time_seconds` | gauge | unix seconds | changes on restart | none |
| `jetstream_turbo_release_info` | gauge (1) | - | re-evaluated per process | `release` (bounded ASCII, or omitted with `available="0"`) |
| `jetstream_turbo_previous_termination_info` | gauge (1) | - | loaded once at startup | `state` ∈ {available, missing, malformed, stale}, `class` ∈ {none, controlled_memory_exit, cgroup_oom, global_oom, application_failure, unavailable} |

Interpretation: a decrease in any `_total` series is only valid if
`jetstream_turbo_process_start_time_seconds` changed. Within one epoch all
cumulative values are monotonic. Rate queries must be scoped to a single epoch:

```promql
sum(rate(jetstream_turbo_pipeline_completed_records_total[5m]))
and unchanged jetstream_turbo_process_start_time_seconds[5m]
```

Deployment wiring: `deploy/activate-candidate.sh` writes
`JETSTREAM_TURBO_RELEASE_ID=<release-directory-name>` into
`/opt/jetstream-turbo/.env` on every activation; the systemd unit loads it via
`EnvironmentFile`. The service also reads
`JETSTREAM_TURBO_TERMINATION_PATH` (default
`/opt/jetstream-turbo/diagnostics/latest-termination.env`) for retained
previous-termination evidence.

Health fields (same data as JSON under `diagnostics.runtime_identity`):
`process_started_at_unix_seconds`, `release.availability`,
`release.identifier`, `previous_termination.state`, `.classification`,
`.captured_at_unix_seconds`. Missing evidence is reported explicitly
(`missing`/`malformed`/`stale`); the service never infers a crash reason.

## Source progress and convergence

| Metric / health field | Type | Unit | Reset |
|---|---|---|---|
| `jetstream_turbo_committed_source_velocity` | gauge | source-seconds per wall-second | windowed estimate, not cumulative |
| `jetstream_turbo_net_convergence_rate` | gauge | ratio above 1 = converging | windowed estimate |
| `jetstream_turbo_catch_up_eta_seconds` | gauge | seconds; NaN unless stable convergence | derived |

Velocity estimates use a bounded 12-sample window; fewer than three samples
report "unavailable", regressing source timestamps report "unstable". A large
but shrinking backlog is healthy; only velocity below 1.0 sustained over the
window means non-convergence.

## Input backpressure

| Health field | Metric | Type | Unit | Reset |
|---|---|---|---|---|
| `input_occupancy` / `input_capacity` | occupancy is derivable from channel gauges | gauge | events | current value |
| `input_backpressured` | - | bool in health | - | current state |
| `blocked_send_duration_ms` | - | counter | milliseconds | monotonic within epoch |
| `input_drops` | `jetstream_turbo_pipeline_input_drops_total` | gauge-exported counter | events | monotonic within epoch |

Continuous blocked-send growth with zero drops means saturation without loss;
any drop is a correctness failure and marks health unhealthy.

## Completion frontier

| Metric | Type | Unit | Reset |
|---|---|---|---|
| `jetstream_turbo_frontier_pending_ranges` | gauge | ranges | current value |
| `jetstream_turbo_frontier_next_required_ordinal` | gauge | ordinal | resumes from durable checkpoint after restart |
| `jetstream_turbo_frontier_furthest_completed_ordinal` | gauge | ordinal | NaN before first completion |
| `jetstream_turbo_frontier_durable_checkpoint_ordinal` | gauge | ordinal | survives restarts (durable) |
| `jetstream_turbo_frontier_unresolved_gap_age_seconds` | gauge | seconds | resets to NaN on contiguous advancement |

Triage: `pending_ranges > 0` with growing `unresolved_gap_age_seconds` means an
out-of-order gap is blocking checkpoint advancement — ordinary batch completion
continues meanwhile. Compare the blocking ordinal (`next_required_ordinal`)
against active-batch stages in `/api/v1/health`
(`diagnostics.pipeline_progress.active_batches`) rather than treating an
unchanged committed cursor as a stall.

## Stage and hydration latency

| Metric | Type | Unit | Labels |
|---|---|---|---|
| `jetstream_turbo_pipeline_stage_duration_seconds_sum/_count` | histogram | seconds | `stage` ∈ {ingress, duplicate_detection, hydration, storage, publication, broadcast, checkpoint_persistence, end_to_end}; `outcome` ∈ {success, error, timeout, cancellation} |
| `jetstream_turbo_bluesky_fetch_lock_duration_seconds_sum/_count` | histogram | seconds | `kind` ∈ {profiles, posts} (cache lookup + coordination hold) |
| `jetstream_turbo_bluesky_rate_limiter_wait_seconds_sum/_count` | histogram | seconds | `kind` (shared limiter wait per attempt) |
| `jetstream_turbo_bluesky_fetch_http_duration_seconds_sum/_count` | histogram | seconds | `kind` (upstream HTTP incl. retries) |
| `jetstream_turbo_bluesky_local_assembly_seconds_sum/_count` | histogram | seconds | `kind` (decode + local result assembly) |

Average latency per substage = Δsum / Δcount within one process epoch.
Coordination admission pressure is exposed through the bounded coordination
gauges below (waiters, high watermarks) rather than a duration histogram.

## Batch efficiency

Batch fill is aggregated in `/api/v1/health`
(`diagnostics.pipeline_progress`) and logged periodically: total/full/timer/
partial/shutdown batch counts with min/max/average size and average fill
percent. Upstream cardinality:

| Metric | Type | Unit | Labels |
|---|---|---|---|
| `jetstream_turbo_bluesky_api_requests_total` | counter | requests | `kind` |
| `jetstream_turbo_bluesky_request_items_total` | counter | identifiers | `kind`; items/request = Δitems/Δrequests |
| `jetstream_turbo_bluesky_fetch_errors_total` | counter | requests | `kind`, `class` ∈ {rate_limited, upstream} |

Retries that recover are observable independently of exhaustion:
`bluesky_request_retries_total{operation,category,retry_ordinal,retry_limit}`
and `bluesky_request_retry_delay_seconds` record every scheduled retry, while
`bluesky_request_exhaustions_total` counts only terminal failures.

## Lag diagnosis playbook

1. Confirm one process epoch (`process_start_time_seconds` unchanged).
2. Growing committed lag? Check `committed_source_velocity`: < 1.0 sustained =
   losing ground; ETA appears only when stably converging.
3. Velocity fine but cursor stuck? Inspect frontier: pending ranges + gap age
   identify out-of-order blocking.
4. Velocity < 1.0? Rank stage averages (Δsum/Δcount). Hydration dominant?
   Split via substage histograms: rate-limiter wait vs upstream HTTP vs cache
   lookup vs assembly.
5. Rate-limiter wait dominates → upstream quota bound. Coordination waiters at
   capacity → key ceiling reached. HTTP dominates → upstream latency/retries
   (check retry counters).

## Declared recovery acceptance workload

The release gate (`cargo test --test recovery_gate_test`) replays a fully
deterministic production-shaped workload and compares sequential against
parallel hydration in the same run. Declared parameters:

| Parameter | Value |
|---|---|
| Deterministic seed / generator | fixed constants, no randomness |
| Batches × batch size | 40 × 25 messages (+1 duplicate per batch) |
| Warm-up | first 4 batches, not measured |
| Measurement windows | 4 windows × 9 batches |
| Arrival rate to beat | 1.0 committed source-second per wall-second, every window |
| Upstream delays | profiles 40 ms, posts 60 ms per request (delayed doubles) |
| Cache-hit mix | authors of every 3rd batch resolve from local cache |
| Post-reference mix | every 2nd message carries one unique referenced post |
| Duplicate events | the first message of each batch is re-submitted |
| Latency model | per-batch critical path over branches that actually issued requests |

Gate criteria: every candidate window reports positive net convergence above
the arrival rate, zero failed batches, record parity with the baseline,
candidate not slower than the same-run sequential baseline, and durable
checkpoint advancement only across contiguous prefixes under reversed
completion order. The run writes a machine-readable comparison artifact to
`target/recovery-gate-artifact.json`.

## Parallel hydration switch

`HYDRATION_EXECUTION_MODE` selects sequential (default) or parallel resolution
of independent profile and referenced-post misses inside a batch. It is a
temporary rollback lever intended to be retained for one release cycle after
parallel hydration is promoted. Invalid values are rejected at startup.

Rollback: set `HYDRATION_EXECUTION_MODE=sequential` in
`/opt/jetstream-turbo/.env` and restart. No persisted data or checkpoints are
affected by either mode.

Canary promotion criteria (all must hold over every measurement window):
positive net convergence (`net_convergence_rate > 0`) at or above the
sequential baseline, no failed batches, no input drops, no stranded
coordination ownership (waiters/in-flight/pending return to zero), memory
within envelope, no increase in terminal upstream exhaustion, and checkpoint
ordering intact (`durable_checkpoint_ordinal` never skips an unresolved range).

## Previous-termination artifact review

The deployment captures a termination record on service exit
(`capture-memory-incident.sh` →
`/opt/jetstream-turbo/diagnostics/latest-termination.env`). At startup the
service reads `captured_at` and `incident_class` only; everything else in the
artifact stays out of diagnostics. Review procedure:

```bash
cat /opt/jetstream-turbo/diagnostics/latest-termination.env
curl -s http://127.0.0.1:8080/api/v1/health | jq .diagnostics.runtime_identity
```

Evidence older than 30 days is reported as `stale` and drops its
classification. Absent or unreadable artifacts report `missing`.
