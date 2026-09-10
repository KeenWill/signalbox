#!/bin/sh
set -eu
path=$(sed -n 's/^0:://p' /proc/self/cgroup)
parent=${path%/*}
case "$parent" in /kubepods/*/pod*) ;; *) echo "Unexpected pod cgroup: $parent" >&2; exit 1;; esac
printf '%s\n' "$parent" > /benchmark-cgroup
ip link add cicapacity0 type bridge
ip addr add 172.30.0.1/24 dev cicapacity0
ip link set cicapacity0 up
dockerd --bridge=cicapacity0 --fixed-cidr=172.30.0.0/24 --host=unix:///shared-run/cicapacity.sock --exec-opt native.cgroupdriver=cgroupfs --cgroup-parent="$parent" --storage-driver=overlay2 &
daemon_pid=$!
attempt=0
while [ ! -S /shared-run/cicapacity.sock ]; do
  kill -0 "$daemon_pid"
  attempt=$((attempt+1))
  [ "$attempt" -le 120 ]
  sleep 0.5
done
chown 0:1001 /shared-run/cicapacity.sock
chmod 660 /shared-run/cicapacity.sock
wait "$daemon_pid"
