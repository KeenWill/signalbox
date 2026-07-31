#!/usr/bin/env python3
"""Check that no two SQL migrations share a version number.

Ground truth is every `migrations/` directory under `crates/`. A migration's
version is the digit prefix of its filename before the first underscore
(sqlx's convention); two files with the same version in the same directory
make every migration run fail at apply time with a duplicate
`_sqlx_migrations` key — but only once BOTH are present, which is exactly
the state a merge of two independently green pull requests produces. Per-PR
CI cannot catch this (each branch carries only its own file) and git does
not conflict (the filenames differ), so the collision lands on the default
branch silently. This happened on 2026-07-31: two pull requests each added
a migration versioned 202608020003, each was green alone, and every
Postgres job on main failed from the moment both merged until one was
renumbered.

The check fails when

1. two or more migration files in one directory share a version prefix, or
2. a file in a migrations directory does not begin with a digit prefix
   followed by an underscore — an unparseable name would silently escape
   rule 1, so it fails loudly instead.

Run from the repository root; exits nonzero with a per-collision report
naming every file involved.
"""

import re
import sys
from collections import defaultdict
from pathlib import Path

MIGRATION_NAME = re.compile(r"^(\d+)_.+\.sql$")


def main() -> int:
    failures = []
    for directory in sorted(Path("crates").glob("*/migrations")):
        by_version = defaultdict(list)
        for entry in sorted(directory.iterdir()):
            if not entry.is_file():
                continue
            match = MIGRATION_NAME.match(entry.name)
            if match is None:
                failures.append(
                    f"unparseable migration name (need <digits>_<name>.sql): {entry}"
                )
                continue
            by_version[match.group(1)].append(entry.name)
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
