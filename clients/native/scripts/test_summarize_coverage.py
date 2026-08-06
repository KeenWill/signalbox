#!/usr/bin/env python3
"""Regression tests for clients/native/scripts/summarize-coverage.py.

No Xcode run happens here: xccov report documents are built directly, so the
product/test split and the ordering are exercised deterministically without a
simulator.
"""

from __future__ import annotations

import importlib.util
import json
import sys
import tempfile
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


def baseline_of(*, executable: int, covered: int, label: str | None = None) -> object:
    """One baseline whose only knob is the line percentage it carries."""
    return summarize_coverage.Baseline(
        covered=covered,
        executable=executable,
        label=summarize_coverage.BASE if label is None else label,
        sha="abc1234",
        date="2026-08-01T09:00:00Z",
    )


def written(directory: str, name: str, content: str) -> Path:
    """Write one file for the loader to read, since the loader reads paths."""
    path = Path(directory) / name
    path.write_text(content, encoding="utf-8")
    return path


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


TITLE = "Native client coverage (report only)"


def one_target_report(*, executable: int, covered: int) -> dict:
    """One product target at a chosen coverage, which is all a delta needs."""
    return report(
        target("SignalboxNative.app", source_file("/repo/App.swift", executable=executable, covered=covered))
    )


class BaselineDeltaTests(unittest.TestCase):
    def test_an_improvement_is_rendered_with_a_plus_sign(self) -> None:
        """A delta is only readable with its sign, and the direction is the
        whole reason it is rendered at all."""
        rendered = summarize_coverage.render(
            one_target_report(executable=100, covered=60),
            REPOSITORY_ROOT,
            0,
            TITLE,
            baseline=baseline_of(executable=100, covered=50),
        )

        self.assertIn("+10.00 pp", rendered)

    def test_a_regression_is_rendered_with_a_minus_sign(self) -> None:
        """A drop reads as a drop. Nothing acts on it — this measurement
        gates nothing — but a reader must be able to see it."""
        rendered = summarize_coverage.render(
            one_target_report(executable=100, covered=40),
            REPOSITORY_ROOT,
            0,
            TITLE,
            baseline=baseline_of(executable=100, covered=50),
        )

        self.assertIn("-10.00 pp", rendered)

    def test_no_change_is_rendered_as_a_signed_zero(self) -> None:
        """An unchanged percentage is a measurement, not a missing one, and
        `+0.00 pp` says so where a silent headline would not."""
        rendered = summarize_coverage.render(
            one_target_report(executable=100, covered=50),
            REPOSITORY_ROOT,
            0,
            TITLE,
            baseline=baseline_of(executable=100, covered=50),
        )

        self.assertIn("+0.00 pp", rendered)

    def test_the_unit_is_percentage_points_not_percent(self) -> None:
        """The difference between two percentages is a percentage-point
        difference; writing it as a percentage would misstate it."""
        rendered = summarize_coverage.render(
            one_target_report(executable=100, covered=60),
            REPOSITORY_ROOT,
            0,
            TITLE,
            baseline=baseline_of(executable=100, covered=50),
        )

        self.assertNotIn("+10.00%", rendered)

    def test_the_delta_excludes_test_bundles_exactly_as_the_total_does(self) -> None:
        """A delta taken over a different denominator than the headline
        would contradict the number it sits beside."""
        document = report(
            target("SignalboxNative.app", source_file("/repo/App.swift", executable=100, covered=60)),
            target("Tests.xctest", source_file("/repo/Tests.swift", executable=900, covered=900)),
        )

        rendered = summarize_coverage.render(
            document,
            REPOSITORY_ROOT,
            0,
            TITLE,
            baseline=baseline_of(executable=100, covered=50),
        )

        self.assertIn("**60.00%** of product lines covered (60/100)", rendered)
        self.assertIn("+10.00 pp", rendered)

    def test_a_base_baseline_claims_the_delta_for_this_branch(self) -> None:
        """The run measures this branch merged into its current base, so a
        delta against that same base is the branch's own doing, and the
        report says exactly that."""
        rendered = summarize_coverage.render(
            one_target_report(executable=100, covered=60),
            REPOSITORY_ROOT,
            0,
            TITLE,
            baseline=baseline_of(executable=100, covered=50, label=summarize_coverage.BASE),
        )

        self.assertIn("the base this pull request is open against", rendered)
        self.assertIn("this branch's own doing", rendered)

    def test_a_latest_main_baseline_disclaims_it(self) -> None:
        """The fallback baseline is not the pull request's base, so the delta
        is not attributable to this branch alone. Rendering both the same way
        would make an unattributable number read as an attributable one."""
        rendered = summarize_coverage.render(
            one_target_report(executable=100, covered=60),
            REPOSITORY_ROOT,
            0,
            TITLE,
            baseline=baseline_of(executable=100, covered=50, label=summarize_coverage.LATEST_MAIN),
        )

        self.assertIn("**not** the base this pull request is open against", rendered)
        self.assertIn("carries whatever else landed on `main` in between", rendered)

    def test_the_baseline_commit_and_date_are_named(self) -> None:
        """Native coverage is measured on a main push only when that push
        touched the native client, so a baseline can be far behind `main`.
        The date is the only thing that tells a reader how far."""
        rendered = summarize_coverage.render(
            one_target_report(executable=100, covered=60),
            REPOSITORY_ROOT,
            0,
            TITLE,
            baseline=baseline_of(executable=100, covered=50),
        )

        self.assertIn("`abc1234`", rendered)
        self.assertIn("2026-08-01T09:00:00Z", rendered)
        self.assertIn("only when that push touched", rendered)


