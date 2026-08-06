#!/usr/bin/env python3
"""Regression tests for clients/native/scripts/summarize-coverage.py.

No Xcode run happens here: xccov report documents are built directly, so the
product/test split and the ordering are exercised deterministically without a
simulator.
"""

from __future__ import annotations

import importlib.util
import sys
import unittest
from pathlib import Path

SUMMARIZER = Path(__file__).resolve().parent / "summarize-coverage.py"
_specification = importlib.util.spec_from_file_location("summarize_coverage", SUMMARIZER)
assert _specification is not None and _specification.loader is not None
summarize_coverage = importlib.util.module_from_spec(_specification)
sys.modules["summarize_coverage"] = summarize_coverage
_specification.loader.exec_module(summarize_coverage)

REPOSITORY_ROOT = Path("/repo")


def source_file(path: str, *, executable: int, covered: int) -> dict:
    """One xccov file entry whose single knob is its line coverage."""
    return {
        "name": Path(path).name,
        "path": path,
        "executableLines": executable,
        "coveredLines": covered,
        "lineCoverage": 0.0 if executable == 0 else covered / executable,
    }


def target(name: str, *files: dict) -> dict:
    """One xccov target entry totalling the files it carries."""
    executable = sum(source["executableLines"] for source in files)
    covered = sum(source["coveredLines"] for source in files)
    return {
        "name": name,
        "executableLines": executable,
        "coveredLines": covered,
        "lineCoverage": 0.0 if executable == 0 else covered / executable,
        "files": list(files),
    }


def report(*targets: dict) -> dict:
    """One xccov report document wrapping the given targets."""
    return {"targets": list(targets)}


class ProductSplitTests(unittest.TestCase):
    def test_a_test_bundle_is_excluded_from_the_headline_total(self) -> None:
        """An `.xctest` bundle runs its own code by construction, so
        counting it would report how thoroughly the tests test themselves;
        the headline covers product targets alone."""
        product = source_file("/repo/App.swift", executable=100, covered=50)
        document = report(
            target("SignalboxNative.app", product),
            target("Tests.xctest", source_file("/repo/Tests.swift", executable=100, covered=100)),
        )

        rendered = summarize_coverage.render(document, REPOSITORY_ROOT, 0, "Native client coverage (report only)")

        # The counts come from the fixture, so a fixture edit cannot make this
        # fail while the summarizer is right. The percentage stays a hardcoded
        # literal: recomputing it here would mirror the code under test.
        self.assertIn(
            f"({product['coveredLines']}/{product['executableLines']})",
            rendered,
        )
        self.assertIn("**50.00%** of product lines covered", rendered)

    def test_the_excluded_bundles_are_named(self) -> None:
        """What a number leaves out is stated with the number, so a reader
        can tell that the excluded bundles are the reason it is not higher."""
        document = report(
            target("SignalboxNative.app", source_file("/repo/App.swift", executable=10, covered=5)),
            target("Tests.xctest", source_file("/repo/Tests.swift", executable=10, covered=10)),
        )

        rendered = summarize_coverage.render(document, REPOSITORY_ROOT, 0, "Native client coverage (report only)")

        self.assertIn("`Tests.xctest`", rendered)

    def test_a_run_with_no_test_bundles_says_none_were_excluded(self) -> None:
        """The exclusion sentence is unconditional, so it must read
        correctly when there is nothing to exclude."""
        document = report(target("SignalboxNative.app", source_file("/repo/App.swift", executable=10, covered=5)))

        rendered = summarize_coverage.render(document, REPOSITORY_ROOT, 0, "Native client coverage (report only)")

        self.assertIn("Excluded here: none.", rendered)


class UncoveredFileTests(unittest.TestCase):
    def test_ranking_is_by_uncovered_lines_not_by_percentage(self) -> None:
        """A large mostly-tested view file can hide more untested code
        than a tiny file at 0%, and the list exists to find untested code."""
        document = report(
            target(
                "SignalboxNative.app",
                source_file("/repo/Large.swift", executable=1000, covered=900),
                source_file("/repo/Tiny.swift", executable=3, covered=0),
            )
        )

        rows = summarize_coverage.file_rows(document["targets"], REPOSITORY_ROOT, 2)

        self.assertIn("Large.swift", rows[0])
        self.assertIn("Tiny.swift", rows[1])

    def test_a_fully_covered_file_is_absent(self) -> None:
        """The list names gaps, and a file with no uncovered line is not
        a gap."""
        document = report(
            target(
                "SignalboxNative.app",
                source_file("/repo/Covered.swift", executable=40, covered=40),
                source_file("/repo/Partial.swift", executable=40, covered=39),
            )
        )

        rows = summarize_coverage.file_rows(document["targets"], REPOSITORY_ROOT, 5)

        self.assertEqual(len(rows), 1)
        self.assertIn("Partial.swift", rows[0])

    def test_a_path_outside_the_checkout_is_rendered_verbatim(self) -> None:
        """A derived-data or SDK path that reaches the report keeps its
        absolute form rather than producing a misleading relative one."""
        self.assertEqual(
            summarize_coverage.relative_path("/elsewhere/Generated.swift", REPOSITORY_ROOT),
            "/elsewhere/Generated.swift",
        )


class ReportOnlyTests(unittest.TestCase):
    def test_the_report_states_that_it_gates_nothing(self) -> None:
        """A reader meeting this on a pull request must not mistake a
        measurement for a gate, so the report says so in its own body."""
        document = report(target("SignalboxNative.app", source_file("/repo/App.swift", executable=10, covered=1)))

        rendered = summarize_coverage.render(document, REPOSITORY_ROOT, 0, "Native client coverage (report only)")

        self.assertIn("gates no merge", rendered)


if __name__ == "__main__":
    unittest.main()
