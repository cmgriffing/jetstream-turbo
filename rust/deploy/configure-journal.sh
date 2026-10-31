#!/usr/bin/env bash
# Never modifies or vacuums the host's default journal namespace.
set -euo pipefail
mode=${1:-preflight}
namespace=jetstream-turbo
systemctl_bin=${SYSTEMCTL_BIN:-systemctl}
journalctl_bin=${JOURNALCTL_BIN:-journalctl}
du_bin=${DU_BIN:-du}
systemd_root=${SYSTEMD_CONFIG_ROOT:-/etc/systemd}
journal_roots=${JOURNAL_ROOTS:-/var/log/journal:/run/log/journal}
machine_id_file=${MACHINE_ID_FILE:-/etc/machine-id}
script_dir=$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)

preflight() {
    local version help
    version=$("$systemctl_bin" --version | head -1 | awk '{print $2}')
    [[ "$version" =~ ^[0-9]+$ ]] && (( version >= 245 )) || {
        echo 'Journal namespace requires systemd >= 245; shared host policy was not changed' >&2
        return 1
    }
    help=$("$journalctl_bin" --help)
    [[ "$help" == *--namespace* ]] || {
        echo 'journalctl lacks namespace support; shared host policy was not changed' >&2
        return 1
    }
    "$systemctl_bin" cat systemd-journald@.service >/dev/null || {
        echo 'Missing namespace daemon template; shared host policy was not changed' >&2
        return 1
    }
}

usage_check() {
    local machine_id root directory active_kib=0 total_kib=0 size file
    machine_id=$(cat "$machine_id_file")
    [[ "$machine_id" =~ ^[0-9a-fA-F]{32}$ ]] || { echo 'Invalid machine-id' >&2; return 1; }
    IFS=: read -r -a roots <<<"$journal_roots"
    shopt -s nullglob
    for root in "${roots[@]}"; do
        directory="$root/$machine_id.$namespace"
        [[ -d "$directory" && ! -L "$directory" ]] || continue
        size=$("$du_bin" -sk "$directory" | awk '{print $1}')
        total_kib=$((total_kib + size))
        for file in "$directory"/*.journal; do
            [[ "$(basename "$file")" != *@* && -f "$file" && ! -L "$file" ]] || continue
            size=$("$du_bin" -k "$file" | awk '{print $1}')
            active_kib=$((active_kib + size))
        done
    done
    "$journalctl_bin" --namespace="$namespace" --disk-usage
    printf 'journal_namespace=%s allocated_bytes=%s active_bytes=%s allocation_bytes=536870912 active_headroom_bytes=134217728\n' \
        "$namespace" "$((total_kib * 1024))" "$((active_kib * 1024))"
    (( active_kib <= 131072 && total_kib <= 524288 )) || {
        echo 'Namespace exceeds measured active headroom/allocation; investigate isolated journal, do not vacuum the shared journal' >&2
        return 1
    }
}

case "$mode" in
    preflight) preflight ;;
    install)
        preflight
        config=${2:-"$script_dir/systemd/journald@$namespace.conf"}
        [[ -f "$config" ]] || { echo "Missing namespace config: $config" >&2; exit 1; }
        # Namespace-specific file only. Unsupported isolation exits before this point.
        install -m 0644 "$config" "$systemd_root/journald@$namespace.conf"
        "$systemctl_bin" restart "systemd-journald@$namespace.service"
        usage_check
        ;;
    check)
        preflight
        effective=$("$systemctl_bin" show "$namespace.service" --property=LogNamespace --value)
        [[ "$effective" == "$namespace" ]] || { echo 'Service journal isolation is not effective' >&2; exit 1; }
        usage_check
        ;;
    *) echo 'usage: configure-journal.sh preflight|install [config]|check' >&2; exit 2 ;;
esac
