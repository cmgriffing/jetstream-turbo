#!/usr/bin/env bash
set -euo pipefail
script_dir=$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)
fixture=$(mktemp -d)
trap 'rm -rf "$fixture"' EXIT
mkdir -p "$fixture/bin" "$fixture/systemd" "$fixture/journal" "$fixture/deploy/logs" "$fixture/archive"
printf '0123456789abcdef0123456789abcdef\n' >"$fixture/machine-id"
cat >"$fixture/bin/systemctl" <<'FAKE'
#!/usr/bin/env bash
printf '%s\n' "$*" >>"${FIXTURE}/systemctl.log"
case "$1" in
    --version) echo "systemd ${FAKE_SYSTEMD_VERSION:-252}" ;;
    cat) [[ "${FAKE_TEMPLATE_MISSING:-0}" == 0 ]] ;;
    show)
        case "$3" in
            --property=LogNamespace) echo "${FAKE_NAMESPACE:-jetstream-turbo}" ;;
            --property=ActiveState) echo "${FAKE_SERVICE_STATE:-inactive}" ;;
            --property=MainPID) echo "${FAKE_SERVICE_PID:-0}" ;;
        esac
        ;;
esac
FAKE
cat >"$fixture/bin/journalctl" <<'FAKE'
#!/usr/bin/env bash
printf '%s\n' "$*" >>"${FIXTURE}/journalctl.log"
if [[ "$1" == --help ]]; then echo '--namespace=NAMESPACE'; else echo 'Isolated journals use 8M'; fi
FAKE
cat >"$fixture/bin/fuser" <<'FAKE'
#!/usr/bin/env bash
exit "${FAKE_FUSER_RESULT:-1}"
FAKE
cat >"$fixture/bin/du" <<'FAKE'
#!/usr/bin/env bash
printf '%s\t%s\n' "${FAKE_JOURNAL_KIB:-8}" "$2"
FAKE
chmod +x "$fixture/bin/"*
export FIXTURE="$fixture"
export SYSTEMCTL_BIN="$fixture/bin/systemctl" JOURNALCTL_BIN="$fixture/bin/journalctl"
export DU_BIN="$fixture/bin/du" SYSTEMD_CONFIG_ROOT="$fixture/systemd"
export JOURNAL_ROOTS="$fixture/journal" MACHINE_ID_FILE="$fixture/machine-id"
export DEPLOY_PATH="$fixture/deploy" FUSER_BIN="$fixture/bin/fuser"

if FAKE_SYSTEMD_VERSION=244 "$script_dir/configure-journal.sh" install; then
    echo 'unsupported namespace unexpectedly installed' >&2; exit 1
fi
[[ ! -f "$fixture/systemd/journald@jetstream-turbo.conf" ]]
if FAKE_TEMPLATE_MISSING=1 "$script_dir/configure-journal.sh" install; then
    echo 'missing template unexpectedly installed' >&2; exit 1
fi
[[ ! -f "$fixture/systemd/journald@jetstream-turbo.conf" ]]
"$script_dir/configure-journal.sh" install
cmp "$script_dir/systemd/journald@jetstream-turbo.conf" "$fixture/systemd/journald@jetstream-turbo.conf"
grep -q '^restart systemd-journald@jetstream-turbo.service$' "$fixture/systemctl.log"
"$script_dir/configure-journal.sh" check
if FAKE_NAMESPACE=other "$script_dir/configure-journal.sh" check; then
    echo 'non-isolated service unexpectedly passed' >&2; exit 1
fi
namespace_dir="$fixture/journal/0123456789abcdef0123456789abcdef.jetstream-turbo"
mkdir -p "$namespace_dir"
printf 'active' >"$namespace_dir/system.journal"
if FAKE_JOURNAL_KIB=131073 "$script_dir/configure-journal.sh" check; then
    echo 'active-file headroom excess unexpectedly passed' >&2; exit 1
fi
if FAKE_JOURNAL_KIB=524289 "$script_dir/configure-journal.sh" check; then
    echo 'allocation excess unexpectedly passed' >&2; exit 1
fi
! grep -Eq '^(restart systemd-journald.service|--vacuum|--disk-usage$)' "$fixture/systemctl.log" "$fixture/journalctl.log"

"$script_dir/prepare-legacy-logs.sh" check
printf 'legacy log' >"$fixture/deploy/logs/jetstream-turbo.log"
printf 'unrelated log' >"$fixture/deploy/logs/other.log"
if "$script_dir/prepare-legacy-logs.sh" check; then echo 'legacy file accepted' >&2; exit 1; fi
if FAKE_SERVICE_STATE=active FAKE_SERVICE_PID=123 "$script_dir/prepare-legacy-logs.sh" archive "$fixture/archive"; then
    echo 'running-writer migration accepted' >&2; exit 1
fi
if FAKE_FUSER_RESULT=0 "$script_dir/prepare-legacy-logs.sh" archive "$fixture/archive"; then
    echo 'open-writer migration accepted' >&2; exit 1
fi
"$script_dir/prepare-legacy-logs.sh" archive "$fixture/archive"
[[ -f "$fixture/deploy/logs/other.log" ]]
[[ ! -f "$fixture/deploy/logs/jetstream-turbo.log" ]]
[[ $(find "$fixture/archive" -name jetstream-turbo.log | wc -l | tr -d ' ') == 1 ]]
"$script_dir/prepare-legacy-logs.sh" check

echo 'isolated journal and stopped-writer legacy log fixtures passed'
