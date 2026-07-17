## Context

Jetstream Turbo currently runs the HTTP/WebSocket server independently from the ingestion and processing loop. The health model checks Bluesky authentication plus SQLite and not_redis availability, but it does not check whether Jetstream data is arriving or whether batches are completing. The Jetstream reader has no data-idle deadline, and batch work can wait indefinitely in hydration, storage, or publication before records reach the monitor broadcast channel.

This separation allowed the process to remain reachable and return healthy responses while the monitor observed zero records for 183 hours. The monitor then converted each data-idle reconnect cycle into apparent downtime, obscuring the distinction between a reachable silent service and a failed transport.

The implementation spans the Rust service and the Rust/React monitor. It must preserve high-throughput processing, avoid treating WebSocket control frames as useful progress, and provide useful diagnostics even while readiness is failing.

## Goals / Non-Goals

**Goals:**

- Detect loss of useful Jetstream input even when the upstream socket remains open.
- Bound pipeline work so one blocked dependency cannot permanently consume all permits.
- Recover automatically at the smallest safe boundary and escalate to the existing run-loop restart when necessary.
- Make readiness describe end-to-end progress rather than process reachability alone.
- Identify the stage and reason for every stale, timeout, reconnect, and recovery transition.
- Separate transport availability from message-delivery availability in current and historical monitor data.
- Provide deterministic fault-injection seams and regression tests for both leading incident hypotheses.

**Non-Goals:**

- Guaranteeing lossless replay of Jetstream events missed while disconnected; cursor-based recovery is a separate capability.
- Replacing systemd, PostHog, Prometheus, SQLite, not_redis, or the existing comparison monitor.
- Changing enrichment semantics, Bluesky API batching, or the public enriched-record WebSocket payload.
- Retroactively inferring useful-delivery health for historical rows that lack the required reason data.

## Decisions

### 1. Model progress explicitly at pipeline boundaries

Add a shared `PipelineProgress` component that records monotonic timestamps, counters, and state for:

- upstream connection established/disconnected;
- last valid Jetstream data message received;
- input channel drops and recovery;
- batch started/completed/timed out, including oldest active batch age;
- last successful SQLite store and event publication;
- last attempted/successful monitor broadcast and current receiver count;
- reconnect count and reason.

Expose immutable snapshots to health and metrics code. Use monotonic time for decisions and wall-clock timestamps only for serialized diagnostics.

This centralizes the definition of progress without coupling the HTTP server to task internals. Per-module counters alone were rejected because they cannot consistently derive end-to-end readiness or correlate a blocked stage.

### 2. Treat only valid data messages as ingress progress

Wrap each upstream connection read with a configurable data-idle deadline. Text frames that parse into valid `JetstreamMessage` values refresh the data-progress clock. Ping, pong, binary, malformed, and raw control frames do not.

On expiry, close/drop the current upstream socket, record `data_idle_timeout`, rotate to the next configured endpoint, and reconnect using the existing delay policy. A successful connection does not reset failure accounting until valid data arrives.

This is preferred over TCP keepalive or ping-only supervision because those mechanisms prove transport reachability, not useful firehose delivery.

### 3. Put a deadline around complete batch execution

Assign each batch an identifier and run hydration, storage, publication, and broadcast preparation under a configurable outer deadline. Record the currently executing stage so a timeout identifies the blocked boundary. Timeout or cancellation must release the semaphore permit through RAII and return a typed error to the run loop.

The existing run-loop restart remains the coarse recovery boundary. Stale upstream reads reconnect locally; a batch timeout fails the run so outstanding batch tasks can be aborted/drained before a fresh message stream is created. Independent per-operation retries remain bounded within the outer batch deadline.

Moving broadcast before durable store/publication was rejected because it would make the monitor look healthy while durable outputs were failing. Leaving timed-out tasks detached was rejected because it would retain permits and allow duplicate side effects.

### 4. Derive readiness from dependencies and fresh progress

Extend health diagnostics with a pipeline snapshot and a machine-readable readiness reason. During a configurable startup grace period, the service may be starting while it establishes its first useful input. After that grace period, readiness is false when any of the following holds:

- required storage/auth dependencies are unhealthy;
- no valid Jetstream data has arrived within the ingress freshness threshold;
- the oldest active batch exceeds its execution threshold;
- ingress is advancing but successful batch completion/output is stale.

