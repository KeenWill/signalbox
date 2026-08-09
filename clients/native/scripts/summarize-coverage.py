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
from math import isfinite
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


def percent_value(*, covered: int, executable: int) -> float:
    """Covered share, with an empty target reported as fully covered.

    Keyword-only, along with `percent` and `delta` below. The two counters
    are adjacent integers, so a transposed call is valid Python that renders
    the reciprocal share and a delta to match — a wrong number presented
    with the same confidence as a right one. The Rust summarizer is spared
    this by carrying a `Counter`; here the labels do the same work at the
    call site (docs/style.md principle 2).
    """
    if executable == 0:
        return 100.0
    return 100.0 * covered / executable


def percent(*, covered: int, executable: int) -> str:
    return f"{percent_value(covered=covered, executable=executable):.2f}%"


def delta(*, covered: int, executable: int, baseline: Baseline) -> str:
    """Render the signed difference between this run and its baseline.

    The unit is percentage points, not percent: the difference between two
    percentages is a percentage-point difference, and writing it with a `%`
    would overstate or understate it by the ratio of the two denominators.
    The sign is always shown, including on an exact zero, so a reader can tell
    "measured, unchanged" from "not measured".
    """
    difference = percent_value(covered=covered, executable=executable) - percent_value(
        covered=baseline.covered, executable=baseline.executable
    )
    return f"{difference:+.2f} pp"


def product_totals(document: dict) -> tuple[int, int]:
    """Sum covered and executable lines over the product targets alone."""
    products = [target for target in document.get("targets", []) if not is_test_bundle(target)]
    return (
        sum(int(target["coveredLines"]) for target in products),
        sum(int(target["executableLines"]) for target in products),
    )


def impossible_product_targets(document: dict) -> list[str]:
    """Name the product targets whose counters cannot describe a measurement.

    Per target rather than against the sum, because invalid targets cancel: one
    at `(covered 2, executable 1)` and another at `(0, 1)` total to `(2, 2)`,
    which passes every aggregate test while describing nothing real. Checking
    each target also makes the aggregate test redundant, since targets that are
    individually sane cannot sum to an insane total.
    """
    impossible = []
    for target in document.get("targets", []):
        if is_test_bundle(target):
            continue
        covered = int(target["coveredLines"])
        executable = int(target["executableLines"])
        if covered < 0 or executable < 0 or covered > executable:
            impossible.append(str(target.get("name", "<unnamed>")))
    return impossible


