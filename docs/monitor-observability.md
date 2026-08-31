# Monitor connectivity, incidents, and observability

This document describes the monitor's transport/delivery state model, the
durable incident ledger, the versioned operational API, all configuration
values, metrics, retention, status-code semantics, and rollback behavior.

For the runbook (operational procedures, alerting), see
`docs/operations-runbook.md`.

## State model

Each configured stream has two orthogonal state machines:

- **Transport**: `connecting`, `connected`, `disconnected`
- **Delivery**: `unknown`, `waiting`, `delivering`, `idle`

A successful handshake enters `connected/waiting`. A useful text record enters
`connected/delivering`. Crossing the delivery-idle deadline enters
`connected/idle` **without closing the socket**. A socket error, peer close, or
missed liveness deadline enters `disconnected`. The first useful record after
any disruption returns delivery to `delivering` and resolves any open incident.

Accounting rules:

- One **outage episode** per connected→disconnected transition.
- Every failed connection attempt increments **reconnect attempts**
  independently; attempts never create episodes or reset the outage boundary.
- **Transport recovery** ends at handshake success, measured from the original
  outage boundary.
- **Delivery recovery** ends at the first useful record.
- An **idle episode** is a delivery-idle detection on a live socket; it counts
  zero transport outages.

## Configuration

| Setting | Default | Meaning | Validation |
| --- | --- | --- | --- |
| `heartbeat_interval_seconds` | `20` | Proactive WebSocket Ping cadence | `> 0` |
| `transport_liveness_deadline_seconds` | `60` | Max age of peer-liveness evidence (any Pong or peer frame) before transport loss | must exceed heartbeat interval |
| `stream_idle_timeout_seconds` | `30` | Delivery-idle deadline (previously the reconnect trigger; now only emits an idle transition) | `> 0` |
| `reconnect_backoff_min_seconds` | `1` | Minimum exponential backoff (with ±20% jitter) | `> 0` |
| `reconnect_backoff_max_seconds` | `30` | Maximum backoff delay | `>= min`, `> 0` |
| `incident_retention_days` | `90` | Retention for terminal-state incidents | `> 0` |
| `monitor_release` | crate version | Deployed release identity (metrics, health, records) | non-empty |
| `api_server_url` | `http://localhost:3001` | Server URL published in the OpenAPI document | HTTP(S) |

Settings not listed here (streams, bind address, comparison settings, event
time thresholds, diagnostics log) keep their previous meaning.

## Ordered transition stream

Per stream, the `StreamClient` emits one ordered `StreamEvent` sequence —
handshake success, delivery resumed/idle, transport lost, reconnect attempt
failed, and record batches — so every consumer sees one ordering authority.
A single `TransitionProcessor` per stream derives state, uptime accounting,
comparison eligibility, incident commands, and metrics.

## Incident ledger

SQLite (additive, non-destructive migrations):

- `monitor_incidents` — summaries keyed by a sortable opaque ULID `id`.
- `monitor_incident_events` — ordered events keyed `(incident_id, sequence)`
  with a unique constraint; `ON DELETE CASCADE` cleanup; indexes for keyset
  listing by `(detected_at DESC, id DESC)`.

One incident covers the entire continuous loss of useful delivery across
delivery idle, transport loss, reconnect attempts, transport recovery, and the
wait for the first useful record. It resolves only on the first useful record.
Incidents store only bounded operational fields: no URLs, display names,
source identities, incident-free exception text, payloads, or credentials.

Startup reconciliation: incidents left open by a previous process become
`incomplete` with `observation_complete=false` and a terminal
`observation_gap` event before new observation begins.

Retention: terminal-state incidents older than `incident_retention_days` are
deleted with their events in one transaction. Open incidents are retained
until they reach a terminal state.

## API

Versioned, read-only, unauthenticated (deployment may restrict at the reverse
proxy). All operational responses use `Cache-Control: no-store`.

