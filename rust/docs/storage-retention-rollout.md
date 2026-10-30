# Storage retention rollout and recovery

This release separates fresh API entities from recovery state. Defaults are 300 seconds for each entity TTL, a 4096 MiB logical entity cache, a 6144 MiB aggregate SQLite target, and a 16384 MiB filesystem free-space reserve. `MAX_DB_SIZE_MB` now includes main-file allocation, indexes, reusable freelist, and WAL; it is an operational target with bounded in-flight growth, rather than a filesystem quota. Cleanup runs every 60 seconds even below target. Completed payloads become compact receipts; unpublished payloads, ambiguous legacy records, uncovered receipts, and the singleton checkpoint stay protected.

Receipt expiry has its own cleanup loop. Each transaction deletes at most `CLEANUP_CHUNK_SIZE` rows (capped at 10,000), yields between chunks, and finishes its current transaction before checking a 250 ms run budget. If a full chunk reaches that budget, cleanup resumes after 1 ms rather than waiting for the normal interval. This lets expiry catch up with sustained ingestion while preserving replay overlap and yielding to other database work.

`MAX_DB_SIZE_MB` must exceed the admission growth allowance, which is the greater of 2048 MiB and four times `IN_FLIGHT_PAYLOAD_LIMIT_MB`. Startup rejects smaller or equal targets instead of starting an empty store with permanently blocked admission. The existing cache budget must also remain below the aggregate target.

Observed post updates/deletes invalidate that post. Identity, account, and profile changes invalidate that author and cached posts from the author. Unrelated positive entries and temporary-failure suppression survive. Bounded fences reject older in-flight refreshes and prevent delayed missing/error outcomes from replacing newer successful results. Evicting an old fence raises the minimum acceptable fetch token while retaining valid cache entries. If pending disk invalidations exceed their queue bound, the disk tier conservatively clears instead of dropping them. A missing API entity invalidates only its own key, so successful peers in the same response can still enter the persistent cache.

The existing reported 33 GB database and 2.4 GB log require offline preparation. No command below authorizes removing the only recovery copy. An ordinary 33 GB same-volume VACUUM cannot fit on an assumed 40 GB disk. Use measured capacity, alternate storage, or expansion. Binary GiB and decimal drive GB differ: a nominal 40 GB disk has about 37.25 GiB before filesystem overhead.

## Measure and reserve capacity

Locate `DB_DIR/jetstream.db`, the service working directory, and both existing main and `-batches.log` files. Record:

```sh
systemctl show jetstream-turbo -p WorkingDirectory -p EnvironmentFiles -p MainPID
findmnt -T /opt/jetstream-turbo
findmnt -T /mnt/staging
df -B1 /opt/jetstream-turbo /mnt/staging
du -B1 /opt/jetstream-turbo/data/jetstream.db*
du -B1 /opt/jetstream-turbo/logs/*
journalctl --namespace=jetstream-turbo --disk-usage
```

Adjust paths to the actual configuration before executing commands. Include Python/Monitor databases, logs, host journal, OS/files, filesystem metadata, and backup destinations in the capacity inventory. Do not sum `page_count * page_size` and main-file bytes: those are two views of the same allocation. Capture page count, page size, freelist count, WAL bytes, checkpoint values, journal count, and published/protected identity counts using read-only SQLite inspection after stopping writers.

The initial 40 GB rehearsal allocates 6 GiB SQLite + 16 GiB free reserve + 0.5 GiB application logs + 0.5 GiB service journal = 23 GiB. A measured 37.25 GiB filesystem leaves at most 14.25 GiB for all other users, journal granularity, and operational margin. The 4 GiB logical entity budget is inside the 6 GiB target. Index/WAL/recovery overhead can reduce usable cache allocation; protected state can require more capacity. These are capacity calculations, not evidence of production request savings or throughput.

Ordinary VACUUM checks each affected volume. On a shared volume, required free bytes are `max(16 GiB, 2 * main database bytes + growth allowance)`, where growth includes an emergency floor of at least 2 GiB. Separate temp and database/WAL volumes each need their respective allocation and emergency headroom. Changing only `VACUUM_TEMP_DIR` leaves the original WAL on its own volume. Scheduling overrides never bypass these checks.

## Drain, stop, and make a consistent off-volume backup

