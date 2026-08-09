#!/usr/bin/env python3
"""Regression tests for clients/native/scripts/summarize-coverage.py.

No Xcode run happens here: xccov report documents are built directly, so the
product/test split and the ordering are exercised deterministically without a
simulator.
"""

from __future__ import annotations

import importlib.util
import contextlib
import io
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


# The line total every case in this file measures against. It is a constant
# rather than a knob: no test varies the denominator, and a percentage over
# 100 lines is the one a reader can check in their head.
CANONICAL_TOTAL = 100


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


def baseline_of(*, covered: int, executable: int = CANONICAL_TOTAL, label: str | None = None) -> object:
    """One baseline whose only knob is the covered count it carries.

    The denominator is defaulted because no test varies it: every case fixes it
    at the canonical total and moves `covered` alone, so repeating it at each
    call made the sites look denominator-sensitive when they are not. `covered`
    stays explicit — the delta each test asserts is derived from it, so it is a
    value the test genuinely cares about (testing-style rules 4 and 5).
    """
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


def one_target_report(*, covered: int, executable: int = CANONICAL_TOTAL) -> dict:
    """One product target at a chosen coverage, which is all a delta needs.

    Same split as `baseline_of`: `covered` is the axis tests move, and the
    denominator is the constant they all share.
    """
    return report(
        target("SignalboxNative.app", source_file("/repo/App.swift", executable=executable, covered=covered))
    )


