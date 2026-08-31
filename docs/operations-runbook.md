# Operations Runbook — Runtime Memory, VACUUM, Replay Convergence

Operational reference for the runtime memory envelope, cgroup telemetry,
SQLite VACUUM safety gating, replay concurrency scaling, and checkpoint
persistence coalescing. This runbook records the prerequisites for enabling
emergency containment switches (task 6.4 of
`bound-runtime-memory-and-oom-recovery`).

---

## 1. Cgroup memory telemetry verification

Cgroup diagnostics are resolved from the process's **actual cgroup identity**
(`/proc/self/cgroup`, cgroup v2 unified hierarchy with a cgroup v1 fallback),
not a fixed `/sys/fs/cgroup` mount. Resolved paths are computed once per
process.

### Verify telemetry is populating

1. `curl :8080/health | jq '.data.diagnostics.runtime_memory.cgroup'`
   - `hierarchy` must read `v2` (or `v1` on legacy hosts).
   - `current_bytes`, `high_bytes`, `max_bytes`, `pressure_some_avg10`,
     `pressure_full_avg10`, and `events.oom_kill` should be populated
     (unlimited limits appear as the limit field `null` plus
     `max_unlimited: true`).
2. Prometheus: `curl :8080/metrics | grep cgroup` — cgroup gauges must not be
   `NaN`.
3. If fields are `null` and a `collection_error` string is present, the
   resolution failed. Expected causes:
   - Container without access to `/sys/fs/cgroup/<path>/memory.*`.
   - Nested path mismatch: check `cat /proc/self/cgroup` against the error's
     "attempted" path list.
   - cgroup v1 host without a mounted memory controller.
4. Never accept `0` as "below limits": unavailable dimensions are `null`,
   never parsed zero.

### Gate check before containment enablement (prerequisite for 6.4)

- `health.diagnostics.runtime_memory.cgroup.current_bytes` populated on the
  production host for at least one full deployment cycle.
- `events.oom_kill` is a monotonic counter; an increase indicates cgroup-level
  OOM kills (the supervisor classifies these as `CgroupOom` incidents).

---

## 2. VACUUM memory safety and gating

### Modes (`VACUUM_EXECUTION_MODE`)

| Mode | Behavior |
| --- | --- |
| `file_backed_temp_store` (default) | VACUUM runs on a dedicated maintenance connection with `PRAGMA temp_store = FILE`; transient memory is bounded by the page cache, not database size. Temp-volume headroom ≥ database size is verified at startup and before each run. |
| `pooled_memory` | Legacy rollback switch: plain `VACUUM` on the pooled `temp_store = MEMORY` connection. Refused when the database exceeds `VACUUM_MAX_POOLED_MEMORY_DB_MB` (default 2048 MB); startup validation also rejects the combination at production database sizes. |

### Temp volume

- `VACUUM_TEMP_DIR` (optional) selects the temp volume; default is the process
  temp directory. It must exist at startup.
- Headroom requirement: at least the current database size free on the temp
  volume (warning below 2×). On the production host, point `VACUUM_TEMP_DIR`
  at the database volume if its free space is larger.

### Gating behavior

The scheduler defers a pending VACUUM (recording the reason in
`/health` → `sqlite_state.vacuum_gating_reason` and the
`jetstream_turbo_vacuum_gating_reason` gauge) when:

- memory pressure is not `normal` (`memory_pressure`),
- the pipeline phase is replay/catch-up (`recovery_phase`), or
- the current UTC hour is outside `VACUUM_WINDOW_*` (`window`).

The accumulated deferral is observable as
`jetstream_turbo_vacuum_deferred_seconds`. A VACUUM deferred past
`VACUUM_MAX_DEFER_HOURS` (default 6 h) force-runs at the next scheduler tick
with reason `force_defer` recorded (it is never starved indefinitely); a
force-deferred VACUUM during active replay may transiently deepen committed
lag — watch `jetstream_committed_lag_seconds` around the 03:00–05:00 window.

### Operator override (authenticated)

Maintenance overrides are **fail-closed**: unless `MAINTENANCE_API_KEY` is
configured, `POST /maintenance/vacuum` responds `401` with status
`disabled` and never schedules anything. With a key configured, requests
must carry it in the `X-Maintenance-Key` header (constant-time comparison
via SHA-256 digests):

```
curl -X POST -H "X-Maintenance-Key: $MAINTENANCE_API_KEY" :8080/maintenance/vacuum
```

The next scheduler tick force-runs any pending VACUUM, bypassing pressure,
phase, and window gates (`vacuum_last_forced_reason: force_defer`). Rotation
is by restart with the new key value; do not log the key.

### Cleanup under pressure

The over-budget cleanup loop's delete chunks pause while pressure is elevated
and resume on the next cycle; the existing cleanup backoff behavior is
unchanged.

---

## 3. Replay concurrency scaling

