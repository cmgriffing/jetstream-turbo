# Stream Comparison and Recovery Exploration

## Audience and intended use

This note is for a maintainer returning to the stream-monitoring and Rust recovery work without the original investigation context. After reading it, they should be able to review or implement the OpenSpec change `align-stream-comparison-and-recovery` without mistaking replay throughput for a live lead or treating the suspected ingestion failure as proven.

## Executive summary

The replay-aware monitoring change correctly detects when a stream is catching up, but it does not calculate ahead/behind results from comparable data. The dashboard still subtracts cumulative arrival counters. When Rust drains replay backlog, those extra arrivals remain in its counter after it becomes live, so Rust can appear permanently ahead of a baseline even though the streams are currently aligned.

Production Rust was also genuinely behind the live tip during this investigation. Its pipeline was saturated and reported an active boundaryless `invalid_message` containment incident. The strongest code-level explanation is a batch invariant that treats a source timestamp regression as invalid despite valid ingress ordering, but production diagnostics do not expose enough bounded detail to prove that exact variant.

The follow-up change therefore addresses two related but distinct problems:

1. Define comparisons over one shared, settled source-time window.
2. Ensure Rust recovery converges despite timestamp regression and boundaryless failures.

## Production evidence

The Rust websocket sampled during the investigation was:

`wss://turbostream-rs.messijo.com/api/v1/ws`

On 2026-08-18 at approximately `23:39:56Z`, it emitted source events around `20:00:45Z`. The enriched envelope's `processed_at` value was current while `message.time_us` was roughly three hours and thirty-nine minutes old. This confirmed replay delivery rather than a monitor clock-parsing defect.

The service health snapshot independently reported:

- recovery phase `catching_up`;
- roughly three hours and thirty-eight minutes of committed source lag;
- input occupancy `10000/10000` with backpressure active;
- more than twenty-five minutes of cumulative blocked-send time;
- continued batch completion and no input drops;
- an active, non-persistent `pipeline:invalid_message` containment incident;
- no failed checkpoint boundary for that incident;
- a configured recovery delay of five minutes;
- dependency health reported as healthy while readiness reported recovering.

A later snapshot showed the committed watermark still advancing and lag decreasing slowly. The pipeline was making progress, but its catch-up headroom was narrow enough for a multi-hour backlog to persist.

These values were transient production observations, not permanent configuration facts. Recheck the live health endpoint before using them operationally.

## Confirmed monitor accounting defect

The monitor now tracks source watermarks and decides whether streams are live, catching up, or unknown. That eligibility is applied to a comparison produced by subtracting each stream's cumulative arrival counter.

Conceptually, the current behavior is:

```text
Rust raw arrivals since monitor observation began
minus
baseline raw arrivals since monitor observation began
```

The counters can contain different source-time intervals because of:

- Rust replay bursts;
- unequal connection start times;
- disconnect and reconnect gaps;
- monitor restarts;
- overlap duplicates if cursor replay is later added;
- delayed and out-of-order Turbo batch broadcasts.

Checking only the current delivery mode and watermark skew does not repair the historical mismatch. If Rust receives 80,000 replay events before becoming live, that 80,000-event surplus remains after eligibility changes to live.

The earlier proposal required a comparable live-rate delta, but the applied frontend gates cumulative count differences instead. Raw counts are useful transport evidence; they are not a valid source-completeness or performance lead.

## Confirmed recovery bookkeeping gap

Failure containment clears only after a durable checkpoint reaches a recorded failed source boundary. A boundaryless incident has no such boundary, so later checkpoint progress cannot clear it under the current rule.

Production reported exactly this shape:

```text
active incident:        yes
category:               invalid_message
failed source boundary: none
checkpoint progressing: yes
```

That incident can remain active after useful work resumes. If the same category recurs, it can be counted as continued recurrence rather than a newly recovered incident.

The delay policy compounds the issue: every error classified non-retryable receives the maximum delay, including a first boundaryless internal invariant failure. This can pause recovery for five minutes even when an immediate bounded restart would be safe.

## Strong hypothesis requiring confirmation

Ingress events receive increasing process-local ordinals. Batches nevertheless require both increasing ordinals and a non-regressing first-to-last source timestamp. A valid arrival sequence with a later event carrying an earlier `time_us` makes batch construction return `invalid_message` before a failed source range exists.

That behavior matches the production containment shape, particularly the missing failed boundary. It is not yet proven to be the production subtype because health deliberately exposes only the broad category and application logs were not available during exploration.

Implementation should first add a fixed-cardinality failure subtype and regression fixture. It should not assume timestamp regression is the only possible source of `invalid_message`.

## Required comparison model

A semantic comparison needs one shared data interval:

```text
stream A retained coverage ───────[================]──────
stream B retained coverage ──────────[================]───
shared settled interval                [==========]
                                       ^ compare only this
```

The agreed direction is:

- retain bounded source-time buckets per stream;
- wait for a settlement allowance before closing a bucket;
- derive a portable source identity from equivalent raw and enriched fields;
- count a portable identity once within a comparable window;
- compare only the closed intersection covered continuously by both streams;
- begin a new comparison epoch after catch-up or reconnect;
- end the epoch on disconnect, idle delivery, catch-up, missing identity coverage, or excessive watermark skew;
- calculate counts, rates, and signed deltas in the backend;
- preserve raw arrival totals but never describe their difference as ahead, behind, or even.

## Required recovery model

The follow-up design uses ingress ordinal as the completion-order authority. Source time remains the portable replay cursor and lag signal but is not assumed to be strictly monotonic within a batch.

For a boundaryless failure, containment records the durable checkpoint ordinal present at incident start. A later durable ordinal proves recovery and clears the incident. If no checkpoint existed, the first persisted checkpoint proves progress.

Recovery diagnostics also need:

- bounded failure subtype and pipeline stage;
- boundary presence and incident-start checkpoint ordinal;
- committed source-seconds advanced per wall second;
- net convergence rate and a stable/unavailable/non-converging ETA state;
- separate queued work and running permit holders;
- queue occupancy, blocked time, and completion throughput;
- serving dependency health distinct from recovering, live, and stale readiness.

## Decisions deliberately left open

- Settlement allowance and retained comparison horizon need empirical defaults.
- Identity-incomplete buckets may suppress all semantic comparison or only completeness counts while permitting timestamp-only rate comparison.
- Baseline reconnects may gain replay cursors, or every monitor-side reconnect may deliberately terminate the comparison epoch.
- The amount of durable progress required to separate recurring boundaryless incidents may be one ordinal or a larger policy threshold.
- Processing capacity should be tuned only after correctness fixes and convergence metrics identify the actual bottleneck; increasing concurrency blindly could violate upstream API limits.

## Follow-up artifact

The complete proposal, design, capability specifications, and implementation checklist are captured in the OpenSpec change:

`align-stream-comparison-and-recovery`

That change is the source of truth for implementation scope. This note preserves the evidence and reasoning that motivated it.
