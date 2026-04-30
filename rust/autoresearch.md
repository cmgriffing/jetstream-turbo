# Autoresearch: TurboCache Get Performance

## Objective

Optimize the time to retrieve a value from `TurboCache::get_user_profile`. The benchmark `cache_user_profile_get` measures median time for 1000 get operations after warm cache. Current baseline: ~81,700 ns total (~81 ns per get). Potential improvements:

- Use a faster hash function (switch from `ahash` to `fxhash` or `twister`).
- Reduce metric counter increments (they are atomic; maybe batch or skip if not needed).
- Use ` get` from moka directly with less overhead.

## Metrics

- **Primary**: `cache_get_ns` (nanoseconds, lower is better) — total time for 1000 get ops.
- **Secondary**: `test_status`.

## How to Run

`./autoresearch.sh` runs `cache_user_profile_get` benchmark and outputs `METRIC cache_get_ns=<median_ns>`.

## Files in Scope

- `src/hydration/cache.rs` — `TurboCache::get_user_profile` and builder.
- `Cargo.toml` — maybe change hasher dependency.
- `autoresearch.sh`

## Off Limits

- Do NOT break correctness (cache must return correct values).
- Do NOT remove metrics collection for production observability (but could make optional via feature flag; we'll keep for now).

## Constraints

- All tests must pass.
- No new dependencies (can switch to existing optional hashers if present, but likely we have only `ahash`; we could try `std::collections::hash_map::DefaultHasler`? Not better).

## What's Been Tried

(Will be populated)
