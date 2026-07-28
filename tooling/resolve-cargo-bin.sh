#!/usr/bin/env bash
# Resolve the executable Cargo actually built for one workspace binary,
# instead of assuming it landed under <target-dir>/debug/<bin>. Cargo honors
# `build.target` from a caller's `$CARGO_HOME/config.toml` (or
# `CARGO_BUILD_TARGET` in the environment) regardless of the invoking working
# directory, so a configured build target places the binary under
# <target-dir>/<triple>/debug/<bin> instead. Assuming the plain layout either
# fails outright or, worse, silently execs a stale binary left over from an
# earlier build at the assumed path. This resolves the real path from
# Cargo's own build-message JSON stream and refuses a resolved path this
# host cannot execute — an intentionally cross-compiled target — with an
# actionable message naming the host triple, mirroring the dev-instance
# daemon launcher's equivalent resolution in devenv.nix.
#
# Usage: resolve-cargo-bin.sh <manifest-path> <target-dir> <package> <bin>
set -euo pipefail

if [ "$#" -ne 4 ]; then
	echo "usage: resolve-cargo-bin.sh <manifest-path> <target-dir> <package> <bin>" >&2
	exit 2
fi

manifest_path="$1"
target_dir="$2"
package="$3"
bin_name="$4"

# Diagnostics still render to stderr; only the JSON artifact stream is
# captured, matching the daemon launcher's own capture discipline.
executable="$(
	cargo build --manifest-path "$manifest_path" --target-dir "$target_dir" \
		-p "$package" --bin "$bin_name" \
		--message-format=json-render-diagnostics |
		python3 -c "import json, sys; print([m['executable'] for m in map(json.loads, sys.stdin) if m.get('executable')][-1])"
)"

# Resolving the artifact says where Cargo put it, not whether this host can
# run it: a configured foreign target with a working cross toolchain builds
# successfully and only a later exec fails. Compare against the two layouts
# a host build produces — plain, and an explicitly named host target — and
# refuse anything else with an actionable message.
host_target="$(rustc -vV | sed -n 's/^host: //p')"
if [ "$executable" != "$target_dir/debug/$bin_name" ] &&
	[ "$executable" != "$target_dir/$host_target/debug/$bin_name" ]; then
	echo "resolve-cargo-bin: cargo built $package for a target this host cannot" \
		"run: $executable" >&2
	echo "resolve-cargo-bin: unset build.target and CARGO_BUILD_TARGET, or set" \
		"them to $host_target" >&2
	exit 1
fi

printf '%s\n' "$executable"
