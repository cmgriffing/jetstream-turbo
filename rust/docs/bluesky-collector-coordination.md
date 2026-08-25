# Bluesky collector coordination limits

The focused concurrent-load test `concurrent_load_reaches_configured_key_and_waiter_bounds_then_settles` drives both collectors to a four-key/eight-waiter test ceiling. It observes those exact high-watermarks, then verifies pending keys, in-flight keys, waiters, and retained identifier bytes all return to zero. Duplicate callers share active keys while retaining one bounded waiter registration per requested identifier.

Production defaults scale the same model to the existing six admitted pipeline batches of 25 upstream identifiers:

| Collector | Distinct-key ceiling | Waiter ceiling | Selection basis |
| --- | ---: | ---: | --- |
| profiles | 150 | 600 | `6 × 25` active keys and four-way duplicate fan-out |
| posts | 150 | 600 | `6 × 25` active keys and four-way duplicate fan-out |

Startup rejects a key or waiter ceiling below one upstream batch. Oversized caller inputs are resolved in chunks of at most 25, so one caller cannot reserve the entire request or deadlock on its own slots.

For the broader runtime-memory envelope, use these deliberately conservative coordination inputs per collector:

- 8 KiB per retained identifier, covering unusually large inputs rather than the much smaller observed DIDs and post URIs;
- 192 bytes of map, queue, phase, and sender metadata per active key;
- 128 bytes per registered waiter.

At 150 keys and 600 waiters this is 1,334,400 bytes per collector, or 2,668,800 bytes (about 2.55 MiB) for profiles and posts together. This envelope excludes response payloads: completed payloads are owned by callers and the bounded hydration caches, while in-flight payloads are transient and shared through `Arc`. Runtime diagnostics export the actual retained-identifier-byte high-watermark so the production-scale memory suite can replace this conservative input with measured values.

## Focused regression comparison

The concurrent measurement uses two disjoint upstream-sized test groups with two simultaneous callers per group. The pre-fix collector's coalescing contract implies two profile and two post requests for this workload; the post-fix test observes the same two requests per collector, so the focused request-count change is 0%. The unlocked-overlap regression remains green, preserving concurrent HTTP work rather than serializing it behind coordination state.

The reusable cache-capacity/TTL scenario emits `hydration_churn_diagnostics` with one process RSS sample after each settled wave. RSS is evidence rather than a platform-sensitive unit-test threshold: attribution comes from the stronger ownership invariants that every active collector count and retained identifier byte returns to zero after each wave, `completed_result_owners` is always zero, and the positive hydration caches remain at their configured two-entry test capacities.

The 2026-08-24 local debug verification run recorded RSS samples `[19087360, 19841024, 20168704, 20316160, 20398080, 20496384, 20529152, 20545536]` bytes across eight waves. The focused harness itself retains Wiremock request history, including request URLs, so its raw process slope is not a production RSS gate. Collector-attributed residency was stable at zero after every wave; the production-scale change must reuse the scenario with its non-retaining driver and separate collector-attributed from residual process memory.
