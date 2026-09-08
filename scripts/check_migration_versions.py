#!/usr/bin/env python3
"""Check that no two SQL migrations share a version number.

Ground truth is every `migrations/` directory under `crates/`. A migration's
version is the digit prefix of its filename before the first underscore,
parsed as an integer the way sqlx parses it; two files with the same version
in the same directory make every migration run fail at apply time with a
duplicate `_sqlx_migrations` key — but only once BOTH are present, which is
exactly the state a merge of two independently green pull requests produces.
Git does not conflict, because the filenames differ. Pull-request CI checks
out the merge of the branch into its base branch and so does see both files, but only
as of the run: a branch whose last run predates the other branch's merge
stays green, and the collision lands on the default branch.

The check fails when

1. two or more migration files in one directory share a version, compared as
   integers so that `1_a.sql` and `01_b.sql` collide exactly as they do in
   `_sqlx_migrations`, or
2. a file in a migrations directory does not begin with a digit prefix
   followed by an underscore — an unparseable name would silently escape
   rule 1, so it fails loudly instead, or
3. a branch-added migration does not sort after the highest version in its
   directory on `origin/main` or `--base` (defaulting to the pull request
   base branch from `GITHUB_BASE_REF`, otherwise `origin/main`).

Run from the repository root; exits nonzero with a per-collision report
naming every file involved.
"""

import argparse
import os
import re
import subprocess
import sys
from collections import defaultdict
from pathlib import Path

MIGRATION_NAME = re.compile(r"^(\d+)_.+\.sql$")


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument(
        "--base", default=f"origin/{os.environ.get('GITHUB_BASE_REF') or 'main'}"
    )
    args = parser.parse_args()
    baseline_refs = list(dict.fromkeys([args.base, "origin/main"]))
    base_files = set()
    for reference in baseline_refs:
        base = subprocess.run(
            ["git", "ls-tree", "-r", "--name-only", reference, "--", "crates"],
            capture_output=True,
            text=True,
        )
        if base.returncode:
            print(f"cannot read migration base {reference}: {base.stderr.strip()}")
            return 1
        base_files.update(base.stdout.splitlines())
    failures = []
    for directory in sorted(Path("crates").glob("*/migrations")):
        by_version = defaultdict(list)
        base_versions = [
            int(match.group(1))
            for name in base_files
            if Path(name).parent == directory
            and (match := MIGRATION_NAME.match(Path(name).name))
        ]
        for entry in sorted(directory.iterdir()):
            if not entry.is_file():
                continue
            match = MIGRATION_NAME.match(entry.name)
            if match is None:
                failures.append(
                    f"unparseable migration name (need <digits>_<name>.sql): {entry}"
                )
                continue
            version = int(match.group(1))
            by_version[version].append(entry.name)
            if (
                str(entry) not in base_files
                and base_versions
                and version <= max(base_versions)
            ):
                failures.append(
                    f"new migration {entry} must sort after {', '.join(baseline_refs)} version {max(base_versions)}"
                )
        for version, names in sorted(by_version.items()):
            if len(names) > 1:
                listing = ", ".join(names)
                failures.append(
                    f"version {version} used by {len(names)} migrations in {directory}: {listing}"
                )
    if failures:
        print("migration-version check FAILED:")
        for failure in failures:
            print(f"  - {failure}")
        return 1
    print("migration-version check passed")
    return 0


if __name__ == "__main__":
    sys.exit(main())
