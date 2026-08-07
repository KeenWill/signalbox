#!/usr/bin/env python3
"""Render one `xcrun xccov view --report --json` document as Markdown.

Xcode measures every target the scheme builds, the `.xctest` bundles included,
and its own total counts them. A number that counts test bundles largely
measures how thoroughly the tests execute themselves, so this splits the
product targets from the test bundles and totals only the product targets.

The report measures; it decides nothing. There is no threshold here and no
exit code that depends on a percentage.

A second such document can be supplied as `--baseline`, in which case the
headline also carries the signed difference against it. That difference is
informational in exactly the sense the rest of this tool is: it is rendered, it
is labelled with which baseline produced it, and nothing reads it back. An
unreadable or missing baseline is reported in the body of the report and
changes no exit code.
"""

from __future__ import annotations

import argparse
import json
import sys
from dataclasses import dataclass
from pathlib import Path

# Xcode names a unit- or UI-test bundle target with this extension, and a
# product target with .app, .framework, or .xctest-free names. The split is by
# extension because that is what the report document carries; nothing else in
# it distinguishes product code from test code.
TEST_BUNDLE_SUFFIX = ".xctest"

# The two baselines a caller can supply, which are not interchangeable, and
# which one is attributable follows from what the workflow measures. A
# `pull_request` run measures the merge of this branch's head into the base it
# is currently open against, so the only subtraction that isolates this branch
# is one against that same base: a BASE baseline was measured at the pull
# request's current base commit, and a delta against it is this branch's own
# doing. A LATEST_MAIN baseline was measured at whatever `main` most recently
# ran, which may be behind or ahead of that base, so a delta against it also
# carries every other change that landed in between. (The branch's merge-base
# is deliberately not offered: against a merged measurement it would attribute
# everything that landed on `main` since the fork point to this branch.)
# Rendering the same delta without naming which one produced it would let an
# unattributable number read as an attributable one, so the label is required
# whenever a baseline is.
BASE = "base"
LATEST_MAIN = "latest-main"
BASELINE_LABELS = (BASE, LATEST_MAIN)


@dataclass
class Baseline:
    """One earlier measurement the current one is reported against.

    Only the product-line totals are kept: the delta this renders is an
    aggregate, and holding the baseline's per-file detail would invite a
    per-file diff into a comment that is meant to stay one screen tall.
    """

    covered: int
    executable: int
    label: str
    sha: str
    date: str


def is_test_bundle(target: dict) -> bool:
    return str(target.get("name", "")).endswith(TEST_BUNDLE_SUFFIX)


def percent_value(covered: int, executable: int) -> float:
    """Covered share, with an empty target reported as fully covered."""
    if executable == 0:
        return 100.0
    return 100.0 * covered / executable


def percent(covered: int, executable: int) -> str:
    return f"{percent_value(covered, executable):.2f}%"


def delta(covered: int, executable: int, baseline: Baseline) -> str:
    """Render the signed difference between this run and its baseline.

    The unit is percentage points, not percent: the difference between two
    percentages is a percentage-point difference, and writing it with a `%`
    would overstate or understate it by the ratio of the two denominators.
    The sign is always shown, including on an exact zero, so a reader can tell
    "measured, unchanged" from "not measured".
    """
    difference = percent_value(covered, executable) - percent_value(
        baseline.covered, baseline.executable
    )
    return f"{difference:+.2f} pp"


def product_totals(document: dict) -> tuple[int, int]:
    """Sum covered and executable lines over the product targets alone."""
    products = [target for target in document.get("targets", []) if not is_test_bundle(target)]
    return (
        sum(int(target["coveredLines"]) for target in products),
        sum(int(target["executableLines"]) for target in products),
    )


def load_baseline(path: Path, label: str, sha: str, date: str) -> tuple["Baseline | None", str]:
    """Read a baseline report, or say why it cannot be read.

    Every failure here returns a reason rather than raising: a baseline is an
    optional convenience on a report-only measurement, and an unreadable one
    must degrade the report to its absolute numbers instead of costing the
    report. The reasons are distinguished because "the artifact was never
    written" and "the artifact is not the document it claims to be" call for
    different fixes.
    """
    try:
        document = json.loads(path.read_text(encoding="utf-8"))
        covered, executable = product_totals(document)
    except (OSError, ValueError, KeyError, TypeError, AttributeError) as error:
        return None, f"the baseline report could not be read ({error.__class__.__name__})"
    # A report with no executable product line is not a measurement to compare
    # against; `percent_value` would report it as 100% covered and every delta
    # below it as a large regression.
    if executable == 0:
        return None, "the baseline report measured no product lines"
    return Baseline(covered=covered, executable=executable, label=label, sha=sha, date=date), ""


