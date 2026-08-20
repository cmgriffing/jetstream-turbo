# Ideas Backlog — jetstream-turbo CPU throughput

## Deferred / production-only wins (do NOT move the throughput bench)

- **Owned-string parse in the production ws loop** (`Message::Text` already gives an owned
  `String`; today `parse_message_with_buffers(&text, ...)` copies it). An owned variant
  `parse_message_owned_with_buffers(String, &mut Buffers)` would skip the ~30-40ns/message
  copy. **Unmeasurable in `cpu_throughput`** because the bench must re-run batches over the
  same immutable `raw_jsons` — any owned-input path forces a clone inside the timed region
  (net zero in the bench). Real production win (~2-3%) though; do it for the ws loop.
- Similarly `bench_parse_message_simd_json` calls the copy path; an owned variant exists
  (`bench_parse_message_simd_json_owned`) and measures the same (~equal cost, copy is hidden
  in parse noise).

2. **sqlx store-path serialization buffer reuse** — dead end: `sqlx::Query` holds `&str`
  bind borrows across rows, so a single reused scratch buffer cannot feed the accumulating
  query builder. Owned Strings per record are required by the sqlx API. Don't retry.

## Maybe-worthwhile (below noise floor, high churn)

3. **Raw-record capture** (store the record as raw JSON text instead of a parsed DOM) would
   recover most of the record's parse-build + encode-walk cost (~2ms/batch ≈ 13%), but
   simd-json's serde bridge cannot capture raw spans and `serde_json::RawValue` is
   unsupported by simd-json's Deserializer. Would require a custom CommitData
   Deserialize + Serialize writing raw bytes — high risk, don't chase without a clear
   need.

## Observed / known

- `serde_json::Value` records used BTreeMap (slow build + get). Migrated `CommitData.record`
  to `simd_json::OwnedValue` (ahash HashMap) — big win (see log).
- Machine noise: throughput swings ±5-10% (load avg 2.5-3.0). measure.sh now takes median of
  3 bench invocations (median of 5 batches each) to stabilize.
- CI clippy is pre-existing-broken on this branch: `bluesky.rs:37 large size difference
  between variants` and `hydrator.rs too many arguments` — both present on the original
  commit; not introduced by this session.
- `benches/Cargo.toml` + `tests/Cargo.toml` are separate packages (jetstream-turbo-benches /
  jetstream-turbo-tests) not in the root workspace; root Cargo.toml declares the real benches.

## Dead end (measured, do NOT retry)

4. **Native owned-DOM parse** (`simd_json::to_owned_value_with_buffers` + by-value field
   extraction via `HashMap::remove`, zero-copy `Arc::from(String)`/`String` moves): the
   stage3 tape→DOM conversion is ~1.7x SLOWER than simd-json's serde bridge for these
   small messages. parse phase went 4.3ms → 7.5ms. Reverted (experiment #10).
   Keep the serde bridge in `parse_message_with_buffers`.
