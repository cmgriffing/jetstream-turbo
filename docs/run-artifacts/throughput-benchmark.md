# Throughput benchmark: ordered transition layer (2026-08-31)

`cargo test --release --test throughput -- --ignored --nocapture`

- Processed path (ordered transitions + telemetry + processor + effects):
  1,000,000 record events in 46.68s → **21,422 events/sec**
- Baseline (pre-change aggregation path only, no transition layer):
  1,000,000 records in 46.69s → **21,418 events/sec**

**Conclusion:** the per-event cost is dominated by the existing per-record
comparison-epoch refresh. The ordered transition layer adds no measurable
regression (path matches the old path within noise). Production feed rates
are hundreds of events/sec, leaving two orders of magnitude of headroom.
