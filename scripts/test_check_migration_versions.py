#!/usr/bin/env python3
"""Prove check_migration_versions.py actually fails on a collision.

A checker that cannot be shown to fail is the false-confidence trap this
repository documents, so each rule gets a positive and a negative case: a
directory with unique versions passes, a duplicated version fails naming
both files, and an unparseable filename fails rather than escaping the
duplicate rule. Runs the checker as a subprocess against a synthetic
`crates/*/migrations` tree in a temporary working directory.
"""

import subprocess
import sys
import tempfile
from pathlib import Path

CHECKER = Path(__file__).resolve().parent / "check_migration_versions.py"


def run_checker(root: Path) -> subprocess.CompletedProcess:
    return subprocess.run(
        [sys.executable, str(CHECKER)],
        cwd=root,
        capture_output=True,
        text=True,
    )


def build_tree(root: Path, names: list[str]) -> None:
    directory = root / "crates" / "persistence" / "migrations"
    directory.mkdir(parents=True)
    for name in names:
        (directory / name).write_text("-- synthetic\n")


def main() -> int:
    failures = []

    with tempfile.TemporaryDirectory() as tmp:
        build_tree(Path(tmp), ["1_a.sql", "2_b.sql"])
        result = run_checker(Path(tmp))
        if result.returncode != 0:
            failures.append(f"unique versions should pass: {result.stdout}")

    with tempfile.TemporaryDirectory() as tmp:
        build_tree(Path(tmp), ["3_a.sql", "3_b.sql", "4_c.sql"])
        result = run_checker(Path(tmp))
        if result.returncode == 0:
            failures.append("duplicate version 3 should fail and did not")
        elif "3_a.sql" not in result.stdout or "3_b.sql" not in result.stdout:
            failures.append(
                f"duplicate report must name both files: {result.stdout}"
            )

    with tempfile.TemporaryDirectory() as tmp:
        build_tree(Path(tmp), ["nodigits.sql"])
        result = run_checker(Path(tmp))
        if result.returncode == 0:
            failures.append("unparseable migration name should fail and did not")

    if failures:
        print("migration-version checker self-test FAILED:")
        for failure in failures:
            print(f"  - {failure}")
        return 1
    print("migration-version checker self-test passed")
    return 0


if __name__ == "__main__":
    sys.exit(main())
