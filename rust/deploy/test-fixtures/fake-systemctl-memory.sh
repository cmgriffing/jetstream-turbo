#!/usr/bin/env bash
set -euo pipefail

property=""
for argument in "$@"; do
  case "${argument}" in
    --property=*) property="${argument#--property=}" ;;
  esac
done

case "${property}" in
  Result) printf '%s\n' "${FAKE_RESULT:-success}" ;;
  ExecMainCode) printf '%s\n' "${FAKE_EXEC_MAIN_CODE:-exited}" ;;
  ExecMainStatus) printf '%s\n' "${FAKE_EXEC_MAIN_STATUS:-0}" ;;
  MemoryCurrent) printf '%s\n' "${FAKE_MEMORY_CURRENT:-1048576}" ;;
  MemoryPeak) printf '%s\n' "${FAKE_MEMORY_PEAK:-2097152}" ;;
  OOMKills) printf '%s\n' "${FAKE_OOM_KILLS:-0}" ;;
  *) exit 2 ;;
esac
