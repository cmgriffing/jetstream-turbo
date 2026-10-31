# Application and service journal retention

Both application streams use one bounded queue and one file owner. Defaults are
100 MiB segments, rotation at the next UTC-day write, seven-day retention,
512 MiB shared file bytes, 256 queued records, and 64 KiB per record. A formatted
record over the limit is dropped as one event. Queue overflow never blocks a
producer. Keep the `LogOwner` guard until shutdown to drain accepted events.
The queue holds at most 16 MiB of record data with these defaults; each active
formatter also buffers at most one bounded record. Concurrent pipeline producers
are separately bounded by the existing pipeline concurrency settings.

The owner holds an exclusive filesystem lock on
`.jetstream-turbo-log-owner.lock` until queued writes finish and its files close.
A second process cannot manage the same directory. Leave this zero-length lock
file in place: removing it while the service is running would let another process
lock a different inode. Schema maintenance logs only to stdout and does not open
or scan the application's managed-file directory.

Managed names are exactly
`jetstream-turbo.{main|batches}.<first-entry-unix-second>.<canonical-uuid>.log`.
Only matching regular files are managed; symlinks, legacy names and unrelated
files are never deleted. New entries cannot extend the oldest entry's retention:
expiry uses the segment's first-entry time. The owner runs cleanup at startup,
before each append and at least every 60 seconds while quiet. It closes an active
segment before removing it. No maintenance copies or temporary files are created.
The budget counts the lengths of all managed files, including both active files.
At most 1024 managed segments are retained, including empty files; this count
limit can shorten history independently of the byte and age limits. Startup
enforces the count limit while scanning, keeping its inventory bounded.
Filesystem block allocation and unrelated/legacy files still belong in host `du`
and `df` capacity measurements. External writes to owned segments are unsupported.

If deletion fails and the next record cannot fit, that file append is rejected.
Open/write/delete failures, queue drops, oversized records, and budget drops have
bounded diagnostic fields in health and metrics. Sink failures emit at most one
independent stderr warning per minute; they do not recursively log to the failing
file sink. The independent stdout/journal subscriber remains available. The file
writer does not promise power-loss durability and is not recovery storage.

An open or write failure pauses file-write attempts for 60 seconds. Events
received during that backoff count as I/O drops; stdout continues independently.
Empty failed segments are removed immediately, while partial writes still count
against the shared budget. The segment-count limit also bounds accumulation if
removing a failed segment is unsuccessful.

`LOG_SEGMENT_MAX_MB`, `LOG_TOTAL_MAX_MB`, `LOG_RETENTION_SECONDS`,
`LOG_QUEUE_CAPACITY`, and `LOG_MAX_RECORD_BYTES` configure the policy. Byte budgets
are binary MiB. Routine batch/capacity/checkpoint successes are debug events;
periodic batch summaries, failures and recovery transitions remain visible.

## Namespace rollout

Run `sudo bash rust/deploy/configure-journal.sh preflight` before installing the
service. It requires systemd >= 245, `journalctl --namespace`, and the namespaced
journal daemon template. Unsupported isolation refuses installation before policy
changes; there is no shared-host fallback. The service uses
`LogNamespace=jetstream-turbo`. The only installed policy file is
`/etc/systemd/journald@jetstream-turbo.conf`.

The isolated policy sets a 384 MiB operational retention limit, 16 MiB journal
segments, seven-day retention and one-day rotation, with 128 MiB reserved for
active-file/rotation headroom inside a 512 MiB allocation. `SplitMode=none` reduces
active streams. These systemd settings are operational limits with filesystem and
active-file granularity; they do not impose an exact quota or strict per-entry age
cap. At installation and after activation, `configure-journal.sh check` verifies
the service's effective namespace, reports actual allocated and active bytes from
both persistent and runtime namespace directories, and fails above either the
512 MiB allocation or 128 MiB active headroom. Recheck under actual traffic and
periodically in operations. Increase measured active headroom only by reducing the
retention allocation, or deliberately revise the host capacity model. Residual
overshoot must remain visible and accounted for; do not silently claim it fits.

Inspect only this service's journal with:

```sh
sudo journalctl --namespace=jetstream-turbo -u jetstream-turbo.service
sudo bash rust/deploy/configure-journal.sh check
```

The old shared journal history is unaffected. Do not run a global journal vacuum
or change `/etc/systemd/journald.conf` to enforce this service's allocation.
See the systemd documentation for [namespaces](https://www.freedesktop.org/software/systemd/man/latest/systemd-journald.service.html),
[`LogNamespace`](https://www.freedesktop.org/software/systemd/man/latest/systemd.exec.html),
and [journal retention settings](https://www.freedesktop.org/software/systemd/man/latest/journald.conf.html).

## Existing 2.4 GB unrotated log

The deployment refuses activation while either exact old name remains in
`/opt/jetstream-turbo/logs`: `jetstream-turbo.log` or
`jetstream-turbo-batches.log`. Do this during the storage maintenance window:

1. Measure `df -h`, `du -h` and archive destination capacity. Select external or
   expanded storage if retaining old logs on the VPS would break the capacity plan.
2. Stop the service and wait until it has exited and drained its logging guard.
   Confirm `systemctl show jetstream-turbo -p ActiveState -p MainPID` reports
   inactive/failed and PID 0. Inspect the exact two paths; do not use wildcards.
3. Run `sudo DEPLOY_PATH=/opt/jetstream-turbo bash
   rust/deploy/prepare-legacy-logs.sh archive /existing/archive/destination`.
   The tool checks service state and `fuser` for open descriptors before moving
   either file, and checks off-volume copy headroom. It refuses symlinks.
4. Verify the archive, source free-space change and destination allocation. A move
   within the same filesystem reclaims no space. A cross-volume move temporarily
   needs both copies; count destination storage. Removal instead of archival is an
   explicit operator decision after any required log backup.
5. Run `prepare-legacy-logs.sh check`, install the new policy and binary, then
   verify managed segments, `/api/v1/health` sink counters, namespace usage and `df`.

New retention does not automatically delete or claim the old shared journal or
unrotated files as reclaimed space. The application does not touch production
files until the operator runs the rollout.

## Candidate rollback compatibility

Each packaged release declares `completion_receipts_v1` in its `recovery-format`
file. Candidate activation requires that marker before schema maintenance. On a
failed readiness check, automatic rollback is allowed only to a previous release
with the same declared receipt-aware format. If the previous release is unproven,
activation stops the service, keeps the candidate link and database intact, and
reports the compatible-state recovery procedure. It never restarts a legacy
binary against a database whose full payloads may already have been pruned.
Do not add this marker to an older binary as a workaround: it is a declaration
made by the matching release build. A snapshot restore still needs the replay
and downstream deduplication preflight in the storage rollout runbook.
