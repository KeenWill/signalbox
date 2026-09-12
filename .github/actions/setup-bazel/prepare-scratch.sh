#!/usr/bin/env bash
set -euo pipefail

# RUNNER_TEMP is on the runner workspace volume and is reclaimed by the runner
# between jobs. A unique root also isolates repeated setup calls in one job.
bazel_scratch=$(mktemp -d "${RUNNER_TEMP:?}/signalbox-bazel.XXXXXXXX")
printf 'root=%s\n' "$bazel_scratch" >>"${GITHUB_OUTPUT:?}"
printf 'BAZELISK_HOME=%s/bazelisk\n' "$bazel_scratch" >>"${GITHUB_ENV:?}"
