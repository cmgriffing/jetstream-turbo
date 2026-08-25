#!/usr/bin/env bash
set -euo pipefail

unit="${JETSTREAM_TURBO_SYSTEMD_UNIT:-jetstream-turbo.service}"
state_dir="${JETSTREAM_TURBO_DIAGNOSTICS_DIR:-/opt/jetstream-turbo/diagnostics}"
systemctl_command="${SYSTEMCTL_COMMAND:-systemctl}"
journalctl_command="${JOURNALCTL_COMMAND:-journalctl}"
mkdir -p "${state_dir}"

property() {
  "${systemctl_command}" show "${unit}" --property="$1" --value 2>/dev/null | head -c 256 || true
}

captured_at="$(date -u +%Y-%m-%dT%H:%M:%SZ)"
result="$(property Result)"
exec_main_code="$(property ExecMainCode)"
exec_main_status="$(property ExecMainStatus)"
memory_current="$(property MemoryCurrent)"
memory_peak="$(property MemoryPeak)"
oom_kill="$(property OOMKills)"
kernel_evidence="$(${journalctl_command} --dmesg --since '-10 minutes' --grep 'oom-kill\|Out of memory\|Killed process' --no-pager -n 8 2>/dev/null | tail -c 4096 || true)"

case "${result}:${exec_main_status}:${kernel_evidence}" in
  *:75:*) incident_class="controlled_memory_exit" ;;
  oom-kill:*|*:137:*) incident_class="cgroup_oom" ;;
  *:*:*"Out of memory"*|*:*:*"Killed process"*) incident_class="global_oom" ;;
  success::) incident_class="none" ;;
  *) incident_class="application_failure" ;;
esac

temporary="${state_dir}/latest-termination.env.tmp"
{
  printf 'captured_at=%q\n' "${captured_at}"
  printf 'incident_class=%q\n' "${incident_class}"
  printf 'service_result=%q\n' "${result:-unavailable}"
  printf 'exec_main_code=%q\n' "${exec_main_code:-unavailable}"
  printf 'exec_main_status=%q\n' "${exec_main_status:-unavailable}"
  printf 'memory_current=%q\n' "${memory_current:-unavailable}"
  printf 'memory_peak=%q\n' "${memory_peak:-unavailable}"
  printf 'oom_kills=%q\n' "${oom_kill:-unavailable}"
  printf 'last_snapshot=%q\n' "${state_dir}/latest-memory-snapshot.json"
  printf 'kernel_evidence=%q\n' "${kernel_evidence:-unavailable}"
} > "${temporary}"
mv "${temporary}" "${state_dir}/latest-termination.env"
