#!/usr/bin/env bash
set -euo pipefail
[[ ! -f ${FAKE_READINESS_FAIL_FILE:?} ]]