class BaselineAbsenceTests(unittest.TestCase):
    def test_an_unavailable_baseline_is_stated_with_its_reason(self) -> None:
        """A silently missing delta reads as "no change" to anyone who saw
        one last week, so the absence is stated and attributed."""
        rendered = summarize_coverage.render(
            one_target_report(executable=100, covered=60),
            REPOSITORY_ROOT,
            0,
            TITLE,
            baseline_unavailable="run 42 measured no native coverage",
        )

        self.assertIn("No baseline to compare against: run 42 measured no native coverage.", rendered)
        self.assertNotIn(" pp", rendered)

    def test_a_caller_that_asked_for_no_baseline_gets_no_baseline_line(self) -> None:
        """A push to main measures the tree that becomes the next baseline
        and compares against nothing, so its report must be unchanged from
        what it was before deltas existed."""
        rendered = summarize_coverage.render(
            one_target_report(executable=100, covered=60), REPOSITORY_ROOT, 0, TITLE
        )

        self.assertNotIn("baseline", rendered.lower())
        self.assertIn("**60.00%** of product lines covered (60/100).", rendered)


class BaselineLoadingTests(unittest.TestCase):
    def test_a_well_formed_baseline_loads_with_no_reason(self) -> None:
        """The happy path totals the same product targets the current
        report totals, so the two numbers are comparable."""
        document = report(
            target("SignalboxNative.app", source_file("/repo/App.swift", executable=200, covered=50)),
            target("Tests.xctest", source_file("/repo/Tests.swift", executable=90, covered=90)),
        )
        with tempfile.TemporaryDirectory() as directory:
            path = written(directory, "coverage.json", json.dumps(document))

            baseline, reason = summarize_coverage.load_baseline(
                path, summarize_coverage.BASE, "abc1234", "2026-08-01T09:00:00Z"
            )

        self.assertEqual(reason, "")
        assert baseline is not None
        self.assertEqual(baseline.executable, 200)
        self.assertEqual(baseline.covered, 50)

    def test_a_baseline_that_is_not_json_reports_a_reason(self) -> None:
        """A truncated download is the likeliest way a baseline goes wrong,
        and it must cost the delta rather than the report."""
        with tempfile.TemporaryDirectory() as directory:
            path = written(directory, "coverage.json", '{"targets": [')

            baseline, reason = summarize_coverage.load_baseline(
                path, summarize_coverage.BASE, "abc1234", "2026-08-01T09:00:00Z"
            )

        self.assertIsNone(baseline)
        self.assertIn("could not be read", reason)

    def test_a_baseline_of_another_document_shape_reports_a_reason(self) -> None:
        """An artifact whose layout changed can hand back a file of the
        right name and the wrong shape, which is not a crash."""
        with tempfile.TemporaryDirectory() as directory:
            path = written(directory, "coverage.json", json.dumps({"targets": [{"name": "App.app"}]}))

            baseline, reason = summarize_coverage.load_baseline(
                path, summarize_coverage.BASE, "abc1234", "2026-08-01T09:00:00Z"
            )

        self.assertIsNone(baseline)
        self.assertIn("could not be read", reason)

    def test_a_missing_baseline_file_reports_a_reason(self) -> None:
        """An extraction that produced nothing leaves no file, and reading
        one that is not there is an expected outcome here."""
        with tempfile.TemporaryDirectory() as directory:
            baseline, reason = summarize_coverage.load_baseline(
                Path(directory) / "absent.json",
                summarize_coverage.BASE,
                "abc1234",
                "2026-08-01T09:00:00Z",
            )

        self.assertIsNone(baseline)
        self.assertIn("could not be read", reason)

    def test_a_baseline_measuring_no_product_lines_is_refused(self) -> None:
        """An empty target reports as fully covered by this tool's own
        convention, so a test-bundles-only baseline would render every
        current number as a large regression."""
        document = report(target("Tests.xctest", source_file("/repo/Tests.swift", executable=10, covered=10)))
        with tempfile.TemporaryDirectory() as directory:
            path = written(directory, "coverage.json", json.dumps(document))

            baseline, reason = summarize_coverage.load_baseline(
                path, summarize_coverage.BASE, "abc1234", "2026-08-01T09:00:00Z"
            )

        self.assertIsNone(baseline)
        self.assertEqual(reason, "the baseline report measured no product lines")


if __name__ == "__main__":
    unittest.main()
