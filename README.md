# Jetstream Turbo Rewrites

This repo is aimed at creating other language versions of jetstream-turbo by graze.social.

It is vibe coded from the original source code. The LICENSE for the python code remains unchanged as well as the source code itself. I just moved the python code into its own folder so that I could use the source code as context for OpenCode.

The original source code is available at https://github.com/graze-social/jetstream-turbo

## Engineering notes

- [Stream comparison and recovery exploration](docs/stream-comparison-recovery-exploration.md) — production evidence and rationale for source-window comparison and convergent Rust recovery.
- [Operations runbook](docs/operations-runbook.md) — cgroup telemetry verification, VACUUM safety gating and authenticated override (`MAINTENANCE_API_KEY`), replay/checkpoint rollback switches, alert thresholds, and gate artifacts.
- [Run artifacts](docs/run-artifacts/restore-replay-convergence/) — production-scale memory-gate evidence (convergence, concurrency sweep, baseline comparison).

## Benchmarks

The Rust benchmark suite is organized into three tiers, one file per tier, under `rust/benches/`:

| Tier | File | What it measures | Metric |
|------|------|------------------|--------|
| 1 | `cpu_hot_path.rs` | Isolated CPU hot-path microbenchmarks | ns/op (criterion) |
| 2 | `cpu_throughput.rs` | End-to-end CPU throughput (parse → hydrate → serialize) | msgs/sec |
| 3 | `regression.rs` | Cache, SQLite, hydration, full-pipeline, and progress-tracker guards | ns/op (criterion) |

### Tier 1 — CPU hot path

Measures the production CPU-bound stages in isolation using `simd_json`:

- `parse_message_simd_json` — the production `parse_message` path, including the input copy.
- `parse_message_simd_json_owned` — a String-by-value parse variant (no input copy).
- `record_view_extract_refs` — `RecordView` facet/reply/embed reference extraction.
- `simd_json_serialize_record` — message + metadata serialization via `simd_json`.
- `extract_at_uri` — AT-URI string building.

### Tier 2 — Throughput

`cpu_throughput` is a custom `harness = false` harness that times parse → hydrate (cache-hit) → serialize over a large fixed batch with I/O mocked, and prints a single `msgs/sec` number.

### Tier 3 — Regression guards

Retains the cache get/set/bulk benchmarks (`cache_user_profile_set`, `cache_user_profile_get`, `cache_post_set`, `cache_post_get`, `cache_bulk_get_user_profiles`, `cache_bulk_get_posts`), SQLite stores (`sqlite_store_record`, `sqlite_batch_store`), hydration (`single_message_hydration`, `batch_hydration_25_messages`), full-pipeline (`full_pipeline_single_message`, `full_pipeline_batch_25`), and progress-tracker (`progress_tracker_ingress_update`, `progress_tracker_batch_boundaries`) benchmarks as regression guards.

### Running the benchmarks

```bash
cd rust

# All tiers
cargo bench

# A single tier
cargo bench --bench cpu_hot_path
cargo bench --bench cpu_throughput
cargo bench --bench regression

# Throughput with a custom batch size
THROUGHPUT_BATCH_SIZE=10000 cargo bench --bench cpu_throughput
```

CI runs all three tiers on the main branch and the candidate and enforces per-tier regression thresholds (Tier 1: 2%, Tier 2: 5%, Tier 3: 5%).
