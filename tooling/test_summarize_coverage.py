#!/usr/bin/env python3
"""Regression tests for tooling/summarize_coverage.py.

No coverage run happens here: llvm-cov export documents are built directly, so
the aggregation, ordering, and omission-tolerance logic is exercised in
isolation and deterministically, without a compile or an instrumented test run.
"""

from __future__ import annotations

import sys
import unittest
from pathlib import Path

sys.path.insert(0, str(Path(__file__).resolve().parent))

from summarize_coverage import (  # noqa: E402
    EXPORT_TYPE,
    OUTSIDE_WORKSPACE,
    Counter,
    aggregate_by_crate,
    crate_of,
    least_covered_files,
    read_file_summaries,
    render,
    total_of,
)

REPO_ROOT = Path("/repo")


def coverage_file(filename: str, *, lines: int, covered: int) -> dict:
    """One export file entry whose single knob is its line coverage.

    The function and region counters are derived from that same knob rather
    than taken as free integers: no test below depends on them varying
    independently, and deriving them keeps a reader from reconciling three
    unrelated numbers to understand one row.
    """
    return {
        "filename": filename,
        "summary": {
            "lines": {"count": lines, "covered": covered},
            "functions": {"count": lines, "covered": covered},
            "regions": {"count": lines, "covered": covered},
        },
    }


def export(*files: dict) -> dict:
    """One llvm-cov export document wrapping the given file entries."""
    return {"type": EXPORT_TYPE, "version": "2.0.1", "data": [{"files": list(files)}]}


class CrateAttributionTests(unittest.TestCase):
    def test_files_under_one_crate_directory_aggregate_into_one_row(self) -> None:
        """Two files under crates/domain report as a single crate whose
        counters are their sum, which is the whole point of this tool over
        `cargo llvm-cov`'s per-file table."""
        document = export(
            coverage_file("/repo/crates/domain/src/session.rs", lines=100, covered=40),
            coverage_file("/repo/crates/domain/src/turn.rs", lines=100, covered=60),
        )

        crates = aggregate_by_crate(read_file_summaries(document), REPO_ROOT)

        self.assertEqual(list(crates), ["crates/domain"])
        self.assertEqual(crates["crates/domain"].lines.count, 200)
        self.assertEqual(crates["crates/domain"].lines.covered, 100)

    def test_an_app_member_is_named_by_its_own_directory(self) -> None:
        """Workspace members live under apps/ as well as crates/, and an
        app is attributed to itself rather than collapsing into one apps row."""
        self.assertEqual(crate_of("/repo/apps/signalboxd/src/main.rs", REPO_ROOT), "apps/signalboxd")

    def test_a_path_outside_the_checkout_is_bucketed_not_dropped(self) -> None:
        """A registry or toolchain path that survives into the export is
        kept under one bucket, because the table's total is the sum of its
        rows and a dropped file would make that total disagree with them."""
        self.assertEqual(crate_of("/elsewhere/vendor/lib.rs", REPO_ROOT), OUTSIDE_WORKSPACE)

    def test_a_file_outside_any_member_directory_is_named_by_its_top_level(self) -> None:
        """A build script or generated file directly under the checkout is
        attributed to its top-level directory rather than to a crate."""
        self.assertEqual(crate_of("/repo/tooling/helper.rs", REPO_ROOT), "tooling")


class TotalsTests(unittest.TestCase):
    def test_the_total_is_the_sum_of_the_crate_rows(self) -> None:
        """The reported total is computed from the same rows the table
        renders, so a reader can add the column up and get the total."""
        document = export(
            coverage_file("/repo/crates/domain/src/session.rs", lines=100, covered=40),
            coverage_file("/repo/apps/signalboxd/src/main.rs", lines=50, covered=5),
        )

        crates = aggregate_by_crate(read_file_summaries(document), REPO_ROOT)
        total = total_of(list(crates.values()))

        self.assertEqual(total.lines.count, 150)
        self.assertEqual(total.lines.covered, 45)

    def test_an_empty_counter_reports_fully_covered(self) -> None:
        """llvm-cov reports 100% for a counter with nothing to cover, and
        this tool matches it so a row stays comparable with llvm-cov output."""
        self.assertEqual(Counter(count=0, covered=0).percent, 100.0)

    def test_a_counter_the_toolchain_omits_reads_as_empty(self) -> None:
        """llvm-cov has added counters across releases, so a summary block
        missing one this report renders yields an empty counter rather than a
        crash on the next toolchain bump."""
        document = export({"filename": "/repo/crates/domain/src/session.rs", "summary": {"lines": {"count": 10, "covered": 3}}})

        files = read_file_summaries(document)

        self.assertEqual(files[0][1].lines.covered, 3)
        self.assertEqual(files[0][1].regions.count, 0)

    def test_a_document_of_another_type_is_refused(self) -> None:
        """An lcov file or a cargo-metadata document passed by mistake is
        rejected by name rather than silently summarized as zero coverage."""
        with self.assertRaises(ValueError):
            read_file_summaries({"type": "cargo.metadata", "data": []})


