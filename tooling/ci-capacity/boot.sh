#!/bin/sh
set -eu
path=$(sed -n 's/^0:://p' /proc/self/cgroup)
budget=${path%/*}
case "$budget" in /cicapacity*.slice|/cicapacity*) ;; *) echo "Unexpected benchmark cgroup: $budget" >&2; exit 1;; esac
printf '800000 100000\n' > "/sys/fs/cgroup$budget/cpu.max"
printf '25769803776\n' > "/sys/fs/cgroup$budget/memory.max"
printf '0\n' > "/sys/fs/cgroup$budget/memory.swap.max"
printf '%s\n' "$budget" > /benchmark-cgroup
exec dockerd --host=unix:///var/run/docker.sock --exec-opt native.cgroupdriver=cgroupfs --cgroup-parent="$budget" --storage-driver=overlay2
