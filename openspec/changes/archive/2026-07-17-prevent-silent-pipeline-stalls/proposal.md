## Why

Jetstream Turbo remained reachable and reported healthy while producing no broadcast messages for 183 recorded hours between July 9 and July 17, 2026. The service needs an end-to-end definition of healthy progress, automatic recovery from stale or blocked pipeline stages, and monitoring that distinguishes transport downtime from a connected-but-silent stream.

## What Changes

- Track progress and freshness independently at Jetstream ingress, batch completion, durable storage, event publication, and monitor broadcast boundaries.
- Detect stale-open upstream WebSocket connections and reconnect them after a configurable data-idle deadline.
- Bound batch and external-operation execution so blocked work cannot consume all processing capacity indefinitely.
- Make readiness reflect whether the pipeline is making timely progress while preserving diagnostic access during an incident.
- Expose pipeline progress, saturation, timeout, reconnect, and recovery signals through health diagnostics and Prometheus metrics.
- Update the comparison monitor to report transport availability separately from useful-stream availability and to attribute reconnects to data-idle detection instead of presenting the reconnect penalty as unexplained server downtime.
- Add deterministic tests for stale upstream connections, blocked batch stages, readiness degradation, automatic recovery, and monitor accounting.

## Capabilities

### New Capabilities

- `pipeline-progress-supervision`: Detect, report, and recover from stale ingress or stalled processing while keeping readiness aligned with end-to-end message progress.
- `stream-reliability-monitoring`: Measure transport connectivity and useful message delivery independently, retaining explicit reasons for data-idle reconnects and downtime.

### Modified Capabilities

None. This repository does not yet define baseline OpenSpec capabilities.

## Impact

- Rust Jetstream client connection lifecycle and configuration.
- Turbocharger batch orchestration, concurrency control, health diagnostics, readiness, and Prometheus output.
- Monitor stream client, uptime aggregation, historical storage/API data, and dashboard labels.
- Integration and fault-injection tests around WebSocket, hydration, storage, publishing, and broadcast seams.
- Operational alerting can move from process/socket availability to message-progress freshness; existing health response consumers may observe readiness changing to `503 Service Unavailable` during a stale pipeline.
