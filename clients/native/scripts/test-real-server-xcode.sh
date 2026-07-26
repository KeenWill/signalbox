#!/usr/bin/env bash
set -euo pipefail

cat >&2 <<'EOF'
The phase-A native client has no real remote/mobile smoke target.
signalboxd currently exposes only a local Unix socket, and the process protocol
defines no authentication field. Use scripts/test-xcode.sh for the v5 local
harness. A real remote smoke resumes after the owner transport design gate.
EOF
exit 2
