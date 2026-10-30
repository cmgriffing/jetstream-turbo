#!/usr/bin/env bash
# Explicit, stopped-writer migration for the two legacy unrotated file names.
set -euo pipefail
mode=${1:-check}
deploy_path=${DEPLOY_PATH:-/opt/jetstream-turbo}
service_name=${SERVICE_NAME:-jetstream-turbo}
systemctl_bin=${SYSTEMCTL_BIN:-systemctl}
fuser_bin=${FUSER_BIN:-fuser}
log_dir="$deploy_path/logs"
files=()
for name in jetstream-turbo.log jetstream-turbo-batches.log; do
    path="$log_dir/$name"
    if [[ -e "$path" || -L "$path" ]]; then
        [[ -f "$path" && ! -L "$path" ]] || { echo "Refusing non-regular legacy log: $path" >&2; exit 1; }
        files+=("$path")
    fi
done
if (( ${#files[@]} == 0 )); then
    echo 'No legacy unrotated application files remain'
    exit 0
fi
if [[ "$mode" == check ]]; then
    printf 'Legacy unrotated log requires stopped/drained migration: %s\n' "${files[@]}" >&2
    exit 1
fi
[[ "$mode" == archive ]] || { echo 'usage: prepare-legacy-logs.sh check|archive /existing/destination-directory' >&2; exit 2; }
destination=${2:?provide an existing archive destination, preferably off-volume}
[[ -d "$destination" && ! -L "$destination" ]] || { echo 'Archive destination must be an existing real directory' >&2; exit 1; }
state=$("$systemctl_bin" show "$service_name" --property=ActiveState --value)
pid=$("$systemctl_bin" show "$service_name" --property=MainPID --value)
[[ ( "$state" == inactive || "$state" == failed ) && "$pid" == 0 ]] || {
    echo 'Stop the service and wait for its writer to drain before migrating logs' >&2; exit 1;
}
command -v "$fuser_bin" >/dev/null || { echo 'fuser is required to verify no writer holds the legacy files' >&2; exit 1; }
for file in "${files[@]}"; do
    if "$fuser_bin" -s "$file"; then
        echo "Legacy log is still open: $file" >&2; exit 1
    else
        result=$?
        (( result == 1 )) || { echo "Cannot inspect open-file state: $file" >&2; exit 1; }
    fi
done
source_device=$(df -Pk "$log_dir" | awk 'END {print $1}')
dest_device=$(df -Pk "$destination" | awk 'END {print $1}')
available_kib=$(df -Pk "$destination" | awk 'END {print $4}')
needed_kib=0
for file in "${files[@]}"; do
    size=$(du -k "$file" | awk '{print $1}')
    apparent_bytes=$(wc -c <"$file" | tr -d ' ')
    apparent_kib=$(((apparent_bytes + 1023) / 1024))
    (( size >= apparent_kib )) || size=$apparent_kib
    needed_kib=$((needed_kib + size))
done
if [[ "$source_device" != "$dest_device" ]] && (( available_kib < needed_kib + 2097152 )); then
    echo 'Insufficient destination capacity for legacy logs plus 2 GiB emergency headroom' >&2; exit 1
fi
archive="$destination/jetstream-turbo-legacy-logs-$(date -u +%Y%m%dT%H%M%SZ)-$$"
mkdir "$archive"
echo 'Source free space before migration:'
df -Pk "$log_dir"
for file in "${files[@]}"; do mv -- "$file" "$archive/"; done
echo 'Source free space after migration:'
df -Pk "$log_dir"
du -sk "$archive"
if [[ "$source_device" == "$dest_device" ]]; then
    echo 'Archive remains on the same volume: no disk space was reclaimed; include it in capacity accounting'
else
    echo 'Verify the measured free-space increase before enabling steady-state ingestion'
fi
