# Domain Glossary

Shared vocabulary for the jetstream-turbo codebase. Use these terms consistently in code, comments, and architecture discussions.

## Core concepts

**Jetstream** — The AT Protocol firehose producing raw events (commit, identity, account) over WebSocket. Messages arrive as JSON.

**Message** / **JetstreamMessage** — A single inbound Jetstream event. Contains a DID, optional timestamp/sequence, a `kind` (commit/identity/account), and optionally a `CommitData` payload.

**CommitData** — The commit payload within a Jetstream message. Contains a revision (`rev`), operation type, collection, rkey, an optional raw record JSON blob, and a CID.

**Hydration** — The process of enriching a JetstreamMessage by fetching related profiles, referenced posts, and extracting content features (hashtags, mentions, URLs, language). Produces an `EnrichedRecord`.

**EnrichedRecord** — A JetstreamMessage after hydration. Wraps the original message plus `HydratedMetadata` (profiles, referenced posts, content features) and `ProcessingMetrics`.

**Record** — The raw JSON blob in `CommitData.record`. Conforms to the AT Protocol lexicon for a given collection (e.g., `app.bsky.feed.post`).

**RecordView** — A read-only, zero-allocation lens over a record's raw JSON (`&serde_json::Value`). Exposes semantic accessors for facets, reply references, embed URIs, and text without duplicating JSON traversal across callers.

**Facet** — A rich-text annotation on a post. Has byte start/end indices and an array of typed features: tag (hashtag), link (URL), or mention (user DID).

**Profile** / **BlueskyProfile** — A user profile fetched from the Bluesky API. Identified by DID. Contains handle, display name, avatar, follower counts, etc.

**Post** / **BlueskyPost** — A full post fetched from the Bluesky API. Contains the text, author profile, embed, reply info, facets, and engagement counts.

**Cache** / **TurboCache** — In-memory cache (Moka) for profiles and posts with TTL eviction. Backed by hit/miss metrics.

**RecordStore** — Stores enriched records to durable storage (SQLite in production). Adapter trait.

**EventPublisher** — Publishes enriched records to a stream (Redis in production). Adapter trait.

**Turbocharger** — The orchestrator that wires together message ingestion, hydration, storage, publishing, and broadcasting.

**DID** — Decentralized Identifier. Identifies an AT Protocol user (e.g., `did:plc:abc123`).