Monitor broadcast freshness is diagnostic and affects readiness only while receivers are subscribed; a service with no monitor clients must not become unready solely because broadcast sends have no receivers. `/api/v1/health` continues returning the complete diagnostic body when unhealthy, and `/ready` returns `503 Service Unavailable` until progress recovers.

Process-liveness and dependency-only health were rejected because they reproduce the incident's false-green state.

### 5. Emit transition-based operational signals

Emit structured logs only on state transitions and periodic summaries, with fields for stage, reason, endpoint, batch ID, age, active permits, channel occupancy/drops, and recovery duration. Add Prometheus gauges/counters for stage freshness, active/maximum batches, oldest batch age, input drops, timeouts, reconnects by reason, completed records, and broadcast receiver/send counts.

Transition-based logs avoid a reconnect or health-check log storm while retaining enough evidence to distinguish stale ingress from downstream saturation.

### 6. Give the monitor two independent availability models

The monitor tracks:

- **Transport availability**: whether a WebSocket connection can be established and maintained, with disconnect reason attribution.
- **Delivery availability**: whether valid text records arrive within the configured useful-data interval.

A data-idle reconnect records delivery downtime plus a `data_idle_timeout` recovery attempt. Its client-enforced reconnect delay is not presented as an unexplained server transport outage. Handshake failures, read/write errors, peer closes, and server timeouts retain distinct transport reasons.

Historical storage gains reasoned reconnect and delivery-state fields through an additive SQLite migration. Existing legacy rows remain readable and are labeled as legacy/unknown where transport and delivery cannot be separated. The API returns both metrics, and the dashboard presents them with unambiguous labels and tooltips.

### 7. Verify with deterministic fault injection

Use local WebSocket fixtures that can stay open without data, send only control frames, resume data, or close. Use mock hydration/store/publisher adapters that can block until cancellation. Tokio paused time should drive idle and deadline tests without wall-clock waits.

End-to-end tests assert readiness transitions, reconnect reasons, permit release, output recovery, metrics, and monitor accounting. Live production probes remain operational verification, not automated test dependencies.

## Risks / Trade-offs

- **[False stale detection during genuinely quiet traffic]** → Keep thresholds configurable, base progress on the selected collection's observed rate, and use a conservative default with startup grace.
- **[Timeout cancellation occurs after a partial durable side effect]** → Preserve stable batch identifiers, report the failed stage, keep operations idempotent where supported, and document at-least-once behavior rather than claiming transactional processing across SQLite and not_redis.
- **[Restarting the run loop can drop buffered messages]** → Prefer local upstream recovery, bound and drain task shutdown, expose drop counts, and leave cursor-based replay to a future change.
- **[Readiness flaps during dependency or upstream turbulence]** → Use separate failure and recovery thresholds or a short consecutive-success requirement, while metrics retain the raw ages.
- **[Historical monitor charts change meaning]** → Version/add fields rather than rewriting old rows and visibly mark legacy periods with unknown reason attribution.
- **[Additional atomics and timestamps affect the hot path]** → Update counters at stage boundaries rather than per enrichment field and benchmark the ingestion path before rollout.

## Migration Plan

1. Add progress tracking, diagnostics, and metrics without changing readiness; deploy and observe thresholds against normal traffic.
2. Enable upstream data-idle reconnection and bounded batch execution with structured reason reporting.
3. Enable progress-aware readiness after validating startup and recovery thresholds.
4. Apply the monitor's additive SQLite migration, begin storing reasoned transport/delivery samples, and deploy the updated API/UI together.
5. Alert on delivery staleness and batch timeouts; retain process and transport alerts as separate signals.

Rollback can disable progress-aware readiness and deadlines through configuration while leaving diagnostics present. The additive monitor schema remains backward-compatible; the previous monitor binary can ignore new columns.

## Open Questions

- Production logs from July 9 should still be examined to determine whether the initiating event was stale ingress or downstream permit exhaustion; both paths are covered by this design.
- Initial timeout defaults should be selected from observed production percentiles during migration step 1 rather than inferred solely from the incident.
- Cursor-based replay should be evaluated separately if measurements show that reconnect loss is operationally significant.
