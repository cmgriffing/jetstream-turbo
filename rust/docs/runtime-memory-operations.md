# Runtime memory operations

The production envelope assumes an 8 GiB host. The application starts recovery
at 3072 MiB, confirms soft pressure at 3584 MiB, enters emergency containment at
4608 MiB, and must exit before the 5120 MiB systemd `MemoryMax`. `MemoryHigh` is
4864 MiB, leaving at least 3072 MiB for the OS, monitoring, and restart overlap.
The measured and conservative owner breakdown is recorded in
[`runtime-memory-envelope.md`](runtime-memory-envelope.md).

## Latest incident

Run:

```sh
/opt/jetstream-turbo/current/latest-memory-incident.sh
```

The command correlates the last externally retained phase/sample with systemd's
service result, exit status, cgroup peak/OOM counter, and a bounded kernel-journal
excerpt. Classification precedence is controlled memory exit (status 75), cgroup
OOM, global OOM evidence, then unrelated application failure. No Jetstream event
payloads, DIDs, post URIs, or request fingerprints are retained.

The bounded files are overwritten rather than appended:

- `diagnostics/latest-memory-snapshot.json`: latest material phase/peak sample.
- `diagnostics/latest-memory-incident.json`: final in-process emergency sample.
- `diagnostics/latest-termination.env`: latest post-stop systemd/kernel evidence.

## Rollout and rollback

1. Deploy with `MEMORY_PRESSURE_ACTIONS_ENABLED=false` and
   `MEMORY_EMERGENCY_EXIT_ENABLED=false`; canary the observer and alerts.
2. Enable pressure actions on one canary. Alert on any non-`normal` pressure
   state, sample age above twice the configured interval, cgroup `oom` changes,
   or warmed RSS growth above 64 MiB per settling window.
3. Exercise status-75 controlled exit, then a constrained cgroup OOM, and verify
   the diagnostic command plus monotonic checkpoint replay.
4. Enable the emergency exit switch after the first two stages remain stable.

Rollback by disabling both switches and removing the `MemoryHigh`, `MemoryMax`,
and `MemorySwapMax` directives, while retaining observational sampling. Cache,
SQLx, ingress, and collector hard bounds remain correctness safeguards and are
rolled back only with the code/configuration change that owns them.

## Regression gate

Run the smoke gate with `scripts/runtime-memory-suite.sh`. The scheduled/release
gate sets `MEMORY_SUITE_SCALE=production`; artifacts are written beneath
`target/runtime-memory-artifacts/` and compare memory, throughput, committed lag,
Bluesky request volume, hydration completeness, component limits, backlog, and
checkpoint monotonicity.