1. Schedule the stop inside the upstream replay-availability window. Record current checkpoint/cursor and downstream deduplication support. Arrange a longer supported replay horizon before deliberately rewinding farther than the configured overlap. Completion receipts retain at least `max(3600 seconds, cursor overlap)` behind a covered source boundary.
2. Stop ingestion through the normal service shutdown so admitted batches drain and the final contiguous checkpoint flushes. Confirm the process exited and all database/log writers are closed. If shutdown must abort a batch, preserve the publication journal and use the existing at-least-once replay recovery path.
3. Verify the service remains stopped during migration. Inspect open handles with `lsof` or the host equivalent. Do not delete a live WAL, SHM, rollback journal, or log file held open by a writer.
4. Use SQLite's backup API (or the SQLite shell `.backup`) to produce an independent consistent snapshot on alternate storage. A filesystem copy of only the main file while WAL exists is insufficient. Check `PRAGMA integrity_check`, exact checkpoint values, and the inventory of pending payloads and identities on the backup. Preserve this backup untouched.
5. Use another path for the migration working copy. The migration command uses SQLite backup again before changing the working copy. Its conservative staging preflight reserves three times the source allocation plus 2 GiB on the working staging volume for the working copy, rollback/index work, and verification sorts. A shared working/output volume uses that same requirement; on separate volumes the output volume additionally needs one source allocation plus 2 GiB. A 33 GB source may therefore need over 99 GB free on a shared staging volume. SQLite sort/index temporaries use a private directory on the measured working staging volume, rather than the VPS default `/tmp`. Provision this capacity rather than bypass the preflight.

The offline command needs only Python 3 and its SQLite module; it does not read Bluesky credentials, connect to Redis, or start ingestion. The release also includes `current/migrate_storage.py`; the examples below invoke the repository copy from its root:

```sh
python3 rust/scripts/migrate_storage.py migrate \
  --source /mnt/backup/jetstream-consistent.db \
  --working /mnt/staging/jetstream-working.db \
  --output /mnt/staging/jetstream-replacement.db \
  --target-mb 6144 --writers-stopped

python3 rust/scripts/migrate_storage.py verify \
  --manifest /mnt/staging/jetstream-replacement.db.manifest.json \
  --temp-dir /mnt/staging
```

Use new paths for every retry. Existing paths are never silently overwritten. Failed migration leaves the source unchanged and produces no install manifest. Preserve failed working/output files for diagnosis or deliberately remove only those disposable copies after checking paths.

Migration first repairs the legacy batch-journal bug on the working copy: origin/main stored one journal identity per SQL chunk, while every record in the batch shares its `hydrated_at` timestamp with the surviving journal entry's `created_at`. All matching timestamp peers retain full payloads and receive missing journal entries before any compaction. Timestamp collisions conservatively retain extra rows; missing/invalid timestamps with stable identities are journaled conservatively, and null-identity rows remain full. Migration creates receipts only when a stable source identity and known publication state permit it. Legacy published identities without a journal entry become uncovered receipts; source timestamps alone do not prove checkpoint coverage. Published work ahead of a stalled frontier therefore keeps its identity. Null/ambiguous identities retain full payloads, and a store without a publication journal retains all legacy payloads. Entity caches start empty because historical enriched objects do not establish API fetch freshness. A compacted protected set larger than the target is refused for installation and requires more capacity or resolution of the recovery backlog.

Verification compares exact checkpoint rows, every original column of pending/ambiguous payloads, journal hashes, and the complete union of recovery identities; runs SQLite integrity checking; and verifies required index definitions. A whole-file SHA-256 checksum in the manifest detects changes to receipt coverage metadata and any other output bytes before verification or installation. The version-2 manifest distinguishes the untouched source signature from the normalized replacement journal signature. Install checks the former for stale-source detection, and output verification checks the latter. Pre-repair version-1 manifests are refused. Failed verification cannot produce an installable result.

## Install a verified replacement

Keep writers stopped. Confirm that the independent off-volume backup and verified replacement remain readable. If the original 33 GB database prevents coexistence with the replacement plus reserve, move its stopped, consistently checkpointed database to alternate capacity first, or expand the disk. Account for all old sidecars and verify the backup before relocating anything. Do not unlink an active database or assume an off-volume copy freed local space.

