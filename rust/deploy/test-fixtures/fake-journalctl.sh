#!/usr/bin/env bash
set -euo pipefail
printf '%s\n' "$*" >>"${FAKE_JOURNAL_LOG:?}"
