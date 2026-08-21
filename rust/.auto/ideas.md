# Ideas Backlog — jetstream-turbo CPU throughput

## DONE (production wins implemented — see prompt.md "What's Been Tried")

- Shared simd-json Buffers, single-pass hydrate, ahash maps, OwnedValue record,
  MessageContext did-dedup, owned ws-loop parse, one-buffer store serialization,
  **wire-faithful storage** (raw_json capture + write_json), **RwLock<HashMap>
  caches replacing moka** (100x faster get), **slot-indexed hydrate** (no
  per-message hash lookups).

## PARKED — Record-DOM elimination (biggest remaining lever, ~20-25%)

The record JSON DOM (`CommitData.record: Option<simd_json::OwnedValue>`) is now
PURE OVERHEAD: nothing needs it anymore (stored message = raw wire bytes). Its
costs per 10k batch: parse-build ~1.6ms + drop ~1.2-1.8ms + extract walk ~0.1ms.
Eliminating it (extract refs from the tape during parse, keep semantic
`RecordData` instead of the DOM) is worth ~2.2-2.8ms ≈ +25% throughput.

**Why parked:** `serde::Serializer` cannot emit raw JSON, so changing the record
type breaks the serde round-trip used by (a) the redis publication blob
(`serde_json::to_string(&EnrichedRecord)`, src/storage/redis.rs:161), (b) bench
input construction (`serde_json::to_string(&fixture_message)`), (c) ~15 test
construction sites. A byte-faithful splice would need redis to build its blob
manually around `message.write_json()` and fixtures to construct via parse. That
plus a custom tape-walker for ref extraction (correctness risk on exotic
records) = 2-3h surgical change. Design if attempted:
  - `CommitData.record: Option<RecordData>` where RecordData = { text,
    reply_parent_uri, reply_root_uri, embed_record_uri, facets[] } extracted by
    a custom Deserialize over the simd-json bridge (walk map/array nodes).
  - RecordView reworked over RecordData (API change); get_text reads
    RecordData.text; extract_refs_from_view reads RecordData directly.
  - redis `publication_values` builds the blob with message.write_json() + the
    rest via serde (key order preserved: message, hydrated_metadata,
    processed_at, metrics). Redis test asserts only source_event_id/message_id.
  - fixtures switch to constructing raw wire JSON + parse (raw_json set), so
    bench inputs stay faithful.
  - The `record_view_extract_refs` hot-path bench reworks to measure extraction
    from wire text.

## Dead ends (measured, do NOT retry)

- **Native owned-DOM parse** (`to_owned_value_with_buffers` + HashMap::remove
  field extraction): stage3 tape→DOM ~1.7x SLOWER than the serde bridge for
  small messages. Parse 4.3→7.5ms. Reverted (#10).
- **sqlx store buffer reuse**: `sqlx::Query` holds `&str` borrows across rows; a
  single reused scratch cannot feed the accumulating query builder. One owned
  String per row is required by the sqlx API.
- **Raw-record span capture via serde bridge**: simd-json's bridge exposes no
  raw spans and `serde_json::RawValue` is unsupported; `Node::Object` on the
  tape carries no byte offset, so raw text cannot be recovered post-parse.
  (The whole-MESSAGE raw is captured instead by cloning the owned input BEFORE
  parse — simd-json rewrites escaped strings in place, verified.)
- **Arc<str> for raw_json**: `Arc::from(String)` copies bytes a second time
  (2 allocs+copies vs 1 for String) — parse +0.9ms vs +0.3ms. Use String.

## Observations

- simd-json serde bridge leaves UNESCAPED inputs byte-identical after parse
  (mutation only rewrites escaped strings). Can't rely on it — clone first.
- moka sync cache get ≈ 160-560ns (shard lock + TTL machinery) vs RwLock<HashMap>
  ≈ 4-30ns. Cache benches improved 2-6x after the swap.
- Drop of a 10k batch of parsed messages ≈ 1.7-1.9ms (record DOM ~8 deallocs +
  4 envelope Strings + raw_json). Allocator-bound; tied to the record DOM.
- Encode metadata (~1.9ms) is structural: each bench message has a UNIQUE
  profile (no dedup possible); profile serialize ≈ 220ns/msg is field-walk +
  string writes.
- Parse floor: simd-json tape build ≈ 2.8ms/10k (~280ns/msg); envelope strings
  (did/rev/collection/rkey/cid) ≈ 0.7ms; record DOM build ≈ 1.6ms.
