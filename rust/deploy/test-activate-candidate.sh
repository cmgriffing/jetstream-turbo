#!/usr/bin/env bash
set -euo pipefail

script_dir=$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)
fixture=$(mktemp -d)
trap 'rm -rf "$fixture"' EXIT

deploy_path="$fixture/deploy"
fake_bin="$fixture/bin"
mkdir -p "$deploy_path/releases/old" "$deploy_path/releases/candidate" "$fake_bin"
ln -sfn "$deploy_path/releases/old" "$deploy_path/current"

write_release_binary() {
    local path=$1
    cp "$script_dir/test-fixtures/fake-release.sh" "$path"
    chmod 0755 "$path"
}

write_release_binary "$deploy_path/releases/old/jetstream-turbo"
write_release_binary "$deploy_path/releases/candidate/jetstream-turbo"

cp "$script_dir/test-fixtures/fake-systemctl.sh" "$fake_bin/systemctl"
cp "$script_dir/test-fixtures/fake-curl.sh" "$fake_bin/curl"
cp "$script_dir/test-fixtures/fake-journalctl.sh" "$fake_bin/journalctl"
chmod 0755 "$fake_bin/systemctl" "$fake_bin/curl" "$fake_bin/journalctl"

run_candidate() {
    DEPLOY_PATH="$deploy_path" \
    SERVICE_NAME=jetstream-turbo \
    RUN_AS_SERVICE_USER=0 \
    SYSTEMCTL_BIN="$fake_bin/systemctl" \
    JOURNALCTL_BIN="$fake_bin/journalctl" \
    CURL_BIN="$fake_bin/curl" \
    READINESS_ATTEMPTS=2 \
    READINESS_INTERVAL_SECS=0 \
    FAKE_SYSTEMCTL_LOG="$fixture/systemctl.log" \
    FAKE_JOURNAL_LOG="$fixture/journal.log" \
    FAKE_MAINTENANCE_FAIL_FILE="$fixture/maintenance-fails" \
    FAKE_READINESS_FAIL_FILE="$fixture/readiness-fails" \
    "$script_dir/activate-candidate.sh" "$deploy_path/releases/candidate/jetstream-turbo"
}

: >"$fixture/systemctl.log"
run_candidate
[[ $(readlink "$deploy_path/current") == "$deploy_path/releases/candidate" ]]
grep -q '^restart jetstream-turbo$' "$fixture/systemctl.log"

ln -sfn "$deploy_path/releases/old" "$deploy_path/current"
: >"$fixture/systemctl.log"
touch "$fixture/maintenance-fails"
if run_candidate; then
    echo "maintenance failure unexpectedly succeeded" >&2
    exit 1
fi
[[ $(readlink "$deploy_path/current") == "$deploy_path/releases/old" ]]
[[ ! -s "$fixture/systemctl.log" ]]
rm "$fixture/maintenance-fails"

: >"$fixture/systemctl.log"
touch "$fixture/readiness-fails"
if run_candidate; then
    echo "readiness timeout unexpectedly succeeded" >&2
    exit 1
fi
[[ $(readlink "$deploy_path/current") == "$deploy_path/releases/old" ]]
[[ $(grep -c '^restart jetstream-turbo$' "$fixture/systemctl.log") -eq 2 ]]
grep -q '^status jetstream-turbo --no-pager$' "$fixture/systemctl.log"
grep -q '^-u jetstream-turbo --no-pager -n 100$' "$fixture/journal.log"

echo "candidate deployment simulation passed"
