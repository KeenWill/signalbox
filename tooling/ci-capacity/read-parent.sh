#!/bin/sh
set -eu
parent=$(cat /benchmark-cgroup)
root="/sys/fs/cgroup$parent"
printf '{"kind":"scope","path":"%s","cpu_max":"%s","memory_max":"%s"}\n' "$parent" "$(cat "$root/cpu.max")" "$(cat "$root/memory.max")"
while :; do
  awk -v at="$(date +%s)" '
  FILENAME ~ /cpu.stat$/ && $1 == "usage_usec" { cpu=$2 }
  FILENAME ~ /memory.current$/ { charged=$1 }
  FILENAME ~ /memory.stat$/ && $1 == "inactive_file" { inactive=$2 }
  FILENAME ~ /memory.peak$/ { peak=$1 }
  END { printf "{\"at\":%.0f,\"cpu_seconds_total\":%.6f,\"charged_memory_bytes\":%.0f,\"working_memory_bytes\":%.0f,\"kernel_peak_memory_bytes\":%.0f}\n", at,cpu/1000000,charged,charged-inactive,peak }
  ' "$root/cpu.stat" "$root/memory.current" "$root/memory.stat" "$root/memory.peak"
  [ ! -e /tmp/capacity-profile-stop ] || break
  sleep 0.5
done