Stage the compacted replacement on the destination filesystem using a temporary name. Copy its manifest with it, change only the manifest's `output` path to the staged absolute path, and run `verify` again. Installation requires at least 16384 MiB measured free after staging; use the configured reserve if intentionally choosing another host profile. Verification and installation also require measured writable sort staging: at least one replacement allocation plus 2 GiB for verification, or the larger of replacement/original-destination allocation plus 2 GiB for installation. `--temp-dir` selects that volume; its default is the replacement directory. Use off-volume staging when local sort headroom is insufficient. All replacement/destination `-wal`, `-shm`, and `-journal` files and symlinks must be absent. If sidecars exist, stop all handles and checkpoint/close the corresponding database deliberately; never pair old sidecars with a new main file.

```sh
python3 rust/scripts/migrate_storage.py verify \
  --manifest /opt/jetstream-turbo/data/replacement.db.manifest.json \
  --temp-dir /mnt/staging

python3 rust/scripts/migrate_storage.py install \
  --manifest /opt/jetstream-turbo/data/replacement.db.manifest.json \
  --destination /opt/jetstream-turbo/data/jetstream.db \
  --rollback /opt/jetstream-turbo/data/jetstream.pre-retention.db \
  --min-free-mb 16384 --temp-dir /mnt/staging --writers-stopped
```

When a destination still exists, installation requires its checkpoint/protected state to match the migrated snapshot, creates an exclusive rollback hardlink, syncs it, then atomically replaces the destination and syncs the directory. Interruption before the rename leaves the original live path; interruption after it leaves the verified replacement. A rollback hardlink retains the original disk allocation until deliberately moved or removed, so it does not solve the 33 GB coexistence problem. If the original has already been safely moved off-volume, installation can target an absent destination; the independent backup is then the rollback source. Set ownership and permissions for the service before restarting.

## Migrate old logs and activate the policies

With writers stopped and drained, inspect the exact old main and batch paths. Move them to a measured off-volume archive if retention is required, or remove only the confirmed legacy files after deciding their disposition. Do not use broad wildcard deletion. An archive on the same filesystem retains its space; an off-volume archive consumes its destination capacity. Verify `df`, `du`, and open handles after disposition, including the reported 2.4 GB file, before declaring the new application budget enforced.

Install the namespace-scoped journal policy using the [logging deployment preflight and legacy-file tooling](log-retention.md). Unsupported namespace isolation blocks rollout; changing or vacuuming the shared host journal is not a fallback. The 512 MiB journal allocation reserves active-file headroom, but journald rotates at file granularity. Measure actual namespace usage and residual overshoot at startup and during sustained traffic. Application files rotate before 100 MiB or the next UTC day, expire within the next successful 60-second cleanup after seven days, and share a 512 MiB budget that may shorten history.

Set the documented host defaults explicitly in the environment. Preserve deliberate existing operator values only after checking their capacity implications. Start with cold entity caches and independent 300-second TTLs. After startup:

- Confirm required schema verification, exact durable checkpoint, pending journal recovery, and replay duplicate suppression; inspect published source identity continuity.
- Confirm allocation/WAL/freelist/free-space gauges and disk admission/readiness. An oversized store may start gated for diagnostics and safe cleanup; it must not admit persistent growth or attempt unsafe compaction.
- Exercise cache-disabled fallback, restart within 300 seconds, expiry, a stalled frontier with later completed work, and low-free-space admission/recovery in staging.
- Compare actual API request counts, fetched entity counts, persistent hits, throughput, output freshness, queue drops, and SQLite contention over matched traffic windows. A saved entity is not necessarily a saved batched HTTP request.
- Observe quiet-period log expiry and combined stream allocation, namespace disk usage, sink drops/failures, maintenance deferrals, and protected-state overage.

Record observed results and rollout acceptance in the task/change evidence. Do not increase TTLs or cache allocation until measured reuse and acceptable emitted-data freshness support the change. Production measurement remains a rollout precondition; synthetic tests cannot establish production request savings.

## Roll back safely

Disable persistent entity lookup using `HYDRATION_DISK_CACHE_ENABLED=false` to return to RAM/API hydration while retaining the new binary's receipts and recovery behavior. This is the compatible, reversible cache rollback.

An older binary that only recognizes full historical records cannot safely read a store after payloads were compacted into receipts. Do not simply downgrade it against the migrated store. Use a verified compatible recovery snapshot or an explicit tested migration/replay procedure. After any new ingestion, restoring an older snapshot rewinds durable progress: first verify upstream replay availability over the entire rewind interval and downstream source-event deduplication/duplicate-publication behavior. If replay is no longer available, restoring the snapshot cannot recover missing source events. Preserve the current store and checkpoint before any restoration, and never claim exactly-once external publication.
