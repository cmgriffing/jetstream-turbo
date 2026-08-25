#!/usr/bin/env bash
set -euo pipefail

script_dir=$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd)
fixture=$(mktemp -d)
trap 'rm -rf "${fixture}"' EXIT

export JETSTREAM_TURBO_DIAGNOSTICS_DIR="${fixture}/diagnostics"
export SYSTEMCTL_COMMAND="${script_dir}/test-fixtures/fake-systemctl-memory.sh"
export JOURNALCTL_COMMAND="${script_dir}/test-fixtures/fake-journalctl-memory.sh"

run_case() {
  local expected=$1
  shift
  env "$@" "${script_dir}/capture-memory-incident.sh"
  # The file is emitted by the local trusted capture script using shell escaping.
  # shellcheck disable=SC1091
  source "${JETSTREAM_TURBO_DIAGNOSTICS_DIR}/latest-termination.env"
  [[ "${incident_class}" == "${expected}" ]]
}

run_case controlled_memory_exit FAKE_RESULT=exit-code FAKE_EXEC_MAIN_STATUS=75
run_case cgroup_oom FAKE_RESULT=oom-kill FAKE_EXEC_MAIN_STATUS=9 FAKE_OOM_KILLS=1
run_case global_oom FAKE_RESULT=signal FAKE_EXEC_MAIN_STATUS=9 \
  'FAKE_KERNEL_EVIDENCE=Out of memory: Killed process 123 (jetstream-turbo)'
run_case application_failure FAKE_RESULT=exit-code FAKE_EXEC_MAIN_STATUS=1

mkdir -p "${JETSTREAM_TURBO_DIAGNOSTICS_DIR}"
printf '{"phase":"containment"}\n' > \
  "${JETSTREAM_TURBO_DIAGNOSTICS_DIR}/latest-memory-incident.json"
view=$("${script_dir}/latest-memory-incident.sh")
grep -q 'class: application_failure' <<<"${view}"
grep -q 'latest-memory-incident.json' <<<"${view}"

printf 'memory incident classification and retained-view tests passed\n'