| Endpoint | Notes |
| --- | --- |
| `GET /api/v1/health` | Monitor self-health; HTTP 200 `healthy`/`degraded`, HTTP 503 `unhealthy` |
| `GET /api/v1/metrics` | Prometheus text exposition |
| `GET /api/v1/incidents` | Keyset pagination `(detected_at DESC, id DESC)`; `limit` default 50, max 200; filters `stream`, `state`, `trigger`, `detected_from`, `detected_to`, `min_silence_ms` |
| `GET /api/v1/incidents/{incidentId}` | Bounded sanitized detail; 404 missing/expired |
| `GET /api/v1/incidents/{incidentId}/events` | Ascending incident-local sequence; default limit 100, max 500 |
| `GET /openapi.json` | OpenAPI 3.1 contract (media type `application/vnd.oai.openapi+json`, contract ETag, `public, max-age=60`, `x-monitor-release` header) |

Status semantics:

- `200 degraded`: the monitor works but observes an external problem (a
  configured stream idle or disconnected). Alerts on stream health should use
  the metrics and incident stream rather than health status codes.
- `503 unhealthy`: the monitor cannot provide trustworthy observation —
  the observation loop stopped, required state cannot be persisted, or
  incident storage is unusable.
- `400 invalid_cursor/limit/filter`: standard machine-readable error shape
  `{code, message, api_version}`.
- A stale hourly writer is degraded context but does not by itself make the
  monitor untrustworthy.

Contract evolution: additive optional fields are compatible within
`/api/v1`; breaking changes require a new versioned path. The served document
is generated from the same typed handler annotations; a checked-in snapshot
(`monitor/openapi/openapi.json`) is compared against the regenerated contract
in tests and must be refreshed (`OPENAPI_SNAPSHOT_WRITE=1 cargo test`) when the
contract changes intentionally.

## Metrics

All labels are bounded: `stream` ∈ {`a`, `b`, `baseline1`, `baseline2`}.
URLs, display names, source identities, incident IDs, exception text, record
contents, and credentials are never used as labels or retained in
observability data.

Key families:

- `monitor_process_start_seconds_ago` — changed value identifies a process
  reset (counter restart).
- `monitor_stream_transport_state`, `monitor_stream_delivery_state`,
  `monitor_stream_connection_epoch` — per-stream gauges.
- `monitor_stream_last_useful_record_age_seconds`,
  `monitor_stream_last_pong_age_seconds`,
  `monitor_stream_source_lag_seconds` — per-stream ages.
- `monitor_outage_episode_total`, `monitor_reconnect_attempt_total`,
  `monitor_idle_episode_total`, `monitor_record_total`.
- `monitor_transport_recovery_seconds`, `monitor_delivery_recovery_seconds`,
  `monitor_data_gap_seconds` — recovery and gap histograms.
- `monitor_storage_failure_total`, `monitor_hourly_snapshot_age_seconds`,
  `monitor_incidents_retained`,
  `monitor_incident_last_success_age_seconds`,
  `monitor_hourly_last_success_age_seconds`,
  `monitor_dashboard_subscribers`.

## Historical reliability contract

Hourly rows written since this release use `metrics_contract_version = 4` /
`reliability_contract_version = 4` (classification `observed_episodes`) and
carry explicit `stream_*_outage_episodes` / `stream_*_reconnect_attempts`
columns plus the corresponding counters in `reliability_json`.

Rules for consumers:

- Rows with contract `< 4` are `observed`/`legacy_unknown`; their
  `stream_*_disconnects` count every disconnected status (including repeated
  failed handshake attempts) and **must not** be aggregated with v4 episode
  counts.
- v4 rows keep the legacy columns zeroed; episodes and attempts are read from
  the dedicated columns only.

## Structured decision logs

Logs target `monitor::transition` and `monitor::ledger` and record only
bounded fields (stream id, epoch, attempt ordinal, bounded reason, durations):
delivery idle detected, transport lost, reconnect attempt failed, transport
recovered, delivery recovered, incident persistence failed, retention cleanup
failed. There is no per-record operational logging.

## Rollback

Deploy the previous monitor binary. Incident tables are additive and unused by
the old binary; existing `/api/*` history routes keep functioning. No
destructive schema rollback is required.