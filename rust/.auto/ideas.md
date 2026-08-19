# Ideas Backlog — cpu_throughput autoresearch

## Considered but deliberately NOT applied

- **Parallel hydrate via tokio::spawn**: The bench creates a multi-thread Runtime and `block_on`s hydrate_batch; parallelizing per-message hydration across workers would cut hydrate wall-time ~2ms. DECLINED: the benchmark is a deterministic single-core CPU-throughput harness (parse/serialize remain single-threaded); parallelizing only hydrate games the wall-clock measurement rather than improving per-core throughput. Could be a legit production architecture change (needs ordering-preserving indexed collection + error semantics), but out of scope for this harness.

## Deferred / blocked optimizations

- **Raw-record splice (BLOCKED — mechanism now precisely understood)**: Store `CommitData.record` as raw JSON bytes captured during parse; emit verbatim during serialize. Would save the record's materialize+walk (RecordValue::serialize currently calls `value()` → full simd-json re-parse of the record raw + OwnedValue serde walk). The vendored simd-json DOES have a raw-emit hook (`RAW_VALUE_TOKEN` splice, se.rs:851) — used by the whole-message splice (jetstream.rs:87) and the profile fragment splice (enriched.rs:39). The record splice is nonetheless blocked by a serializer-duality hazard: `RecordValue` is serialized by BOTH serializers — simd-json (storage path) and serde_json (bench harness line 31 generates the parse input with `serde_json::to_string(m)` on fixtures with `raw_json: None`; tests also use serde_json). Under serde_json, `serialize_newtype_struct(RAW_VALUE_TOKEN, raw)` falls through to a normal string emit → the record would be wrapped in quotes+escaping → wire form corrupts → parse input garbage. RecordValue::serialize can't detect which serializer it's under. So the record splice cannot be done unconditionally; the envelope splice is safe only because parsed messages always carry `raw_json` (checked in the same Serialize impl) and serde_json never serializes those in the harness. No metric impact either way (the bench message is whole-envelope spliced, so the record walk never happens there) — closing the book on this.
- **`simd_json::to_writer` with shared buffer in storage path (PRODUCTION ONLY)**: `sqlite.rs store_batch` serializes message+metadata per record with `to_string` (fresh alloc each). Reusing one `Vec<u8>` + `to_writer` measured ~4.6ms vs ~5.3ms per 10k. The benchmark uses `to_string` so this doesn't move the metric, but it's a real production win.
- **known-key feature for RecordView/parse**: simd-json `known-key` cargo feature precomputes key hashes. Marginal (~0.2ms) and requires manual KnownKey plumbing; not automatic with serde derive.
- **stack-buffer parse copy**: avoid the `text.to_string()` heap alloc for small messages (~0.2-0.4ms). Tried mentally; marginal, adds unsafe/size-threshold complexity.
- **target-cpu=native**: simd-json uses runtime detection by default so little gain; changes build config globally.
- **moka → faster read structure for user cache**: ~0.5ms of resolve phase is moka gets. Would need a new dependency (dashmap etc.) — not worth it.
- **Reduce per-message Instant::now() in hydrate_one** (hydration_time_ms metric): keep — it's real metric data.
- **skip `banner:null` etc. in BlueskyProfile serialization**: format change (missing keys vs null), risky for downstream consumers; ~0.1ms.

## Done (do not re-try)

- Hydrate: ahash sets/maps, single-pass profile resolution, sync hydrate_one, merged traversal, single get/match, Arc<str>-indexed profile attachment, per-batch span.
- OwnedValue record, then wire-form lazy records (RecordValue) + substring-shortcut ref extraction.
- Thread-local ParseScratch reuse; dead Instant removal; raw_json move-not-clone (both paths).
- Raw-message splice via vendored simd-json raw-write hook; metadata profile fragment splice.
- Fast envelope parsers: parse_envelope_shape (fixed-order, +4.6% A/B-verified) + parse_envelope_fast (generic strict) + tape fallback.
- Box<CommitData> struct shrink.
- **Honesty fix**: `#[serde(skip_deserializing)]` on record (was skip — omitted records from fixture wires, inflating measurements).

## Final state (80+ experiments, all kept)

- HONEST steady state ~0.97-1.22M msgs/sec (+105-153% over the honest 474k baseline = 2x), load-dependent (4.7-9). CI-equivalent green, bench untouched.
- All phases at measured floors: parse (fast parsers + captures), hydrate (moka-bound resolve 66ns/get), serialize (splices + harness-locked to_string allocs).
- Declined with reasons: entry-map did_index (production dup allocs), banner-null skip (format risk), mirror cache (unbounded), parallel hydrate (gaming), memchr on short strings (two A/B-confirmed regressions), tape+manual-walk hybrid (measured no-win), skipping the escape check in the shape parser (silent-corruption risk).

## Tested and reverted (round 156)

- **did_index/dids Arc<str> -> CompactString (REVERTED)**: swapped the prepass dedup index keys and the `dids` Vec from `Arc<str>` to `CompactString` (inline, no alloc per unique did). Sub-phase probe showed prepass unchanged (1.55-1.77 -> 1.72ms at similar load); official 1.181M vs 1.174M prior 20-run median — neutral. Likely cause: AHashMap entries grew 16B (Arc fat ptr) -> 24B (inline CompactString), worsening map cache behavior while only removing a ~25ns alloc per unique did. The Arc<str> dids design stays.
- **hydrate_batch sub-phase instrumentation**: prepass ~1.5-1.8ms (did_index + 3x memchr contains + message moves), resolve ~0.7-0.85ms (moka floor), per_msg ~0.5-0.7ms (metadata build floor). The prepass did_index (~0.6-0.7ms of hashing/inserts) is the largest remaining hydrate item but is the production dedup design — no honest lever found.

- **Parse from warm Arc copy (REVERTED, round 157)**: parse_envelope_shape/find_record_span/scratch read `raw_json.as_ref()` (the just-copied Arc, L1-warm) instead of the caller's cold buffer. Official 1.189M == pre-change 1.189M — neutral (saving ~10-20ns/message, inside load noise). Reverted.
