#!/usr/bin/env bash
set -euo pipefail

state_dir="${JETSTREAM_TURBO_DIAGNOSTICS_DIR:-/opt/jetstream-turbo/diagnostics}"
termination="${state_dir}/latest-termination.env"
snapshot="${state_dir}/latest-memory-incident.json"

if [[ ! -r "${termination}" ]]; then
  printf 'No retained termination evidence at %s\n' "${termination}"
  exit 1
fi

# The capture file contains only shell-escaped, bounded systemd/kernel fields
# produced by the trusted local capture script. No event payload is persisted.
# shellcheck disable=SC1090
source "${termination}"
printf 'Latest memory incident\n'
printf '  captured_at: %s\n' "${captured_at:-unavailable}"
printf '  class: %s\n' "${incident_class:-unknown}"
printf '  service_result: %s\n' "${service_result:-unavailable}"
printf '  exec_main: %s/%s\n' "${exec_main_code:-unavailable}" "${exec_main_status:-unavailable}"
printf '  cgroup_memory_current: %s\n' "${memory_current:-unavailable}"
printf '  cgroup_memory_peak: %s\n' "${memory_peak:-unavailable}"
printf '  cgroup_oom_kills: %s\n' "${oom_kills:-unavailable}"
if [[ -r "${snapshot}" ]]; then
  printf '  final_snapshot: %s\n' "${snapshot}"
elif [[ -r "${last_snapshot:-}" ]]; then
  printf '  last_snapshot: %s\n' "${last_snapshot}"
else
  printf '  snapshot: unavailable\n'
fi