- `REPLAY_MAX_CONCURRENT_BATCHES` (default 27 ≈ 3× the live permit count 9)
  applies while the pipeline is in replay/catch-up **and** pressure is normal.
- Reuses the pressure-coordinator permit pool (single semaphore); restoration
  to the live permit count happens after `live=true` has held for the
  configured stability observations (`JETSTREAM_LIVE_STABILITY_OBSERVATIONS`).
- Metrics: `jetstream_turbo_effective_batch_permits` (current capacity),
  `jetstream_turbo_batch_permits_phase{phase="..."}`
  (replay/live/load-bearing phase flags), `jetstream_turbo_workload_phase`.
- Rollback: set `REPLAY_MAX_CONCURRENT_BATCHES` equal to
  `MAX_CONCURRENT_REQUESTS` (9) — replay then behaves exactly like the
  pre-change pipeline. Startup validation requires the value to be ≥ the live
  count.

### Envelope coupling

`in_flight_payload_limit_mb` must cover
`replay_max_concurrent_batches × 25 × max_ingress_event_bytes`
(177 MB with defaults vs. the 256 MB limit). Re-validate whenever either value
changes; the production-scale memory suite is the regression gate.

---

## 4. Checkpoint persistence coalescing

- The in-memory completion frontier always tracks committed work per batch;
  durable `ingestion_checkpoint` writes are coalesced to at most one write per
  `CHECKPOINT_PERSIST_INTERVAL_MS` (default 500 ms) or every
  `CHECKPOINT_PERSIST_BATCH_INTERVAL` completions (default 4), whichever comes
  first.
- Rollback: set both to `0` — persist-per-batch (pre-change) behavior is fully
  restored.
- Guarantees (unchanged):
  - the durable checkpoint never advances past committed work;
  - shutdown, controlled memory exit (`ControlledMemoryExit`), and
    failure-containment boundaries always flush the contiguous frontier;
  - a crash inside the coalescing window re-replays at most the window worth
    of events, which are deduplicated by `completed_source_event_ids` (no
    duplicate publication).
- Alert on diagnostics: `frontier.durable_checkpoint_ordinal` must be
  monotonic; the checkpoint cadence gauge/`last_committed_event_time_us` must
  remain live.

---

## 5. Rollback switches (summary)

| Switch | Values | Effect |
| --- | --- | --- |
| `CHECKPOINT_PERSIST_INTERVAL_MS` + `CHECKPOINT_PERSIST_BATCH_INTERVAL` | `0` / `0` | Persist-per-batch (legacy durability cadence) |
| `REPLAY_MAX_CONCURRENT_BATCHES` | `9` (== live count) | Replay at live concurrency |
| `VACUUM_EXECUTION_MODE` | `pooled_memory` | Legacy VACUUM (only valid above nothing; enforced ≤ `VACUUM_MAX_POOLED_MEMORY_DB_MB`) |
| `MEMORY_PRESSURE_ACTIONS_ENABLED` / `MEMORY_EMERGENCY_EXIT_ENABLED` | `false` | Pressure containment switches remain inert (current state) |

---

## 6. Alert thresholds (baseline; tune after the 6.4 canary)

| Metric | Alert when | Notes |
| --- | --- | --- |
| `jetstream_committed_lag_seconds` | > 300 s sustained 30 m, or non-decreasing during > 1 h of backlog drain | Replay convergence is the primary health signal |
| `jetstream_turbo_vacuum_pending` | == 1 for > `VACUUM_MAX_DEFER_HOURS` + 1 h | VACUUM should never starve indefinitely |
| `jetstream_turbo_vacuum_gating_reason{reason="memory_pressure"}` | == 1 for > 1 h | Pressure-gated maintenance sustained |
| `cgroup OOM kill delta` (`events.oom_kill`) | > 0 | Immediately investigate |
| `jetstream_turbo_vacuum_deferred_seconds` | > 6 h | Deferral clock approaching forced run |
| `pipeline_failure_persistent` | == 1 | Containment is persistent; check fingerprints |
| DB size gauge (`jetstream_turbo_db_size_bytes`) | > `max_db_size_mb` for > 2 cleanups | VACUUM not reclaiming / cleanup failing |

---

## 7. Verification runbook steps

1. **Cgroup telemetry** — confirm §1 outputs populate post-deploy; record one
   incident-free cycle before touching containment switches.
2. **VACUUM cycle** — on the production database (20 GiB, ~39% freelist):
   force-run via `POST /maintenance/vacuum` inside the window, then record
   completion, reclaimed bytes, duration, and the absence of memory-pressure
   transitions in the operational evidence.
3. **Convergence** — after a deployment with a 6+ hour backlog, confirm
   committed lag decreases monotonically faster than the production rate and
   the input channel occupancy settles below capacity
   (`jetstream_committed_lag_seconds` trend + input occupancy via health).
4. Only then proceed to the staged containment canary (task 6.4 of
   `bound-runtime-memory-and-oom-recovery`).