class LeastCoveredTests(unittest.TestCase):
    def test_ranking_is_by_uncovered_lines_not_by_percentage(self) -> None:
        """A large mostly-tested file can hide more untested code than a
        tiny file at 0%, so the list that answers 'what is still untested'
        ranks by uncovered line count."""
        document = export(
            coverage_file("/repo/crates/domain/src/large.rs", lines=1000, covered=900),
            coverage_file("/repo/crates/domain/src/tiny.rs", lines=3, covered=0),
        )

        listed = least_covered_files(read_file_summaries(document), REPO_ROOT, 2)

        self.assertIn("crates/domain/src/large.rs", listed[0])
        self.assertIn("crates/domain/src/tiny.rs", listed[1])

    def test_a_fully_covered_file_is_absent(self) -> None:
        """The list exists to name gaps, so a file with no uncovered line
        is not a gap and does not appear."""
        document = export(
            coverage_file("/repo/crates/domain/src/covered.rs", lines=40, covered=40),
            coverage_file("/repo/crates/domain/src/partial.rs", lines=40, covered=39),
        )

        listed = least_covered_files(read_file_summaries(document), REPO_ROOT, 5)

        self.assertEqual(len(listed), 1)
        self.assertIn("crates/domain/src/partial.rs", listed[0])

    def test_a_file_with_no_instrumented_lines_is_absent(self) -> None:
        """A file llvm-cov instrumented nothing in carries no signal
        about untested code and would otherwise sort as a spurious gap."""
        document = export(coverage_file("/repo/crates/domain/src/empty.rs", lines=0, covered=0))

        self.assertEqual(least_covered_files(read_file_summaries(document), REPO_ROOT, 5), [])


class RenderTests(unittest.TestCase):
    def test_crates_are_ordered_least_covered_first(self) -> None:
        """The report opens on the crates with the most untested code,
        because that ordering is what makes the table worth reading."""
        document = export(
            coverage_file("/repo/crates/domain/src/session.rs", lines=100, covered=90),
            coverage_file("/repo/crates/persistence/src/session.rs", lines=100, covered=10),
        )

        report = render(document, REPO_ROOT, 0, "Rust coverage (report only)")
        persistence_row = report.index("`crates/persistence`")
        domain_row = report.index("`crates/domain`")

        self.assertLess(persistence_row, domain_row)

    def test_the_report_states_that_it_gates_nothing(self) -> None:
        """The report is a measurement, and it says so in its own body so
        a reader who meets it on a pull request cannot mistake it for a gate."""
        document = export(coverage_file("/repo/crates/domain/src/session.rs", lines=10, covered=1))

        report = render(document, REPO_ROOT, 0, "Rust coverage (report only)")

        self.assertIn("gates no merge", report)

    def test_the_caller_preamble_precedes_the_first_figure(self) -> None:
        """What a run excluded is stated before any percentage, so a
        reader cannot meet a number before meeting its denominator."""
        document = export(coverage_file("/repo/crates/domain/src/session.rs", lines=10, covered=1))

        report = render(document, REPO_ROOT, 0, "Rust coverage (report only)", "The doctests were not measured.")

        self.assertLess(report.index("The doctests were not measured."), report.index("| Lines |"))

    def test_the_uncovered_section_is_omitted_when_no_files_are_requested(self) -> None:
        """A caller rendering only the crate table (a narrow pull-request
        comment, say) asks for zero files and gets no empty section."""
        document = export(coverage_file("/repo/crates/domain/src/session.rs", lines=10, covered=1))

        report = render(document, REPO_ROOT, 0, "Rust coverage (report only)")

        self.assertNotIn("uncovered lines", report)


if __name__ == "__main__":
    unittest.main()
