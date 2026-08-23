# SQLite schema maintenance

Jetstream Turbo keeps table-wide SQLite work out of normal service startup. Required indexes are reconciled explicitly with the candidate binary before a release is activated.

## Startup work inventory

Before this change, `SQLiteStore::new` performed all of the following synchronously before the HTTP listener could bind:

| Operation | Classification | Current execution mode |
| --- | --- | --- |
| Create `records` when absent | Bounded compatibility setup | Serve and maintenance |
| Create `ingestion_checkpoint` when absent | Bounded compatibility setup | Serve and maintenance |
| Inspect `PRAGMA table_info(records)` | Bounded metadata inspection | Serve and maintenance |
| Add `source_event_id` when absent | Additive metadata-only compatibility change | Serve and maintenance |
| Add `hydration_quality` when absent | Additive metadata-only compatibility change | Serve and maintenance |
| Normalize every invalid `hydration_quality` value | Table-wide data maintenance | Retired; unknown stored values are interpreted as `unknown` without rewriting records |
| Create `idx_records_at_uri` | Table-wide index build | Maintenance only |
| Create `idx_records_did` | Table-wide index build | Maintenance only |
| Create `idx_records_time_us` | Table-wide index build | Maintenance only |
| Create `idx_records_created_at` | Table-wide index build | Maintenance only |
| Create `idx_records_hydration_quality` | Table-wide index build | Maintenance only |
| Create unique partial `idx_records_source_event_id` | Table-wide index build | Maintenance only |

Serve mode now performs a read-only `sqlite_schema` inspection after bounded compatibility setup. Missing or incompatible indexes cause an actionable `SchemaMaintenanceRequired` error before authentication, ingestion, cleanup, or HTTP serving begins. Verification and creation share the declarative `REQUIRED_INDEXES` manifest.

## Run maintenance

From the service working directory, using the same `.env` and `DB_DIR` as production:

```sh
./jetstream-turbo schema-maintenance
```

The default SQLite lock wait is 30 seconds. Configure it with `SQLITE_SCHEMA_MAINTENANCE_BUSY_TIMEOUT_SECS`, or override one invocation:

```sh
./jetstream-turbo schema-maintenance --busy-timeout-secs 60
```

The command does not initialize Bluesky authentication, Redis, ingestion, cleanup, or the HTTP server. It logs start, skip, completion, elapsed time, and terminal outcome for every required index, then exits successfully only after post-creation verification passes.

Expected structured fields include `index`, `lifecycle`, `outcome`, and `elapsed_ms`. A successful repeat reports each index as `skip`/`already_present`; it does not rebuild indexes.

## Lock timeout and retry

If a writer prevents SQLite from acquiring the required lock through the busy-timeout deadline, maintenance exits non-zero with a typed `LockTimeout` naming the affected index or compatibility operation. The active release is not restarted or replaced.

Leave the old service serving, reduce write contention if operationally appropriate, and rerun the same candidate command. Each `CREATE INDEX` is atomic in SQLite, and the command re-verifies the manifest on every run, so a retry safely skips committed indexes and recreates only those still absent. An incompatible index definition is not overwritten automatically; investigate the definition and take an explicit migration action.

## Deployment gates

The Turbo deployment workflow uses these gates:

1. Install the candidate at `/opt/jetstream-turbo/releases/<commit>/jetstream-turbo` without changing the active service.
2. Run the candidate's `schema-maintenance` command as the service user in `/opt/jetstream-turbo`.
3. Abort without activation or restart if maintenance fails.
4. Record the active release in `/opt/jetstream-turbo/previous`, point `current` at the candidate, and restart systemd.
5. Poll `http://127.0.0.1:8080/ready` for at most 30 attempts at two-second intervals.
6. On timeout or connection refusal, emit `systemctl status` and the latest 100 journal entries, restore `current` to the prior release, restart it, and fail the workflow even if rollback succeeds.

Systemd executes `/opt/jetstream-turbo/current/jetstream-turbo`. A process being `active` is not sufficient for deployment success; the localhost readiness endpoint must respond successfully within the deadline. File logs are written beneath `/opt/jetstream-turbo/logs`, outside immutable versioned release directories; if a file appender cannot be opened, the process continues with journal/stdout logging.

## Manual rollback

The activation script rolls back automatically after a readiness failure. For a manual rollback after a later operational issue:

```sh
sudo ln -sfn "$(readlink /opt/jetstream-turbo/previous)" /opt/jetstream-turbo/current
sudo systemctl restart jetstream-turbo
curl --fail --silent --show-error http://127.0.0.1:8080/ready
```

Required indexes are additive and remain in SQLite after rollback; prior releases ignore them. If rollback readiness fails, collect:

```sh
sudo systemctl status jetstream-turbo --no-pager
sudo journalctl -u jetstream-turbo --no-pager -n 100
```

## Local deployment simulation

Run the deterministic shell harness before changing the deployment sequence:

```sh
rust/deploy/test-activate-candidate.sh
```

It covers successful maintenance and activation, maintenance failure without restart, readiness timeout diagnostics, and restoration of the previous release.
