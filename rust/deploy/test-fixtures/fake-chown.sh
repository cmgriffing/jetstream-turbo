#!/usr/bin/env bash
set -euo pipefail
printf '%s\n' "$*" >>"${FAKE_CHOWN_LOG:?}"