class BaselineDeltaTests(unittest.TestCase):
    def test_an_improvement_is_rendered_with_a_plus_sign(self) -> None:
        """A delta is only readable with its sign, and the direction is the
        whole reason it is rendered at all."""
        rendered = summarize_coverage.render(
            one_target_report(covered=60),
            REPOSITORY_ROOT,
            0,
            TITLE,
            baseline=baseline_of(covered=50),
        )

        self.assertIn("+10.00 pp", rendered)

    def test_a_regression_is_rendered_with_a_minus_sign(self) -> None:
        """A drop reads as a drop. Nothing acts on it — this measurement
        gates nothing — but a reader must be able to see it."""
        rendered = summarize_coverage.render(
            one_target_report(covered=40),
            REPOSITORY_ROOT,
            0,
            TITLE,
            baseline=baseline_of(covered=50),
        )

        self.assertIn("-10.00 pp", rendered)

    def test_no_change_is_rendered_as_a_signed_zero(self) -> None:
        """An unchanged percentage is a measurement, not a missing one, and
        `+0.00 pp` says so where a silent headline would not."""
        rendered = summarize_coverage.render(
            one_target_report(covered=50),
            REPOSITORY_ROOT,
            0,
            TITLE,
            baseline=baseline_of(covered=50),
        )

        self.assertIn("+0.00 pp", rendered)

    def test_the_unit_is_percentage_points_not_percent(self) -> None:
        """The difference between two percentages is a percentage-point
        difference; writing it as a percentage would misstate it."""
        rendered = summarize_coverage.render(
            one_target_report(covered=60),
            REPOSITORY_ROOT,
            0,
            TITLE,
            baseline=baseline_of(covered=50),
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
            baseline=baseline_of(covered=50),
        )

        # The counters are read back off the product target the fixture built,
        # so a behaviour-preserving change to those numbers moves the
        # expectation with them. Naming the product target here also states
        # that the `.xctest` target is the excluded one, which is the property
        # this test exists for.
        #
        # The percentage stays a literal. Recomputing it as
        # `100.0 * covered / executable` is the body of `percent_value`, so the
        # assertion would agree with the summarizer by construction and pass
        # however wrong that shared arithmetic became.
        product = document["targets"][0]
        self.assertIn(
            f"**60.00%** of product lines covered "
            f"({product['coveredLines']}/{product['executableLines']})",
            rendered,
        )
        self.assertIn("+10.00 pp", rendered)

    def test_a_base_baseline_claims_the_delta_for_this_branch(self) -> None:
        """The run measures this branch merged into its current base, so a
        delta against that same base is the branch's own doing, and the
        report says exactly that."""
        rendered = summarize_coverage.render(
            one_target_report(covered=60),
            REPOSITORY_ROOT,
            0,
            TITLE,
            baseline=baseline_of(covered=50, label=summarize_coverage.BASE),
        )

        self.assertIn("the base this pull request is open against", rendered)
        self.assertIn("this branch's own doing", rendered)

    def test_an_incomplete_run_withdraws_the_attribution(self) -> None:
        """The reporting step runs under `always()`, so a run whose bundles
        crashed still renders a measurement — a partial one. Compared against
        a complete baseline that reads as a large negative delta, which is
        indistinguishable from a real regression unless the report says so."""
        rendered = summarize_coverage.render(
            one_target_report(covered=60),
            REPOSITORY_ROOT,
            0,
            TITLE,
            baseline=baseline_of(covered=50, label=summarize_coverage.BASE),
            measurement_incomplete=True,
        )

        # Whitespace-normalized: this prose is hard-wrapped, so an assertion
        # on a contiguous phrase would break on a rewrap rather than on a
        # change of meaning.
        flowed = " ".join(rendered.split())
        self.assertIn("tests did not finish", flowed)
        self.assertIn("not attributable to this branch", flowed)
        # The unconditional claim must be gone, not merely qualified further
        # down: a reader who stops at the first sentence must not be misled.
        self.assertNotIn("this branch's own doing", flowed)

    def test_an_incomplete_run_still_names_the_exact_base(self) -> None:
        """An unfinished run must still say *which* baseline it compared
        against. The two complete-run branches distinguish the exact base from
        a fallback because the distinction changes what the delta means, and
        rendering both identically here left a reader unable to tell which
        comparison they were looking at."""
        rendered = summarize_coverage.render(
            one_target_report(covered=60),
            REPOSITORY_ROOT,
            0,
            TITLE,
            baseline=baseline_of(covered=50, label=summarize_coverage.BASE),
            measurement_incomplete=True,
        )

        flowed = " ".join(rendered.split())
        self.assertIn("tests did not finish", flowed)
        self.assertIn("is the base this pull request is open against", flowed)

    def test_an_incomplete_run_against_a_fallback_stacks_both_caveats(self) -> None:
        """The case the omission cost most: an unfinished run *and* a fallback
        baseline are two independent reasons the delta is not this branch's,
        and the reader needs both."""
        rendered = summarize_coverage.render(
            one_target_report(covered=60),
            REPOSITORY_ROOT,
            0,
            TITLE,
            baseline=baseline_of(covered=50, label=summarize_coverage.LATEST_MAIN),
            measurement_incomplete=True,
        )

        flowed = " ".join(rendered.split())
        self.assertIn("tests did not finish", flowed)
        self.assertIn("not** the base this pull request is open against", flowed)
        self.assertIn("stacked parent", flowed)

    def test_the_two_incomplete_notes_differ(self) -> None:
        """The guard that makes the two cases above meaningful: they must not
        render the same prose, which is exactly what the defect was."""
        def note(label: str) -> str:
            return " ".join(
                summarize_coverage.render(
                    one_target_report(covered=60),
                    REPOSITORY_ROOT,
                    0,
                    TITLE,
                    baseline=baseline_of(covered=50, label=label),
                    measurement_incomplete=True,
                ).split()
            )

        self.assertNotEqual(
            note(summarize_coverage.BASE), note(summarize_coverage.LATEST_MAIN)
        )

    def test_a_complete_run_keeps_the_attribution(self) -> None:
        """Negative control for the case above: the withdrawal is driven by
        the flag, not present unconditionally."""
        rendered = summarize_coverage.render(
            one_target_report(covered=60),
            REPOSITORY_ROOT,
            0,
            TITLE,
            baseline=baseline_of(covered=50, label=summarize_coverage.BASE),
            measurement_incomplete=False,
        )

        self.assertIn("this branch's own doing", rendered)
        self.assertNotIn("did not finish", rendered)

    def test_a_latest_main_baseline_disclaims_it(self) -> None:
        """The fallback baseline is not the pull request's base, so the delta
        is not attributable to this branch alone. Rendering both the same way
        would make an unattributable number read as an attributable one."""
        rendered = summarize_coverage.render(
            one_target_report(covered=60),
            REPOSITORY_ROOT,
            0,
            TITLE,
            baseline=baseline_of(covered=50, label=summarize_coverage.LATEST_MAIN),
        )

        self.assertIn("**not** the base this pull request is open against", rendered)
        # Whitespace-normalized: this prose is hard-wrapped, so a contiguous
        # phrase would break on a rewrap rather than on a change of meaning.
        flowed = " ".join(rendered.split())
        self.assertIn("what landed on `main` in between", flowed)
        # The fallback also spans a stacked parent's unmerged changes, which
        # can be larger than anything `main` contributed.
        self.assertIn("stacked on another", flowed)
        self.assertIn("dominate the number", flowed)

    def test_the_baseline_commit_and_date_are_named(self) -> None:
        """Native coverage is measured on a main push only when that push
        touched the native client or this workflow file itself, so a
        baseline can be far behind `main`. The date is the only thing that
        tells a reader how far."""
        baseline = baseline_of(covered=50)

        rendered = summarize_coverage.render(
            one_target_report(covered=60),
            REPOSITORY_ROOT,
            0,
            TITLE,
            baseline=baseline,
        )

        self.assertIn(f"`{baseline.sha}`", rendered)
        self.assertIn(baseline.date, rendered)
        self.assertIn("only when that push touched", rendered)


