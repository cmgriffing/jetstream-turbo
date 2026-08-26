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
cp "$script_dir/test-fixtures/fake-chown.sh" "$fake_bin/chown"
cp "$script_dir/test-fixtures/fake-journalctl.sh" "$fake_bin/journalctl"
chmod 0755 "$fake_bin/systemctl" "$fake_bin/curl" "$fake_bin/chown" "$fake_bin/journalctl"

run_candidate() {
    local candidate_binary=${1:-"$deploy_path/releases/candidate/jetstream-turbo"}
    DEPLOY_PATH="$deploy_path" \
    SERVICE_NAME=jetstream-turbo \
    RUN_AS_SERVICE_USER=0 \
    SYSTEMCTL_BIN="$fake_bin/systemctl" \
    JOURNALCTL_BIN="$fake_bin/journalctl" \
    CURL_BIN="$fake_bin/curl" \
    CHOWN_BIN="$fake_bin/chown" \
    READINESS_ATTEMPTS=2 \
    READINESS_INTERVAL_SECS=0 \
    FAKE_SYSTEMCTL_LOG="$fixture/systemctl.log" \
    FAKE_JOURNAL_LOG="$fixture/journal.log" \
    FAKE_CHOWN_LOG="$fixture/chown.log" \
    FAKE_MAINTENANCE_FAIL_FILE="$fixture/maintenance-fails" \
    FAKE_READINESS_FAIL_FILE="$fixture/readiness-fails" \
    "$script_dir/activate-candidate.sh" "$candidate_binary"
}

: >"$fixture/systemctl.log"
run_candidate
[[ $(readlink "$deploy_path/current") == "$deploy_path/releases/candidate" ]]
grep -q '^restart jetstream-turbo$' "$fixture/systemctl.log"

# Activation must publish a bounded release identifier for startup diagnostics.
grep -q '^JETSTREAM_TURBO_RELEASE_ID=candidate$' "$deploy_path/.env"

# Re-activating from a differently named release updates the identifier in place.
mkdir -p "$deploy_path/releases/candidate-2"
write_release_binary "$deploy_path/releases/candidate-2/jetstream-turbo"
: >"$fixture/systemctl.log"
run_candidate "$deploy_path/releases/candidate-2/jetstream-turbo"
[[ $(readlink "$deploy_path/current") == "$deploy_path/releases/candidate-2" ]]
grep -q '^JETSTREAM_TURBO_RELEASE_ID=candidate-2$' "$deploy_path/.env"
[[ $(grep -c '^JETSTREAM_TURBO_RELEASE_ID=' "$deploy_path/.env") -eq 1 ]]

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
grep -Eq '^-u jetstream-turbo --since [0-9]{4}-[0-9]{2}-[0-9]{2}T[0-9]{2}:[0-9]{2}:[0-9]{2}Z --no-pager -n 100$' "$fixture/journal.log"

rm "$fixture/readiness-fails"
rm "$deploy_path/current"
cp "$script_dir/test-fixtures/fake-release.sh" "$deploy_path/jetstream-turbo"
chmod 0755 "$deploy_path/jetstream-turbo"
: >"$fixture/chown.log"
run_candidate
grep -Eq '^-R jetstream-turbo:jetstream-turbo .*/releases/legacy-[0-9]{14}$' "$fixture/chown.log"

echo "candidate deployment simulation passed"
