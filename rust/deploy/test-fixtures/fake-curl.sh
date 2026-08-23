#!/usr/bin/env bash
set -euo pipefail
[[ ${!#} == "http://127.0.0.1:8080/ready" ]]
[[ ! -f ${FAKE_READINESS_FAIL_FILE:?} ]]