class BaselineAbsenceTests(unittest.TestCase):
    def test_an_unavailable_baseline_is_stated_with_its_reason(self) -> None:
        """A silently missing delta reads as "no change" to anyone who saw
        one last week, so the absence is stated and attributed."""
        reason = "run 42 measured no native coverage"

        rendered = summarize_coverage.render(
            one_target_report(covered=60),
            REPOSITORY_ROOT,
            0,
            TITLE,
            baseline_unavailable=reason,
        )

        self.assertIn(f"No baseline to compare against: {reason}.", rendered)
        self.assertNotIn(" pp", rendered)

    def test_a_caller_that_asked_for_no_baseline_gets_no_baseline_line(self) -> None:
        """A push to main measures the tree that becomes the next baseline
        and compares against nothing, so its report must be unchanged from
        what it was before deltas existed."""
        document = one_target_report(covered=60)

        rendered = summarize_coverage.render(document, REPOSITORY_ROOT, 0, TITLE)

        self.assertNotIn("baseline", rendered.lower())
        # A literal percentage, for the reason given in the delta test above.
        product = document["targets"][0]
        self.assertIn(
            f"**60.00%** of product lines covered "
            f"({product['coveredLines']}/{product['executableLines']}).",
            rendered,
        )


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
                path, label=summarize_coverage.BASE, sha="abc1234", date="2026-08-01T09:00:00Z"
            )

        self.assertEqual(reason, "")
        assert baseline is not None
        # Read back off the product target the fixture built, not off repeated
        # literals: the totals must track the fixture, and naming the product
        # target here also states that the `.xctest` target above is excluded.
        product = document["targets"][0]
        self.assertEqual(baseline.executable, product["executableLines"])
        self.assertEqual(baseline.covered, product["coveredLines"])

    def test_a_baseline_that_is_not_json_reports_a_reason(self) -> None:
        """A truncated download is the likeliest way a baseline goes wrong,
        and it must cost the delta rather than the report."""
        with tempfile.TemporaryDirectory() as directory:
            path = written(directory, "coverage.json", '{"targets": [')

            baseline, reason = summarize_coverage.load_baseline(
                path, label=summarize_coverage.BASE, sha="abc1234", date="2026-08-01T09:00:00Z"
            )

        self.assertIsNone(baseline)
        self.assertIn("could not be read", reason)

    def test_a_baseline_of_another_document_shape_reports_a_reason(self) -> None:
        """An artifact whose layout changed can hand back a file of the
        right name and the wrong shape, which is not a crash."""
        with tempfile.TemporaryDirectory() as directory:
            path = written(directory, "coverage.json", json.dumps({"targets": [{"name": "App.app"}]}))

            baseline, reason = summarize_coverage.load_baseline(
                path, label=summarize_coverage.BASE, sha="abc1234", date="2026-08-01T09:00:00Z"
            )

        self.assertIsNone(baseline)
        self.assertIn("could not be read", reason)

    def test_a_baseline_counter_that_overflows_reports_a_reason(self) -> None:
        """`1e999` is a valid JSON number, and `json.loads` returns it as
        infinity — `int(inf)` then raises `OverflowError`. That escaped the
        guard and ended the whole report step, when an unreadable baseline is
        supposed to cost only the delta.

        The document is otherwise exactly the healthy fixture, so nothing but
        the counter's magnitude can be what makes it unreadable. The workflow's
        `jq` predicate rejects such a counter before it reaches the loader;
        this is the loader keeping its own promise not to raise.
        """
        document = report(target("App.app", source_file("/repo/App.swift", executable=200, covered=50)))
        document["targets"][0]["executableLines"] = "__OVERFLOW__"
        with tempfile.TemporaryDirectory() as directory:
            path = written(
                directory,
                "coverage.json",
                json.dumps(document).replace('"__OVERFLOW__"', "1e999"),
            )

            baseline, reason = summarize_coverage.load_baseline(
                path, label=summarize_coverage.BASE, sha="abc1234", date="2026-08-01T09:00:00Z"
            )

        self.assertIsNone(baseline)
        self.assertIn("could not be read", reason)

    def test_a_fractional_baseline_counter_is_refused_not_truncated(self) -> None:
        """`int(100.9)` truncates to 100 rather than raising, so a baseline
        reporting `executableLines: 100.9` used to be accepted as an ordinary
        50% measurement and rendered a real-looking delta against it, instead
        of being refused and reporting no baseline at all. `covered` and the
        truncated `executable` stay in a sane relationship on purpose, so
        nothing here also trips the impossible-counter guard; the point is
        the type, not the magnitude."""
        current = report(target("App.app", source_file("/repo/App.swift", executable=100, covered=60)))
        baseline_document = report(
            target("App.app", source_file("/repo/App.swift", executable=100, covered=50))
        )
        baseline_document["targets"][0]["executableLines"] = 100.9
        with tempfile.TemporaryDirectory() as directory:
            report_path = written(directory, "coverage.json", json.dumps(current))
            baseline_path = written(directory, "baseline.json", json.dumps(baseline_document))

            with contextlib.redirect_stdout(io.StringIO()) as rendered:
                exit_code = summarize_coverage.main([
                    str(report_path), "--title", TITLE,
                    "--baseline", str(baseline_path),
                    "--baseline-label", summarize_coverage.BASE,
                    "--baseline-sha", "abc1234",
                    "--baseline-date", "2026-08-01T09:00:00Z",
                ])

        self.assertEqual(exit_code, 0)
        self.assertIn("No baseline to compare against", rendered.getvalue())
        self.assertNotIn(" pp", rendered.getvalue())

    def test_a_boolean_baseline_counter_is_refused_not_coerced(self) -> None:
        """`bool` is an `int` subclass in Python, so `int(True)` succeeds as 1
        rather than raising — the same silent-coercion shape as the
        fractional case above, on JSON's other type Xcode never emits for a
        counter."""
        current = report(target("App.app", source_file("/repo/App.swift", executable=100, covered=60)))
        baseline_document = report(
            target("App.app", source_file("/repo/App.swift", executable=1, covered=0))
        )
        baseline_document["targets"][0]["executableLines"] = True
        baseline_document["targets"][0]["coveredLines"] = False
        with tempfile.TemporaryDirectory() as directory:
            report_path = written(directory, "coverage.json", json.dumps(current))
            baseline_path = written(directory, "baseline.json", json.dumps(baseline_document))

            with contextlib.redirect_stdout(io.StringIO()) as rendered:
                exit_code = summarize_coverage.main([
                    str(report_path), "--title", TITLE,
                    "--baseline", str(baseline_path),
                    "--baseline-label", summarize_coverage.BASE,
                    "--baseline-sha", "abc1234",
                    "--baseline-date", "2026-08-01T09:00:00Z",
                ])

        self.assertEqual(exit_code, 0)
        self.assertIn("No baseline to compare against", rendered.getvalue())
        self.assertNotIn(" pp", rendered.getvalue())

    def test_a_counter_too_large_to_render_reports_a_reason(self) -> None:
        """`int()` accepts a 400-digit decimal string, so the loader returned a
        baseline whose percentage then raised `OverflowError` during rendering
        — outside every guard, ending the report instead of degrading to the
        unavailable-baseline note."""
        document = report(target("App.app", source_file("/repo/App.swift", executable=200, covered=50)))
        document["targets"][0]["executableLines"] = int("9" * 400)
        with tempfile.TemporaryDirectory() as directory:
            path = written(directory, "coverage.json", json.dumps(document))

            baseline, reason = summarize_coverage.load_baseline(
                path, label=summarize_coverage.BASE, sha="abc1234", date="2026-08-01T09:00:00Z"
            )

        self.assertIsNone(baseline)
        self.assertIn("could not be read", reason)

    def test_a_baseline_covering_more_than_exists_reports_a_reason(self) -> None:
        """More covered than executable renders a complete current run as a
        fabricated regression instead of degrading to the unavailable note."""
        document = report(target("App.app", source_file("/repo/App.swift", executable=1, covered=2)))
        with tempfile.TemporaryDirectory() as directory:
            path = written(directory, "coverage.json", json.dumps(document))

            baseline, reason = summarize_coverage.load_baseline(
                path, label=summarize_coverage.BASE, sha="abc1234", date="2026-08-01T09:00:00Z"
            )

        self.assertIsNone(baseline)
        self.assertIn("impossible", reason)

    def test_a_fully_covered_baseline_is_still_accepted(self) -> None:
        """The guard: covered == executable is the boundary, not an
        impossibility."""
        document = report(target("App.app", source_file("/repo/App.swift", executable=100, covered=100)))
        with tempfile.TemporaryDirectory() as directory:
            path = written(directory, "coverage.json", json.dumps(document))

            baseline, reason = summarize_coverage.load_baseline(
                path, label=summarize_coverage.BASE, sha="abc1234", date="2026-08-01T09:00:00Z"
            )

        self.assertEqual(reason, "")
        self.assertIsNotNone(baseline)

    def test_compensating_impossible_targets_report_a_reason(self) -> None:
        """Invalid targets cancel in a sum: one at (covered 2, executable 1)
        and another at (0, 1) total to (2, 2), which passes every aggregate
        test while describing nothing real."""
        document = report(
            target("A.app", source_file("/repo/A.swift", executable=1, covered=2)),
            target("B.app", source_file("/repo/B.swift", executable=1, covered=0)),
        )
        with tempfile.TemporaryDirectory() as directory:
            path = written(directory, "coverage.json", json.dumps(document))

            baseline, reason = summarize_coverage.load_baseline(
                path, label=summarize_coverage.BASE, sha="abc1234", date="2026-08-01T09:00:00Z"
            )

        self.assertIsNone(baseline)
        self.assertIn("impossible", reason)

    def test_an_impossible_test_bundle_is_ignored_like_any_other(self) -> None:
        """The guard on which targets are checked: `.xctest` bundles are
        excluded from the measurement, so they are excluded from its validity
        too — the same split `product_totals` and the workflow filter make."""
        document = report(
            target("T.xctest", source_file("/repo/T.swift", executable=1, covered=2)),
            target("A.app", source_file("/repo/A.swift", executable=100, covered=50)),
        )
        with tempfile.TemporaryDirectory() as directory:
            path = written(directory, "coverage.json", json.dumps(document))

            baseline, reason = summarize_coverage.load_baseline(
                path, label=summarize_coverage.BASE, sha="abc1234", date="2026-08-01T09:00:00Z"
            )

        self.assertEqual(reason, "")
        self.assertIsNotNone(baseline)

    def test_a_missing_baseline_file_reports_a_reason(self) -> None:
        """An extraction that produced nothing leaves no file, and reading
        one that is not there is an expected outcome here."""
        with tempfile.TemporaryDirectory() as directory:
            baseline, reason = summarize_coverage.load_baseline(
                Path(directory) / "absent.json",
                label=summarize_coverage.BASE,
                sha="abc1234",
                date="2026-08-01T09:00:00Z",
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
                path, label=summarize_coverage.BASE, sha="abc1234", date="2026-08-01T09:00:00Z"
            )

        self.assertIsNone(baseline)
        self.assertEqual(reason, "the baseline report measured no product lines")


