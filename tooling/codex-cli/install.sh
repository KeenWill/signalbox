#!/usr/bin/env bash
set -euo pipefail

pin_dir=$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)
manifest="$pin_dir/release.json"
destination=${1:-"$pin_dir/bin"}
release=$(jq -er '.release' "$manifest")
asset=$(jq -er '.asset' "$manifest")
sha256=$(jq -er '.sha256' "$manifest")
executable_release=$(jq -er '.executableRelease' "$manifest")
if [[ "$release" != "$executable_release" ]]; then
  printf 'archive and executable releases differ in %s\n' "$manifest" >&2
  exit 1
fi
staging=$(mktemp -d)
trap 'rm -rf "$staging"' EXIT
curl --fail --location --retry 3 \
  "https://github.com/KeenWill/codex/releases/download/$release/$asset" \
  --output "$staging/$asset"
printf '%s  %s\n' "$sha256" "$staging/$asset" | sha256sum --check -
mkdir -p "$destination"
tar -xzf "$staging/$asset" -C "$destination"
executable_sha256=$(jq -er '.executableSha256' "$manifest")
printf '%s  %s\n' "$executable_sha256" "$destination/codex" | sha256sum --check -
"$destination/codex" --version
