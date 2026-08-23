#!/usr/bin/env bash
set -euo pipefail

candidate_binary=${1:?usage: activate-candidate.sh /absolute/path/to/candidate/jetstream-turbo}
deploy_path=${DEPLOY_PATH:-/opt/jetstream-turbo}
service_name=${SERVICE_NAME:-jetstream-turbo}
binary_name=${BINARY_NAME:-jetstream-turbo}
readiness_url=${READINESS_URL:-http://127.0.0.1:8080/health}
readiness_attempts=${READINESS_ATTEMPTS:-30}
readiness_interval_secs=${READINESS_INTERVAL_SECS:-2}
systemctl_bin=${SYSTEMCTL_BIN:-systemctl}
journalctl_bin=${JOURNALCTL_BIN:-journalctl}
curl_bin=${CURL_BIN:-curl}
run_as_service_user=${RUN_AS_SERVICE_USER:-1}

current_link="$deploy_path/current"
previous_link="$deploy_path/previous"
candidate_release=$(dirname "$candidate_binary")

if [[ ! -x "$candidate_binary" ]]; then
    echo "Candidate binary is not executable: $candidate_binary" >&2
    exit 2
fi

run_maintenance() {
    if [[ "$run_as_service_user" == "1" ]]; then
        runuser -u "$service_name" -- bash -c 'cd "$1" && exec "$2" schema-maintenance' -- "$deploy_path" "$candidate_binary"
    else
        (cd "$deploy_path" && "$candidate_binary" schema-maintenance)
    fi
}

poll_readiness() {
    local attempt
    for ((attempt = 1; attempt <= readiness_attempts; attempt += 1)); do
        if "$curl_bin" --fail --silent --show-error --max-time 2 "$readiness_url" >/dev/null; then
            echo "Readiness passed on attempt $attempt/$readiness_attempts"
            return 0
        fi
        if (( attempt < readiness_attempts )); then
            sleep "$readiness_interval_secs"
        fi
    done
    return 1
}

emit_diagnostics() {
    "$systemctl_bin" status "$service_name" --no-pager || true
    "$journalctl_bin" -u "$service_name" --no-pager -n 100 || true
}

mkdir -p "$deploy_path/releases"

# Bootstrap the versioned layout without touching the currently running process.
if [[ ! -L "$current_link" && -x "$deploy_path/$binary_name" ]]; then
    legacy_release="$deploy_path/releases/legacy-$(date -u +%Y%m%d%H%M%S)"
    mkdir -p "$legacy_release"
    cp "$deploy_path/$binary_name" "$legacy_release/$binary_name"
    chmod 0755 "$legacy_release/$binary_name"
    ln -sfn "$legacy_release" "$current_link"
fi

echo "Running candidate schema maintenance: $candidate_binary"
run_maintenance

old_release=""
if [[ -L "$current_link" ]]; then
    old_release=$(readlink "$current_link")
    ln -sfn "$old_release" "$previous_link"
fi

ln -sfn "$candidate_release" "$current_link"
"$systemctl_bin" enable "$service_name"
"$systemctl_bin" restart "$service_name"

if poll_readiness; then
    echo "Candidate activated and ready: $candidate_release"
    exit 0
fi

echo "Candidate failed readiness deadline: $candidate_release" >&2
emit_diagnostics

if [[ -n "$old_release" ]]; then
    echo "Rolling back to previous release: $old_release" >&2
    ln -sfn "$old_release" "$current_link"
    "$systemctl_bin" restart "$service_name"
    if ! poll_readiness; then
        echo "Rollback release also failed readiness" >&2
        emit_diagnostics
    fi
fi

exit 1
