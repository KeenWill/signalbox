#!/usr/bin/env python3
"""Prove check_migration_versions.py actually fails on a collision.

A checker that cannot be shown to fail is the false-confidence trap this
repository documents, so each rule gets a positive and a negative case: a
directory with unique versions passes, a duplicated version fails naming both
files, a zero-padded restatement of an existing version fails because sqlx
parses both prefixes to the same integer, and an unparseable filename fails
rather than escaping the duplicate rule. Each case runs the checker as a
subprocess against a synthetic `crates/*/migrations` tree in a temporary
working directory.
"""

from __future__ import annotations

import subprocess
import sys
import tempfile
import unittest
from pathlib import Path

CHECKER = Path(__file__).resolve().parent / "check_migration_versions.py"


def check_migrations(*names: str) -> subprocess.CompletedProcess:
    """Run the checker over a synthetic migrations tree, outside test bodies.

    Builds `crates/persistence/migrations` holding exactly the named files in a
    temporary working directory, then runs the checker with that directory as
    its working directory so its root-relative discovery sees only the fixture.
    """
    with tempfile.TemporaryDirectory() as tmp:
        root = Path(tmp)
        directory = root / "crates" / "persistence" / "migrations"
        directory.mkdir(parents=True)
        for name in names:
            (directory / name).write_text("-- synthetic\n")
        return subprocess.run(
            [sys.executable, str(CHECKER)],
            cwd=root,
            capture_output=True,
            text=True,
        )


class MigrationVersionCheckerTests(unittest.TestCase):
    def test_unique_versions_pass(self) -> None:
        result = check_migrations("1_a.sql", "2_b.sql")

        self.assertEqual(result.returncode, 0, result.stdout)

    def test_duplicate_version_fails_naming_both_files(self) -> None:
        first = "3_a.sql"
        second = "3_b.sql"

        result = check_migrations(first, second, "4_c.sql")

        self.assertEqual(result.returncode, 1, result.stdout)
        self.assertIn(first, result.stdout)
        self.assertIn(second, result.stdout)

    def test_zero_padded_restatement_of_a_version_fails(self) -> None:
        unpadded = "1_a.sql"
        padded = "01_b.sql"

        result = check_migrations(unpadded, padded)

        self.assertEqual(result.returncode, 1, result.stdout)
        self.assertIn(unpadded, result.stdout)
        self.assertIn(padded, result.stdout)

    def test_unparseable_migration_name_fails(self) -> None:
        unparseable = "nodigits.sql"

        result = check_migrations(unparseable)

        self.assertEqual(result.returncode, 1, result.stdout)
        self.assertIn(unparseable, result.stdout)


if __name__ == "__main__":
    unittest.main()
