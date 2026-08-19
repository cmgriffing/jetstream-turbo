#!/bin/bash
set -euo pipefail

# Pre-check: compile the benches (fast fail on syntax/type errors).
# Silent success; errors surface via exit code and tail.
cargo build --release --features testing --benches 2>&1 | tail -3

# Locate the cpu_throughput bench binary (exclude .d dependency files).
BIN=$(ls -t target/release/deps/cpu_throughput-* 2>/dev/null | grep -v '\.d$' | head -1)
if [ -z "$BIN" ]; then
  echo "error: cpu_throughput binary not found" >&2
  exit 1
fi

# Run the bench a few times; the bench itself reports the median of 3 timed batches.
# We take the median across invocations to absorb cross-run noise.
RUNS=3
values=""
for i in $(seq 1 "$RUNS"); do
  out=$($BIN)
  val=$(echo "$out" | grep -E '^msgs/sec:' | awk '{print $2}')
  if [ -z "$val" ]; then
    echo "error: no msgs/sec line in bench output" >&2
    echo "$out" >&2
    exit 1
  fi
  values="$values $val"
done

# Median of the collected values.
med=$(echo "$values" | tr ' ' '\n' | grep -v '^$' | sort -n | awk '{a[NR]=$1} END {print a[int((NR+1)/2)]}')

echo "METRIC msgs_per_sec=$med"
echo "runs:$(echo $values | tr ' ' ',')"
