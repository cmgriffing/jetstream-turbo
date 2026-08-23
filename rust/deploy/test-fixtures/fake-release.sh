#!/usr/bin/env bash
set -euo pipefail

if [[ ${1:-} != "schema-maintenance" ]]; then
    exit 2
fi
if [[ -f ${FAKE_MAINTENANCE_FAIL_FILE:?} ]]; then
    exit 42
fi