def baseline_note(baseline: Baseline, measurement_incomplete: bool = False) -> list[str]:
    """State which baseline the delta is against, and how old it may be.

    Hard-wrapped like the rest of this report's prose, with the two
    interpolated values on a line of their own so a long SHA or timestamp
    cannot leave one line three times the width of its neighbours.

    The staleness sentence is native-specific and unconditional. This workflow
    measures a `main` push only when that push touched the native client, so
    the most recent measured `main` commit can be well behind `main` itself —
    a fact a reader cannot recover from the label and can from the date.

    `measurement_incomplete` says the test run that produced *this* report did
    not finish. The reporting step runs under `always()` precisely so a partial
    measurement still names untested code, which means the delta can be the
    tests that never ran rather than a code change. That is not a caveat the
    prose can leave to the reader, because a large negative delta is exactly
    what a real regression looks like.
    """
    measured = f"`{baseline.sha}`, measured {baseline.date}."
    if measurement_incomplete:
        provenance = [
            "**This run's tests did not finish**, so the measurement above",
            "covers only the bundles that ran. The delta is therefore not",
            "attributable to this branch: comparing a partial measurement",
            "against a complete baseline shows the tests that were lost, at",
            "whatever size they happen to be. The baseline it is measured",
            "against is",
            measured,
        ]
    elif baseline.label == BASE:
        provenance = [
            "Compared against the base this pull request is open against:",
            measured,
            "This run measured that base with this branch merged into it, so the",
            "delta above is this branch's own doing.",
        ]
    else:
        provenance = [
            "Compared against the most recent `main` push this workflow measured:",
            measured,
            "That is **not** the base this pull request is open against, so the",
            "delta above carries every difference between that commit and the",
            "actual base — what landed on `main` in between, and, when this",
            "pull request is stacked on another, that parent's unmerged",
            "changes as well. A stacked parent can dominate the number.",
        ]
    return provenance + [
        "",
        "Native coverage is measured on a `main` push only when that push touched",
        "`clients/native/**`, so a baseline can sit many commits behind `main`",
        "whichever of the two it is; the date above is how old this one is. The",
        "delta is in percentage points, over a file set that differs from the",
        "baseline's wherever a file was added or removed.",
    ]


def baseline_unavailable_note(reason: str) -> list[str]:
    """Say that there is no baseline, and why, rather than showing nothing.

    A silently absent delta reads as "no change" to anyone who saw one on the
    previous pull request, so the absence is stated with its cause.
    """
    return [
        f"No baseline to compare against: {reason}.",
        "The figure above is absolute; no delta is shown.",
    ]


def target_rows(targets: list[dict]) -> list[str]:
    """One row per target, least-covered first, ties broken by name."""
    ordered = sorted(targets, key=lambda target: (target.get("lineCoverage", 0.0), target["name"]))
    return [
        "| `{name}` | {share} | {covered}/{executable} |".format(
            name=target["name"],
            share=percent(target["coveredLines"], target["executableLines"]),
            covered=target["coveredLines"],
            executable=target["executableLines"],
        )
        for target in ordered
    ]


def file_rows(targets: list[dict], repository_root: Path, limit: int) -> list[str]:
    """One row per source file with uncovered lines, most uncovered first.

    Ordering is by uncovered line count rather than percentage: a large mostly
    tested file can hide more untested code than a small file at zero.
    """
    files = [source for target in targets for source in target.get("files", [])]
    gaps = [
        source
        for source in files
        if source["executableLines"] > 0 and source["coveredLines"] < source["executableLines"]
    ]
    ordered = sorted(
        gaps,
        key=lambda source: (-(source["executableLines"] - source["coveredLines"]), source["path"]),
    )
    return [
        "| `{path}` | {uncovered} | {share} |".format(
            path=relative_path(source["path"], repository_root),
            uncovered=source["executableLines"] - source["coveredLines"],
            share=percent(source["coveredLines"], source["executableLines"]),
        )
        for source in ordered[:limit]
    ]


def relative_path(path: str, repository_root: Path) -> str:
    """Render a path relative to the checkout, or verbatim when outside it."""
    try:
        return str(Path(path).resolve().relative_to(repository_root))
    except ValueError:
        return path


