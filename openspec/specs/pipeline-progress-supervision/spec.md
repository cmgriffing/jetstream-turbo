## Purpose
Ensure the ingestion pipeline exposes useful progress, detects silent stalls, bounds downstream execution, recovers automatically, and reports readiness from actual data flow.

## Requirements

### Requirement: Pipeline stage progress is observable
The service SHALL maintain a progress snapshot containing state, counters, and freshness for upstream connection, valid Jetstream ingress, input-channel saturation, active and completed batches, durable storage, event publication, and monitor broadcast stages.

#### Scenario: Healthy pipeline snapshot
- **WHEN** valid Jetstream messages are being processed successfully
- **THEN** the health diagnostics and Prometheus endpoint expose fresh ingress and completion timestamps, advancing counters, batch capacity, and the absence of a stale stage

#### Scenario: Blocked stage is identified
- **WHEN** a pipeline stage stops completing while earlier stages continue to advance
- **THEN** diagnostics identify the blocked stage, its age, and the relevant batch or connection state without requiring a process restart

### Requirement: Stale upstream connections recover automatically
The service SHALL apply a configurable useful-data idle deadline to each Jetstream connection and SHALL reconnect through the configured endpoint rotation when the deadline expires.

#### Scenario: Open socket stops delivering data
- **WHEN** an upstream WebSocket remains open but no valid Jetstream data message arrives before the idle deadline
- **THEN** the service records a `data_idle_timeout` reason, drops the stale connection, rotates endpoints, and reconnects

#### Scenario: Control frames do not mask data staleness
- **WHEN** an upstream connection sends ping, pong, binary, malformed, or other non-data frames without a valid Jetstream message
- **THEN** those frames do not refresh the useful-data deadline

#### Scenario: Valid delivery resumes
- **WHEN** a replacement connection delivers a valid Jetstream message
- **THEN** ingress freshness becomes healthy and the service records the recovery duration and endpoint

### Requirement: Batch execution is bounded and recoverable
The service SHALL enforce a configurable end-to-end execution deadline for each batch, report its executing stage, and release all concurrency capacity when the batch completes, fails, or times out.

#### Scenario: Downstream operation blocks
- **WHEN** hydration, storage, or publication does not complete before the batch deadline
- **THEN** the batch fails with a typed timeout identifying the stage, its semaphore permit is released, and the run loop initiates bounded recovery

#### Scenario: All permits approach exhaustion
- **WHEN** active work consumes the configured processing capacity
- **THEN** diagnostics expose active and maximum permits, oldest batch age, and input-channel occupancy or drops before the service can remain silently stalled

#### Scenario: Processing resumes after recovery
- **WHEN** the blocked dependency becomes available and the processing loop restarts
- **THEN** new batches complete and all progress stages return to a fresh state without restarting the HTTP server

### Requirement: Readiness reflects useful pipeline progress
After a configurable startup grace period, the service SHALL report ready only when required dependencies are healthy, valid ingress is fresh, and processing completion is keeping pace with ingress.

#### Scenario: Reachable but silent process
- **WHEN** the HTTP and WebSocket server is reachable but valid Jetstream ingress is stale beyond the configured threshold
- **THEN** `/ready` returns `503 Service Unavailable` and health diagnostics name ingress staleness as the readiness reason

#### Scenario: Ingress advances but output is stalled
- **WHEN** valid ingress remains fresh but successful batch completion or required durable output becomes stale
- **THEN** `/ready` returns `503 Service Unavailable` and diagnostics name the stalled downstream stage

#### Scenario: Startup grace
- **WHEN** the process has started but has not yet received its first valid Jetstream message and the startup grace period has not expired
- **THEN** diagnostics report a starting state without prematurely classifying the pipeline as stale

#### Scenario: No monitor subscribers
- **WHEN** durable processing is healthy and no monitor WebSocket receiver is subscribed
- **THEN** the absence of successful broadcast delivery alone does not make the service unready

#### Scenario: Readiness recovers
- **WHEN** dependencies and all required progress stages return within their freshness thresholds
- **THEN** `/ready` returns success after the configured recovery condition is met

### Requirement: Pipeline failures produce actionable telemetry
The service SHALL emit structured transition logs and Prometheus metrics for stale stages, recoveries, timeouts, upstream reconnect reasons, input drops, batch capacity, completion throughput, and broadcast delivery.

#### Scenario: State changes to stale
- **WHEN** a freshness or execution threshold is crossed
- **THEN** one transition event records the stage, reason, age, endpoint or batch identifier, capacity state, and recovery action

#### Scenario: State remains stale
- **WHEN** the same stage remains stale across repeated health checks
- **THEN** the service avoids per-check error spam while metrics continue to expose the current stale state and age

#### Scenario: State recovers
- **WHEN** the stale stage makes useful progress again
- **THEN** a recovery event and counter include the stale duration and recovery reason
