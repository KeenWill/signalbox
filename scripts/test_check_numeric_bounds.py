#!/usr/bin/env python3
"""Prove the numeric-bound inventory gates declarations and derived escapes."""

from __future__ import annotations

import subprocess
import sys
import tempfile
import unittest
from pathlib import Path

CHECKER = Path(__file__).resolve().parent / "check_numeric_bounds.py"
ENFORCED_FILE = Path("crates/application/src/lib.rs")
OUTSIDE_FILE = Path("crates/domain/src/lib.rs")


def run_checker(path: Path, text: str) -> subprocess.CompletedProcess[str]:
    with tempfile.TemporaryDirectory(prefix="signalbox-numeric-bounds-") as directory:
        root = Path(directory)
        fixture = root / path
        fixture.parent.mkdir(parents=True)
        fixture.write_text(text, encoding="utf-8")
        return subprocess.run(
            [sys.executable, str(CHECKER), "--root", str(root)],
            check=False,
            capture_output=True,
            text=True,
        )


class NumericBoundCheckerTests(unittest.TestCase):
    def test_direct_ceiling_and_tunable_declarations_pass(self) -> None:
        result = run_checker(
            ENFORCED_FILE,
            "// numeric-bound: ceiling - protects against retained input growth\n"
            "const MAX_INPUT_BYTES: usize = 1024;\n"
            "// numeric-bound: tunable - controls the ordinary wait\n"
            "const DEFAULT_TIMEOUT: Duration = Duration::from_secs(1);\n",
        )

        self.assertEqual(result.returncode, 0, result.stdout)
        self.assertIn("2 enforced", result.stdout)

    def test_missing_declaration_fails_with_location_and_name(self) -> None:
        result = run_checker(ENFORCED_FILE, "const MAX_INPUT_BYTES: usize = 1024;\n")

        self.assertEqual(result.returncode, 1, result.stdout)
        self.assertIn("crates/application/src/lib.rs:1", result.stdout)
        self.assertIn("MAX_INPUT_BYTES", result.stdout)

    def test_valid_derived_ceiling_inherits_the_source_rationale(self) -> None:
        result = run_checker(
            ENFORCED_FILE,
            "// numeric-bound: ceiling - protects against oversized text\n"
            "const MAX_INPUT_CHARACTERS: usize = 1024;\n"
            "// numeric-bound: derived ceiling from MAX_INPUT_CHARACTERS\n"
            "const MAX_INPUT_BYTES: usize = MAX_INPUT_CHARACTERS * 4;\n",
        )

        self.assertEqual(result.returncode, 0, result.stdout)

    def test_non_bound_false_positive_escape_requires_a_rationale(self) -> None:
        result = run_checker(
            ENFORCED_FILE,
            "// numeric-bound: not-a-bound - fixed percentage representation\n"
            "const MAXIMUM_PERCENT: u64 = 100;\n",
        )

        self.assertEqual(result.returncode, 0, result.stdout)

    def test_non_bound_false_positive_without_rationale_fails(self) -> None:
        result = run_checker(
            ENFORCED_FILE,
            "// numeric-bound: not-a-bound -\nconst MAXIMUM_PERCENT: u64 = 100;\n",
        )

        self.assertEqual(result.returncode, 1, result.stdout)
        self.assertIn("no numeric-bound declaration", result.stdout)

    def test_derived_escape_fails_without_initializer_reference(self) -> None:
        result = run_checker(
            ENFORCED_FILE,
            "// numeric-bound: ceiling - protects against oversized text\n"
            "const MAX_INPUT_CHARACTERS: usize = 1024;\n"
            "// numeric-bound: derived ceiling from MAX_INPUT_CHARACTERS\n"
            "const MAX_INPUT_BYTES: usize = 4096;\n",
        )

        self.assertEqual(result.returncode, 1, result.stdout)
        self.assertIn("invalid derived declaration", result.stdout)

    def test_test_module_bound_is_inventoried_without_gating(self) -> None:
        result = run_checker(
            ENFORCED_FILE,
            "#[cfg(test)]\nmod tests {\n    const MAX_FIXTURE_BYTES: usize = 4;\n}\n",
        )

        self.assertEqual(result.returncode, 0, result.stdout)
        self.assertIn("1 test-only", result.stdout)

    def test_outside_scope_bound_is_inventoried_without_gating(self) -> None:
        result = run_checker(OUTSIDE_FILE, "const MAX_DOMAIN_BYTES: usize = 4;\n")

        self.assertEqual(result.returncode, 0, result.stdout)
        self.assertIn("1 outside blocking scope", result.stdout)

    def test_raw_string_lookalike_is_not_inventoried(self) -> None:
        result = run_checker(
            ENFORCED_FILE,
            'const EXAMPLE: &str = r#"\nconst MAX_FAKE_BYTES: usize = 4;\n"#;\n',
        )

        self.assertEqual(result.returncode, 0, result.stdout)
        self.assertIn("0 enforced", result.stdout)


if __name__ == "__main__":
    unittest.main()
