# Ideas Backlog — jetstream-turbo CPU throughput

## STATUS: EXHAUSTED (31 experiments, +112-124%, all paths closed)

No open optimization ideas remain within the project constraints. This file is
retained as a durable record of what was tried and why — do NOT treat any entry
below as an open task. The two paths that could yield ~5-10% each require a
constraint lift (a dependency change or a storage-format change) and are marked
as such.

## DONE (production wins implemented — see prompt.md "What's Been Tried")

- Shared simd-json Buffers, single-pass hydrate, ahash maps, MessageContext
  did-dedup, owned ws-loop parse, one-buffer store serialization.
- **Wire-faithful storage** (raw_json capture + write_json).
- **RwLock<HashMap> caches replacing moka** (100x faster get).
- **Slot-indexed hydrate** (no per-message hash lookups).
- **Record-DOM elimination**: `CommitData.record` is now `RecordData` (semantic
  text/reply/embed/facets), extracted leniently from the parser's tape via
  deserialize_any visitors (Cow keys, fully-draining IgnoredAny skips). Storage
  writes raw wire bytes; redis blob splices `message.write_json()` + metadata;
  fixtures build wire JSON + parse. Parse benches 2180→1633ns, hydration batch
  8.0→5.9µs, throughput 468k→993k (+112%).

## Observations / remaining structural costs (all ~at floor within constraints)

- Parse ~5.2ms/10k: simd-json tape build ~3.2ms (includes per-message Tape
  Vec<Node> allocation — internal to the dep, cannot reuse) + envelope Strings
  ~0.6ms + record walker ~0.15ms + raw capture clone ~0.3ms.
- Encode ~1.9ms: message raw write ~0.1ms + metadata ~1.86ms (profile serialize
  ~1.64ms of it at ~1.5GB/s; escape scan + serde dispatch dominate — a hand-
  rolled writer would risk format drift for ~3-5%).
- Hydrate ~1.8ms: extract ~0.03 + dedup slots ~0.25 + RwLock bulk get ~0.05 +
  hydrate_one loop ~0.5 + async/misc.
- Drop ~0.9ms (RecordData + envelope Strings + raw) — halved by the DOM removal.
- `rev`/`cid` canNOT be skipped at parse: they feed `SourceEventId` (recovery
  dedup identity).

## Dead ends (measured, do NOT retry)

## Dead ends (measured, do NOT retry)

- **Hand-rolled metadata/profile JSON writer** (bypassing serde dispatch): with a
  proper aarch64 NEON escape scan (same movemask as value-trait), the manual
  writer is only ~2% faster than `simd_json::to_writer` (2.019ms vs 2.062ms per
  10k) — NOT worth the unsafe SIMD + format-drift risk. Breakdown measured:
  empty-string profiles 146ns/msg (12 fields + 13 keys + nulls = pure field
  structure, which ANY writer must emit) vs full profiles 192ns/msg — so the
  serde dispatch is NOT the cost; the field writes and the NEON scan are both
  already near-optimal. Metadata encode confirmed structural (~1.55GB/s).
- **Per-message tape Vec reuse in parse**: definitively blocked — `Deserializer::
  from_slice_with_buffers` pre-sizes the tape with `with_capacity(previous
  structural_indexes.len())` so churn is only ~1 alloc+free per message
  (~0.2-0.3ms/batch, NOT the 0.7ms growth-churn originally estimated), but
  EVERY deserialize path consumes the tape `Vec` (`Tape::deserialize(self)`, and
  the borrowed `Value` from `as_value()` does not implement `Deserialize`), so
  the allocation cannot be pooled. `into_tape()` returns the Vec after
  deserialization, but refilling it still leaves no non-consuming deserialize
  entry point.
- **Single-record arena serialization in the sqlx store**: Vec growth realloc
  copies (~2x final size) cost as much as the 10k small allocs it would remove;
  no measurable win.
- **Native owned-DOM parse** (`to_owned_value_with_buffers`): stage3 tape→DOM
  ~1.7x slower than the serde bridge. Parse 4.3→7.5ms (#10).
- **sqlx store buffer reuse**: `sqlx::Query` holds `&str` borrows across rows.
- **Raw-record span capture via serde bridge**: no raw-span API; tape
  `Node::Object` carries no byte offsets.
- **Arc<str> for raw_json**: `Arc::from(String)` double-copies (+0.9ms vs
  +0.3ms for String).
- **`&str` map keys in the record walker**: works with the simd-json bridge
  (borrowed keys) but FAILS with serde_json's owned-key path (tests). Use
  `Cow<'de, str>` — zero-alloc borrowed on simd-json, owned on serde_json.
- **Lenient non-draining visitors**: simd-json's bridge advances its tape cursor
  per consumed node — visitors MUST fully drain unexpected maps/seqs
  (`IgnoredAny` recursion) or the cursor desyncs and the next key parse fails.

## Observations

- simd-json serde bridge leaves UNESCAPED inputs byte-identical after parse
  (mutation only rewrites escaped strings). Can't rely on it — clone first.
- moka sync get ≈ 160-560ns vs RwLock<HashMap> ≈ 4-30ns; cache benches 2-6x.
- The malformed-facet semantics of the old `FacetIter` (`?`-termination) differ
  from the new walker (skip-and-continue) — new behavior recovers MORE refs and
  matches `RecordData::from_value`; no test relied on termination.
