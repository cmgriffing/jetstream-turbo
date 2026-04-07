# Autoresearch: TurboCache Set Performance

## Objective

Optimize the time to set a value in `TurboCache::set_user_profile`. The benchmark `cache_user_profile_set` measures the time to insert 1000 profiles. Current baseline: ~482,000 ns (0.482 µs per set). Potential improvements:

- Remove Time-To-Live (TTL) expiration checks: currently `time_to_live(300s)` adds overhead on each set (updating expiry). Disabling TTL may reduce per-set latency.
- Adjust eviction listener: currently increments a counter on every eviction; maybe unnecessary overhead? Could be made optional.
- Inline more methods or reduce metric updates.

We aim to reduce the median time for 1000 sets.

## Metrics

- **Primary**: `cache_set_ns` (nanoseconds, lower is better) — median time for 1000 set operations.
- **Secondary**: `cache_get_ns` (for the same run? Might not be affected) — optional.
- **Secondary**: `test_status`.

## How to Run

`./autoresearch.sh` runs the `cache_user_profile_set` benchmark and outputs `METRIC cache_set_ns=<median_ns>`.

## Files in Scope

- `src/hydration/cache.rs` — `TurboCache` implementation, builder configuration.
- `benches/hydration_benchmark.rs` — the benchmark.
- `autoresearch.sh`

## Off Limits

- Do NOT break correctness: cache must still respect capacity limits and evict least recently used items. Removing TTL is acceptable if it does not cause incorrect behavior beyond staleness policy; the original TTL was 300s, but capacity-limited eviction remains.
- Do NOT remove metrics collection (though we could make it optional behind a feature flag? But keep for monitoring).

## Constraints

- All tests must pass.
- No new dependencies.

## What's Been Tried

(Will be filled)
