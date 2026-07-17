## 1. Progress Model and Configuration

- [x] 1.1 Add failing unit tests for healthy, starting, ingress-stale, batch-stalled, and recovered pipeline progress snapshots.
- [x] 1.2 Add validated settings for ingress idle, batch execution, startup grace, readiness recovery, and feature rollout controls, with environment-loading tests and documented defaults.
- [x] 1.3 Implement the shared pipeline progress tracker with monotonic stage timestamps, wall-clock diagnostic timestamps, counters, active batch metadata, reconnect reasons, and immutable snapshots.
- [x] 1.4 Benchmark progress-tracker updates on the ingestion and batch hot paths and keep the overhead within the repository's accepted benchmark threshold.

## 2. Jetstream Ingress Supervision

- [x] 2.1 Add local WebSocket fixture tests for a stale-open connection, control-frame-only traffic, malformed text, endpoint rotation, and valid-data recovery using paused Tokio time where possible.
- [x] 2.2 Wire connection and valid-message events from `JetstreamClient` into the progress tracker without counting control or invalid frames as useful progress.
- [x] 2.3 Enforce the configurable useful-data idle deadline, close stale connections, rotate endpoints, and retain `data_idle_timeout` until valid data resumes.
- [x] 2.4 Emit transition logs and reconnect metrics by endpoint and reason, including recovery duration and input-channel drop/recovery state.

## 3. Batch Deadline and Capacity Recovery

- [x] 3.1 Add blocking hydrator, record-store, and event-publisher test doubles plus failing tests for stage timeout, permit release, task cleanup, and resumed processing.
- [x] 3.2 Assign batch identifiers, track the active stage, and apply the configurable outer deadline across hydration, storage, publication, and broadcast preparation.
- [x] 3.3 Return typed stage-timeout errors, guarantee RAII permit release, and make the run loop abort/drain outstanding batch tasks before restarting ingestion.
- [x] 3.4 Expose active/maximum permits, oldest batch age, stage timeout counts, successful completion counts, input occupancy, and input drops in progress snapshots.

## 4. Health, Readiness, and Service Telemetry

- [x] 4.1 Add table-driven tests for dependency failure, startup grace, stale ingress, advancing ingress with stale output, no monitor subscribers, and readiness recovery.
- [x] 4.2 Extend health diagnostics with the pipeline snapshot and a stable machine-readable readiness state, stage, and reason.
- [x] 4.3 Update health derivation and `/ready` so progress failures return 503 while `/api/v1/health` continues to return the full diagnostic body.
- [x] 4.4 Add Prometheus gauges and counters for stage age/state, ingress and completion throughput, batch capacity/timeouts, reconnect reasons, input drops, broadcast receivers, and successful sends.
- [x] 4.5 Add deduplicated stale/recovery transition logs and periodic progress summaries with the fields needed to distinguish stale ingress from downstream saturation.

## 5. Monitor Availability and Historical Model

- [x] 5.1 Add failing monitor tests for connected-and-delivering, connected-but-silent, transport failure, data-idle recovery, reason retention, and legacy history rows.
- [x] 5.2 Refactor stream status and uptime aggregation to track transport availability, delivery availability, reconnect reason, and client recovery duration independently for every stream.
- [x] 5.3 Add an idempotent additive SQLite migration and persistence model for the new availability durations and reconnect-reason counters.
- [x] 5.4 Extend the uptime history API and live WebSocket statistics with both availability models, reason totals, recovery duration, coverage, and legacy/unknown classification.
- [x] 5.5 Verify historical aggregation does not attribute the configured data-idle reconnect delay to unexplained server transport downtime.

## 6. Monitor Dashboard

- [x] 6.1 Update frontend stream and history types plus hooks to consume the new live and historical reliability fields while tolerating legacy responses.
- [x] 6.2 Present transport uptime, delivery uptime, delivery-stale state, reconnect causes, client recovery time, and coverage with distinct labels and explanatory tooltips.
- [x] 6.3 Update comparison tables and charts so every stream uses equivalent selected-window transport, delivery, message-rate, reason, and coverage calculations.
- [x] 6.4 Add frontend tests for prolonged silence, transport failure, partial coverage, and legacy/unknown rendering.

## 7. End-to-End Verification and Rollout

- [x] 7.1 Run the Rust service and monitor unit/integration suites, including deterministic fault-injection scenarios, and resolve all regressions.
- [x] 7.2 Run frontend tests, type checking, and production build for the updated monitor dashboard.
- [x] 7.3 Run the relevant ingestion/pipeline benchmarks against a clean-main baseline according to `rust/AGENTS.md` and document the result.
- [x] 7.4 **Deferred by user on 2026-07-17; not performed in this change.** Exercise a local end-to-end deployment with stale-open upstream and blocked downstream faults; verify readiness, diagnostics, metrics, reconnects, permit recovery, and monitor accounting.
- [x] 7.5 Document new settings, health fields, metrics, alert recommendations, staged enablement, and rollback controls in the Rust and monitor operational documentation.
- [x] 7.6 **Deferred by user on 2026-07-17; not performed in this change.** After observability-only deployment, measure normal stage-age percentiles, select production timeout defaults, and record the chosen values before enabling progress-aware readiness.