def load_baseline(path: Path, *, label: str, sha: str, date: str) -> tuple["Baseline | None", str]:
    """Read a baseline report, or say why it cannot be read.

    Every failure here returns a reason rather than raising: a baseline is an
    optional convenience on a report-only measurement, and an unreadable one
    must degrade the report to its absolute numbers instead of costing the
    report. The reasons are distinguished because "the artifact was never
    written" and "the artifact is not the document it claims to be" call for
    different fixes.

    The provenance is keyword-only. `label`, `sha` and `date` are three
    consecutive strings, so a positional run lets a transposition read as a
    valid call and print a timestamp where the measured commit belongs — a
    report that is wrong about what it compared, with nothing to catch it
    (docs/style.md principle 2, "position may carry meaning only where types
    do"). `path` keeps its position, being the one argument whose type states
    its role.

    `OverflowError` joins the caught set for the same reason the others are
    there. A counter of `1e999` is a valid JSON number that `json.loads` hands
    back as infinity, and `int(inf)` raises it; an unbounded digit string
    raises it one step later, converting the percentage to a float. Both
    escaped this guard and ended the report step, which is precisely the
    outcome the "return a reason" contract above exists to prevent — an
    unreadable baseline must cost the delta, never the report. The workflow's
    `jq` predicate rejects such counters before they reach here; this is the
    second line, because the loader is what promises not to raise.
    """
    try:
        document = json.loads(path.read_text(encoding="utf-8"))
        covered, executable = product_totals(document)
        # Rendering is what these totals are *for*, so a total that cannot be
        # rendered is one this cannot read. `int()` accepts a 400-digit decimal
        # string, so the loader used to return a `Baseline` that raised
        # `OverflowError` later, inside `delta` — outside every guard, ending
        # the report with a traceback instead of the unavailable-baseline note
        # this function promises. Forcing the percentages here moves that
        # failure inside the protected path where it belongs.
        #
        # `isfinite` covers the other half. A counter large enough to overflow
        # `int()` raises above; one that merely loses precision, or arrives as
        # a float infinity, yields a non-finite percentage instead of raising,
        # and a report of `inf%` is no more actionable than a traceback. The
        # workflow's `jq` predicate rejects both before they reach here; this is
        # the loader keeping its own promise for direct CLI use, which the
        # documented contract permits.
        if not isfinite(percent_value(covered=covered, executable=executable)):
            return None, "the baseline report has a counter that cannot be rendered"
        # More covered than executable, or a negative count, is a corrupt
        # measurement rather than a low one: it renders a complete current run
        # as a fabricated regression instead of degrading to the note below.
        # Same guard, same reason, and now the same granularity as the Rust
        # loader — per target, because invalid targets cancel in the sum.
        impossible = impossible_product_targets(document)
        if impossible:
            return None, (
                "the baseline report reports impossible counters for "
                f"{', '.join(impossible)}"
            )
    except (OSError, ValueError, KeyError, TypeError, AttributeError, OverflowError) as error:
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
        # Which baseline this is still has to be said. The two complete-run
        # branches below distinguish the exact base from a `latest-main`
        # fallback because the distinction changes what the delta means, and an
        # unfinished run does not make that distinction matter less — it stacks
        # a second reason the number is not this branch's doing on top of the
        # first. Rendering identical provenance for both left a reader unable
        # to tell which comparison they were looking at.
        provenance = [
            "**This run's tests did not finish**, so the measurement above",
            "covers only the bundles that ran. The delta is therefore not",
            "attributable to this branch: comparing a partial measurement",
            "against a complete baseline shows the tests that were lost, at",
            "whatever size they happen to be. The baseline it is measured",
            "against is",
            measured,
            *(
                [
                    "That is the base this pull request is open against, so",
                    "the unfinished tests are the only reason the delta is",
                    "unattributable.",
                ]
                if baseline.label == BASE
                else [
                    "That is also **not** the base this pull request is open",
                    "against, so on top of the tests that never ran the delta",
                    "carries what landed on `main` in between, and any unmerged",
                    "changes of a stacked parent.",
                ]
            ),
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
            share=percent(
                covered=target["coveredLines"],
                executable=target["executableLines"],
            ),
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
            share=percent(
                covered=source["coveredLines"],
                executable=source["executableLines"],
            ),
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

    share = percent(covered=covered, executable=executable)
    headline = f"**{share}** of product lines covered ({covered}/{executable})"
    if baseline is not None:
        signed = delta(covered=covered, executable=executable, baseline=baseline)
        headline += f", **{signed}** against the baseline below"
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
    # Provenance is not decoration: the label says *which* baseline this is and
    # the SHA and date say which measurement it was, which is the whole of what
    # makes a delta checkable by the reader it is shown to. Defaulting them to
    # empty strings let a direct call render "``, measured ." beside a
    # confidently attributed delta — a report that looks authoritative and
    # states nothing. The workflows always pass all three; this is the CLI
    # keeping the same contract for anyone invoking it by hand.
    absent = [
        flag
        for flag, value in (
            ("--baseline-sha", arguments.baseline_sha),
            ("--baseline-date", arguments.baseline_date),
        )
        if not value.strip()
    ]
    if arguments.baseline is not None and absent:
        parser.error(f"--baseline requires {' and '.join(absent)}")

    document = json.loads(arguments.json.read_text(encoding="utf-8"))
    baseline: Baseline | None = None
    unavailable = arguments.baseline_unavailable
    if arguments.baseline is not None:
        baseline, reason = load_baseline(
            arguments.baseline,
            label=arguments.baseline_label,
            sha=arguments.baseline_sha,
            date=arguments.baseline_date,
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
