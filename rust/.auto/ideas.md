# Ideas Backlog — jetstream-turbo CPU throughput

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

- **Hand-rolled metadata/profile JSON writer** (bypassing serde dispatch): measured
  SLOWER than `simd_json::to_writer` (2.70ms vs 2.09ms per 10k) — the cost is
  the string escape scan (NEON-accelerated in value-trait on arm64) plus
  unavoidable per-field writes, NOT serde dispatch. Byte-equality prototype
  verified; discarded for speed.
- **Per-message tape Vec reuse in parse**: `to_tape_with_buffers`/`from_str_*
  with_buffers` always allocate a fresh `Vec<Node>`; `Tape::reset` can't feed a
  subsequent parse and lifetimes block Deserializer reuse. The ~50k tape-realloc
  churn per 10k-batch is dep-internal.
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
