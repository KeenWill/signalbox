#!/usr/bin/env bash
set -euo pipefail

pin_dir=$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)
manifest="$pin_dir/release.json"
destination=${1:-"$pin_dir/bin"}
release=$(jq -er '.release' "$manifest")
asset=$(jq -er '.asset' "$manifest")
sha256=$(jq -er '.sha256' "$manifest")
staging=$(mktemp -d)
trap 'rm -rf "$staging"' EXIT
curl --fail --location --retry 3 \
  "https://github.com/KeenWill/codex/releases/download/$release/$asset" \
  --output "$staging/$asset"
printf '%s  %s\n' "$sha256" "$staging/$asset" | sha256sum --check -
mkdir -p "$destination"
tar -xzf "$staging/$asset" -C "$destination"
"$destination/codex" --version
