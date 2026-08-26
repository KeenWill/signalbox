#!/usr/bin/env bash
# Provision the direct repository that daemon-local tools derive for one session.
#
# The daemon never accepts an arbitrary workspace path from a session. Given a
# configured root `/srv/signalbox/workspace`, it looks only for the sibling path
# `/srv/signalbox/workspace.sessions/<session-uuid>`. This helper performs the
# deployment-owned half of that contract without weakening the daemon's
# descriptor and repository-identity checks.
#
# Usage: provision-session-workspace.sh --configured-root <path>
#          --session-id <uuid> --revision <sha> --remote <url>
#          [--seed-tree <path>]
set -euo pipefail

configured_root=""
session_id=""
revision=""
remote=""
seed_tree=""

usage() {
	echo "usage: provision-session-workspace.sh --configured-root <path>" \
		"--session-id <uuid> --revision <sha> --remote <url>" \
		"[--seed-tree <path>]"
}

while [ "$#" -gt 0 ]; do
	case "$1" in
	--configured-root | --session-id | --revision | --remote | --seed-tree)
		if [ "$#" -lt 2 ]; then
			echo "provision-session-workspace: $1 needs a value" >&2
			exit 2
		fi
		case "$1" in
		--configured-root) configured_root=$2 ;;
		--session-id) session_id=$2 ;;
		--revision) revision=$2 ;;
		--remote) remote=$2 ;;
		--seed-tree) seed_tree=$2 ;;
		esac
		shift 2
		;;
	-h | --help)
		usage
		exit 0
		;;
	*)
		echo "provision-session-workspace: unknown argument: $1" >&2
		usage >&2
		exit 2
		;;
	esac
done

if [ -z "$configured_root" ] || [ -z "$session_id" ] ||
	[ -z "$revision" ] || [ -z "$remote" ]; then
	echo "provision-session-workspace: all required arguments must be supplied" >&2
	usage >&2
	exit 2
fi

if [[ ! "$session_id" =~ ^[0-9a-f]{8}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{12}$ ]]; then
	echo "provision-session-workspace: session-id must be a canonical lowercase UUID" >&2
	exit 2
fi

if [[ ! "$revision" =~ ^[0-9a-f]{40}$ ]]; then
	echo "provision-session-workspace: revision must be a lowercase 40-digit commit ID" >&2
	exit 2
fi

case "$configured_root" in
/*) ;;
*)
	echo "provision-session-workspace: configured-root must be absolute" >&2
	exit 2
	;;
esac

if [ ! -d "$configured_root" ] || [ -L "$configured_root" ] ||
	[ ! -d "$configured_root/.git" ] || [ -L "$configured_root/.git" ]; then
	echo "provision-session-workspace: configured-root must be a direct repository" >&2
	exit 1
fi

canonical_root=$(realpath -- "$configured_root")
repository_root=$(git -C "$configured_root" rev-parse --show-toplevel)
if [ "$repository_root" != "$canonical_root" ]; then
	echo "provision-session-workspace: configured-root is not the repository root" >&2
	exit 1
fi
if ! git -C "$configured_root" cat-file -e "$revision^{commit}"; then
	echo "provision-session-workspace: revision is absent from configured-root" >&2
	exit 1
fi
reachable_ref=$(
	git -C "$configured_root" for-each-ref \
		--count=1 --format='%(refname)' --contains="$revision"
)
if [ -z "$reachable_ref" ]; then
	echo "provision-session-workspace: revision is not retained by a repository ref" >&2
	exit 1
fi

if [ -n "$seed_tree" ]; then
	case "$seed_tree" in
	/*) ;;
	*)
		echo "provision-session-workspace: seed-tree must be absolute" >&2
		exit 2
		;;
	esac
	if [ ! -d "$seed_tree" ] || [ -L "$seed_tree" ]; then
		echo "provision-session-workspace: seed-tree must be a directory" >&2
		exit 1
	fi
	if ! command -v rsync >/dev/null 2>&1; then
		echo "provision-session-workspace: rsync is required with --seed-tree" >&2
		exit 1
	fi
fi

configured_parent=$(dirname -- "$canonical_root")
configured_name=$(basename -- "$canonical_root")
derived_parent="$configured_parent/$configured_name.sessions"
target="$derived_parent/$session_id"

if [ -e "$derived_parent" ] || [ -L "$derived_parent" ]; then
	if [ ! -d "$derived_parent" ] || [ -L "$derived_parent" ]; then
		echo "provision-session-workspace: derived parent is not a direct directory" >&2
		exit 1
	fi
else
	mkdir -- "$derived_parent"
fi

if [ -e "$target" ] || [ -L "$target" ]; then
	if [ -d "$target" ] && [ ! -L "$target" ] && [ -d "$target/.git" ] &&
		[ ! -L "$target/.git" ] &&
		[ "$(git -C "$target" rev-parse --show-toplevel 2>/dev/null)" = "$target" ] &&
		[ "$(git -C "$target" remote get-url origin 2>/dev/null)" = "$remote" ]; then
		printf '%s\n' "$target"
	exit 0
	fi
	echo "provision-session-workspace: target exists but does not match its deployment fence" >&2
	exit 1
fi

staging=$(mktemp -d "$derived_parent/.${session_id}.provisioning.XXXXXXXX")
cleanup() {
	if [ -n "$staging" ] && [ -d "$staging" ]; then
		rm -rf -- "$staging"
	fi
}
trap cleanup EXIT HUP INT QUIT TERM

# `--no-hardlinks` gives the session its own object store as well as its own
# worktree and administration directory. `--no-checkout` keeps the publication
# private until the exact commissioned revision has been selected.
rmdir -- "$staging"
git clone --local --no-hardlinks --no-checkout -- "$canonical_root" "$staging"
git -C "$staging" checkout --detach "$revision"
git -C "$staging" remote set-url origin "$remote"

if [ -n "$seed_tree" ]; then
	# Preserve work already present in a legacy session tree. Build products and
	# dependency caches are deliberately regenerated under the new boundary.
	rsync -a --delete \
		--exclude=/.git \
		--exclude=/.cache \
		--exclude=/.cargo/registry \
		--exclude=/node_modules \
		--exclude=/target \
		-- "$seed_tree/" "$staging/"
fi

if [ ! -d "$staging/.git" ] || [ -L "$staging/.git" ]; then
	echo "provision-session-workspace: clone did not produce a direct repository" >&2
	exit 1
fi

mv -T -- "$staging" "$target"
staging=""
trap - EXIT HUP INT QUIT TERM
printf '%s\n' "$target"