def render(
    document: dict,
    repository_root: Path,
    limit: int,
    title: str,
    *,
    baseline: "Baseline | None" = None,
    baseline_unavailable: str = "",
    measurement_incomplete: bool = False,
) -> str:
    """Render the whole report, with a delta only when a baseline was supplied.

    The baseline is optional in both directions, and its two absent cases are
    not the same. A caller that never asked for one — a `main` push measuring
    the tree that becomes the next baseline — passes neither argument and gets
    exactly the report it got before. A caller that asked and could not get one
    passes `baseline_unavailable` and gets that stated in place of the delta.
    Nothing here gates on a delta; the delta is a sentence.
    """
    products = [target for target in document.get("targets", []) if not is_test_bundle(target)]
    bundles = [target for target in document.get("targets", []) if is_test_bundle(target)]
    covered, executable = product_totals(document)

    headline = f"**{percent(covered, executable)}** of product lines covered ({covered}/{executable})"
    if baseline is not None:
        headline += f", **{delta(covered, executable, baseline)}** against the baseline below"
    lines = [
        f"## {title}",
        "",
        "Report only. This measurement has no threshold, gates no merge, and",
        "fails no check; it exists so untested code stays visible.",
        "",
        headline + ".",
    ]
    if baseline is not None:
        lines.extend(["", *baseline_note(baseline, measurement_incomplete)])
    elif baseline_unavailable:
        lines.extend(["", *baseline_unavailable_note(baseline_unavailable)])
    lines.extend(
        [
            "",
            "The `.xctest` bundles Xcode also measures are excluded from that total:",
            "counting them would measure how thoroughly the tests run themselves.",
            "Excluded here: "
            + (", ".join(f"`{target['name']}`" for target in bundles) if bundles else "none")
            + ".",
            "",
            "| Product target | Line % | Lines |",
            "| --- | ---: | ---: |",
        ]
    )
    lines.extend(target_rows(products))

    gaps = file_rows(products, repository_root, limit)
    if gaps:
        lines.extend(
            [
                "",
                f"### {len(gaps)} files with the most uncovered lines",
                "",
                "| File | Uncovered lines | Line % |",
                "| --- | ---: | ---: |",
            ]
        )
        lines.extend(gaps)

    return "\n".join(lines) + "\n"


def main(argv: list[str]) -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("json", type=Path, help="xccov JSON report to summarize")
    parser.add_argument(
        "--repository-root",
        type=Path,
        default=Path(__file__).resolve().parents[3],
        help="checkout root the reported file paths are rendered relative to",
    )
    parser.add_argument(
        "--top-uncovered",
        type=int,
        default=30,
        help="how many of the least-covered files to list (0 omits the section)",
    )
    parser.add_argument(
        "--title",
        default="Native client coverage (report only)",
        help="heading the report opens with",
    )
    parser.add_argument(
        "--baseline",
        type=Path,
        default=None,
        help="earlier xccov JSON report to report the headline total against",
    )
    parser.add_argument(
        "--baseline-label",
        choices=BASELINE_LABELS,
        default=None,
        help="which baseline --baseline is; required with it, because the two differ",
    )
    parser.add_argument(
        "--baseline-sha",
        default="",
        help="commit the baseline was measured at",
    )
    parser.add_argument(
        "--baseline-date",
        default="",
        help="when the baseline was measured, which is how stale a reader judges it",
    )
    parser.add_argument(
        "--baseline-unavailable",
        default="",
        help="why there is no baseline, stated in the report in place of the delta",
    )
    parser.add_argument(
        "--measurement-incomplete",
        action="store_true",
        help=(
            "the test run that produced this report did not finish, so the delta "
            "must not be attributed to the branch"
        ),
    )
    arguments = parser.parse_args(argv)
    # A mislabelled delta is worse than no delta, so the label is not defaulted
    # to either value. This is a caller contract, not a runtime condition: the
    # workflow builds both flags in one place.
    if arguments.baseline is not None and arguments.baseline_label is None:
        parser.error("--baseline requires --baseline-label")

    document = json.loads(arguments.json.read_text(encoding="utf-8"))
    baseline: Baseline | None = None
    unavailable = arguments.baseline_unavailable
    if arguments.baseline is not None:
        baseline, reason = load_baseline(
            arguments.baseline,
            arguments.baseline_label,
            arguments.baseline_sha,
            arguments.baseline_date,
        )
        # A baseline that was fetched and then turned out to be unreadable
        # reports the reading failure, not whatever the fetcher had to say.
        if baseline is None:
            unavailable = reason
    sys.stdout.write(
        render(
            document,
            arguments.repository_root.resolve(),
            max(arguments.top_uncovered, 0),
            arguments.title,
            baseline=baseline,
            baseline_unavailable=unavailable,
            measurement_incomplete=arguments.measurement_incomplete,
        )
    )
    return 0


if __name__ == "__main__":
    raise SystemExit(main(sys.argv[1:]))