class BaselineProvenanceContractTests(unittest.TestCase):
    """The CLI refuses a baseline it cannot attribute.

    Mirrors the rust summarizer's contract tests, because the two scripts
    mirror each other's argument surface: every provenance value defaulted to
    an empty string, so a caller supplying only `--baseline-label` produced a
    confidently attributed delta above a provenance line reading
    "``, measured .".
    """

    def arguments_for(self, *baseline: str) -> list[str]:
        return [str(self.report), "--title", TITLE, *baseline]

    def setUp(self) -> None:
        self.directory = tempfile.TemporaryDirectory()
        self.addCleanup(self.directory.cleanup)
        payload = json.dumps(one_target_report(covered=50))
        self.report = written(self.directory.name, "coverage.json", payload)
        self.baseline = written(self.directory.name, "baseline.json", payload)

    def test_a_baseline_without_a_sha_is_refused(self) -> None:
        with self.assertRaises(SystemExit) as refusal, contextlib.redirect_stderr(io.StringIO()) as stderr:
            summarize_coverage.main(self.arguments_for(
                "--baseline", str(self.baseline),
                "--baseline-label", summarize_coverage.BASE,
                "--baseline-date", "2026-08-01T09:00:00Z",
            ))

        self.assertNotEqual(refusal.exception.code, 0)
        self.assertIn("--baseline-sha", stderr.getvalue())

    def test_a_baseline_without_a_date_is_refused(self) -> None:
        with self.assertRaises(SystemExit), contextlib.redirect_stderr(io.StringIO()) as stderr:
            summarize_coverage.main(self.arguments_for(
                "--baseline", str(self.baseline),
                "--baseline-label", summarize_coverage.BASE,
                "--baseline-sha", "abc1234",
            ))

        self.assertIn("--baseline-date", stderr.getvalue())

    def test_complete_provenance_is_accepted(self) -> None:
        with contextlib.redirect_stdout(io.StringIO()) as rendered:
            exit_code = summarize_coverage.main(self.arguments_for(
                "--baseline", str(self.baseline),
                "--baseline-label", summarize_coverage.BASE,
                "--baseline-sha", "abc1234",
                "--baseline-date", "2026-08-01T09:00:00Z",
            ))

        self.assertEqual(exit_code, 0)
        self.assertIn("abc1234", rendered.getvalue())

    def test_no_baseline_needs_no_provenance(self) -> None:
        with contextlib.redirect_stdout(io.StringIO()):
            exit_code = summarize_coverage.main(self.arguments_for())

        self.assertEqual(exit_code, 0)


if __name__ == "__main__":
    unittest.main()
