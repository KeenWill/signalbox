#!/usr/bin/env python3
"""Render one llvm-cov JSON export as a per-crate Markdown coverage report.

`cargo llvm-cov` reports either a whole-workspace total or a per-file table.
Neither answers the question this report exists to answer — which crate still
has untested code — so this tool aggregates the per-file export into per-crate
rows and orders them least-covered first.

The report measures; it decides nothing. There is no threshold here, no exit
code that depends on a percentage, and no caller that gates on one: every
invocation that produces a well-formed report exits zero.

Input is the `llvm.coverage.json.export` document `cargo llvm-cov report
--json` writes. Only the file summaries are read, so the much larger per-region
detail in the same document is ignored.

A second such document can be supplied as `--baseline`, in which case the
totals table also carries the signed difference against it. That difference is
informational in exactly the sense the rest of this tool is: it is rendered, it
is labelled with which baseline produced it, and nothing reads it back. An
unreadable or missing baseline is reported in the body of the report and
changes no exit code.
"""

from __future__ import annotations

import argparse
import json
import os
import sys
from dataclasses import dataclass
from pathlib import Path

# The export nests one summary block per file under a single data element.
# llvm-cov emits one element per coverage mapping; cargo-llvm-cov merges
# everything it instrumented into one, and additional elements are summed
# rather than assumed absent.
EXPORT_TYPE = "llvm.coverage.json.export"

OUTSIDE_WORKSPACE = "(outside the workspace)"

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
class Counter:
    """One llvm-cov counter: how many of a thing exist and how many were hit."""

    count: int = 0
    covered: int = 0

    def add(self, other: "Counter") -> None:
        self.count += other.count
        self.covered += other.covered

    @property
    def percent(self) -> float:
        """Covered share, with an empty counter reported as fully covered.

        llvm-cov itself reports 100% for an empty counter. A crate with no
        instrumented lines at all is a degenerate row either way; matching
        llvm-cov keeps this table comparable with `cargo llvm-cov` output.
        """
        if self.count == 0:
            return 100.0
        return 100.0 * self.covered / self.count


@dataclass
class Summary:
    """The three counters this report renders, for one file or one crate."""

    lines: Counter
    functions: Counter
    regions: Counter

    @staticmethod
    def empty() -> "Summary":
        return Summary(lines=Counter(), functions=Counter(), regions=Counter())

    def add(self, other: "Summary") -> None:
        self.lines.add(other.lines)
        self.functions.add(other.functions)
        self.regions.add(other.regions)


@dataclass
class Baseline:
    """One earlier measurement the current one is reported against.

    Carries the provenance beside the numbers on purpose: a delta whose
    baseline is unnamed cannot be read, and the two labels this accepts mean
    materially different things about what the delta is attributable to.
    """

    total: Summary
    label: str
    sha: str
    date: str


def read_counter(summary: dict, name: str) -> Counter:
    """Read one named counter, tolerating counters a toolchain omits.

    llvm-cov has grown counters over releases (branches, then MCDC). Reading
    only the three this report renders, and defaulting a missing one to empty,
    keeps the tool working across the toolchain upgrades the repository takes.
    """
    block = summary.get(name)
    if not isinstance(block, dict):
        return Counter()
    return Counter(count=int(block.get("count", 0)), covered=int(block.get("covered", 0)))


def read_file_summaries(document: dict) -> list[tuple[str, Summary]]:
    """Extract (filename, summary) for every file the export names."""
    if document.get("type") != EXPORT_TYPE:
        raise ValueError(f"not an {EXPORT_TYPE} document: type={document.get('type')!r}")

    extracted: list[tuple[str, Summary]] = []
    for element in document.get("data", []):
        for entry in element.get("files", []):
            summary = entry.get("summary", {})
            extracted.append(
                (
                    str(entry["filename"]),
                    Summary(
                        lines=read_counter(summary, "lines"),
                        functions=read_counter(summary, "functions"),
                        regions=read_counter(summary, "regions"),
                    ),
                )
            )
    return extracted


def crate_of(filename: str, repo_root: Path) -> str:
    """Name the crate a source file belongs to, by workspace directory layout.

    The workspace places every member under `crates/<name>` or `apps/<name>`,
    so the first two path components identify the member without parsing any
    manifest. A path outside the checkout is bucketed rather than dropped: it
    would otherwise vanish from a table whose total is the sum of its rows.
    """
    try:
        relative = Path(os.path.relpath(filename, repo_root))
    except ValueError:
        return OUTSIDE_WORKSPACE
    parts = relative.parts
    if not parts or parts[0] == os.pardir:
        return OUTSIDE_WORKSPACE
    if parts[0] in ("crates", "apps") and len(parts) >= 2:
        return f"{parts[0]}/{parts[1]}"
    return parts[0]


def relative_name(filename: str, repo_root: Path) -> str:
    """Render a path relative to the checkout, or verbatim when it is outside."""
    try:
        relative = Path(os.path.relpath(filename, repo_root))
    except ValueError:
        return filename
    if relative.parts and relative.parts[0] == os.pardir:
        return filename
    return str(relative)


def aggregate_by_crate(files: list[tuple[str, Summary]], repo_root: Path) -> dict[str, Summary]:
    """Sum per-file summaries into one summary per crate."""
    crates: dict[str, Summary] = {}
    for filename, summary in files:
        crate = crate_of(filename, repo_root)
        crates.setdefault(crate, Summary.empty()).add(summary)
    return crates


def total_of(summaries: list[Summary]) -> Summary:
    """Sum summaries so the reported total is exactly the sum of the rows."""
    total = Summary.empty()
    for summary in summaries:
        total.add(summary)
    return total


def percent(counter: Counter) -> str:
    return f"{counter.percent:.2f}%"


def delta(current: Counter, baseline: Counter) -> str:
    """Render one signed difference between two coverage percentages.

    The unit is percentage points, not percent: the difference between two
    percentages is a percentage-point difference, and writing it with a `%`
    would overstate or understate it by the ratio of the two denominators.
    The sign is always shown, including on an exact zero, so a reader can tell
    "measured, unchanged" from "not measured".
    """
    return f"{current.percent - baseline.percent:+.2f} pp"


def load_baseline(path: Path, label: str, sha: str, date: str) -> tuple["Baseline | None", str]:
    """Read a baseline export, or say why it cannot be read.

    Every failure here returns a reason rather than raising: a baseline is an
    optional convenience on a report-only measurement, and an unreadable one
    must degrade the report to its absolute numbers instead of costing the
    report. The reasons are distinguished because "the artifact was never
    written" and "the artifact is not the document it claims to be" call for
    different fixes.
    """
    try:
        document = json.loads(path.read_text(encoding="utf-8"))
        total = total_of([summary for _, summary in read_file_summaries(document)])
    except (OSError, ValueError, KeyError, TypeError, AttributeError) as error:
        return None, f"the baseline summary could not be read ({error.__class__.__name__})"
    # An export with no instrumented line is not a measurement to compare
    # against; `Counter.percent` would report it as 100% covered and every
    # delta below it as a large regression.
    if total.lines.count == 0:
        return None, "the baseline summary measured no lines"
    return Baseline(total=total, label=label, sha=sha, date=date), ""


def baseline_note(baseline: Baseline) -> list[str]:
    """State which baseline the deltas are against, and what they include.

    Hard-wrapped like the rest of this report's prose, with the two
    interpolated values on a line of their own so a long SHA or timestamp
    cannot leave one line three times the width of its neighbours.
    """
    measured = f"`{baseline.sha}`, measured {baseline.date}."
    if baseline.label == BASE:
        provenance = [
            "Compared against the base this pull request is open against:",
            measured,
            "This run measured that base with this branch merged into it, so",
            "the deltas below are this branch's own doing — as far as the two",
            "runs measured the same suites, which the note below qualifies.",
        ]
    else:
        provenance = [
            "Compared against the most recent `main` push this workflow measured:",
            measured,
            "That is **not** the base this pull request is open against, so the",
            "deltas below carry every difference between that commit and the",
            "actual base — what landed on `main` in between, and, when this",
            "pull request is stacked on another, that parent's unmerged",
            "changes as well. A stacked parent can dominate the numbers.",
        ]
    return provenance + [
        "",
        "They are percentage points, over a file set that differs from the",
        "baseline's wherever a file was added or removed. Each measured suite",
        "continues on error, so a run that lost a suite still counts as",
        "successful and still uploads an artifact — one covering less of the",
        "workspace. That applies to the base run as much as to any other, so a",
        "delta of that size is the missing suite and not a code change. The",
        "outcome table above is this run's alone: it cannot tell you whether",
        "the baseline run was whole, and no attribution here should be read as",
        "claiming it was.",
    ]


def baseline_unavailable_note(reason: str) -> list[str]:
    """Say that there is no baseline, and why, rather than showing nothing.

    A silently absent delta reads as "no change" to anyone who saw one on the
    previous pull request, so the absence is stated with its cause.
    """
    return [
        f"No baseline to compare against: {reason}.",
        "The figures below are absolute; no delta is shown.",
    ]


def crate_rows(crates: dict[str, Summary]) -> list[str]:
    """Render one table row per crate, least-covered lines first.

    Ties break on the crate name so the table is stable between runs over the
    same tree, which keeps two reports diffable.
    """
    ordered = sorted(crates.items(), key=lambda item: (item[1].lines.percent, item[0]))
    return [
        "| `{crate}` | {lines} | {covered}/{count} | {functions} | {regions} |".format(
            crate=crate,
            lines=percent(summary.lines),
            covered=summary.lines.covered,
            count=summary.lines.count,
            functions=percent(summary.functions),
            regions=percent(summary.regions),
        )
        for crate, summary in ordered
    ]


def least_covered_files(files: list[tuple[str, Summary]], repo_root: Path, limit: int) -> list[str]:
    """Render the least-covered files, which is what this report is for.

    Files with no instrumented lines carry no signal and are dropped; fully
    covered files are dropped because they are not the question. Ordering is
    by uncovered line count rather than percentage, so a large mostly-untested
    file outranks a three-line file at 0%.
    """
    candidates = [
        (filename, summary)
        for filename, summary in files
        if summary.lines.count > 0 and summary.lines.covered < summary.lines.count
    ]
    ordered = sorted(
        candidates,
        key=lambda item: (-(item[1].lines.count - item[1].lines.covered), item[0]),
    )
    return [
        "| `{name}` | {uncovered} | {lines} |".format(
            name=relative_name(filename, repo_root),
            uncovered=summary.lines.count - summary.lines.covered,
            lines=percent(summary.lines),
        )
        for filename, summary in ordered[:limit]
    ]


def measure_row(name: str, current: Counter, baseline: "Counter | None") -> str:
    """Render one totals row, with its delta column only when there is one.

    A baseline counter of zero gets no delta. `read_counter` defaults a counter
    the baseline's toolchain never emitted to an empty one, and `percent`
    reports an empty counter as 100% — so subtracting it would render a current
    50% as a fabricated `-50.00 pp` regression against a measure the baseline
    never made. A baseline that genuinely counted nothing is the same case:
    there is no share to compare against either way.
    """
    cells = [name, str(current.covered), str(current.count), percent(current)]
    if baseline is not None:
        cells.append(delta(current, baseline) if baseline.count else "not in baseline")
    return "| " + " | ".join(cells) + " |"


def render(
    document: dict,
    repo_root: Path,
    limit: int,
    title: str,
    preamble: str = "",
    *,
    baseline: "Baseline | None" = None,
    baseline_unavailable: str = "",
) -> str:
    """Render the whole report, including its own honesty about what it omits.

    The caller supplies the preamble because what a number excludes is a
    property of the run that produced it — which suites executed, which were
    skipped — and only the caller knows that. It lands directly under the
    heading, ahead of the first figure, so no reader meets a percentage before
    meeting its denominator.

    The baseline is optional in both directions, and its two absent cases are
    not the same. A caller that never asked for one — a `main` push measuring
    the tree that becomes the next baseline — passes neither argument and gets
    exactly the report it got before. A caller that asked and could not get one
    passes `baseline_unavailable` and gets that stated in place of the deltas.
    Nothing here gates on a delta; the column is a column.
    """
    files = read_file_summaries(document)
    crates = aggregate_by_crate(files, repo_root)
    total = total_of(list(crates.values()))
    baseline_total = None if baseline is None else baseline.total

    lines = [
        f"## {title}",
        "",
        "Report only. This measurement has no threshold, gates no merge, and",
        "fails no check; it exists so untested code stays visible.",
    ]
    # An absent preamble leaves no blank-line gap of its own; a present one is
    # separated from the paragraph above and the table below.
    if preamble.strip():
        lines.extend(["", *preamble.strip().splitlines()])
    if baseline is not None:
        lines.extend(["", *baseline_note(baseline)])
    elif baseline_unavailable:
        lines.extend(["", *baseline_unavailable_note(baseline_unavailable)])
    header = ["Measure", "Covered", "Total", "Percent"]
    alignment = ["---", "---:", "---:", "---:"]
    if baseline_total is not None:
        header.append("Δ vs baseline")
        alignment.append("---:")
    lines.extend(
        [
            "",
            "| " + " | ".join(header) + " |",
            "| " + " | ".join(alignment) + " |",
            measure_row("Lines", total.lines, None if baseline_total is None else baseline_total.lines),
            measure_row(
                "Functions",
                total.functions,
                None if baseline_total is None else baseline_total.functions,
            ),
            measure_row(
                "Regions",
                total.regions,
                None if baseline_total is None else baseline_total.regions,
            ),
            "",
            "### Per crate, least-covered first",
            "",
            "| Crate | Line % | Lines | Function % | Region % |",
            "| --- | ---: | ---: | ---: | ---: |",
        ]
    )
    lines.extend(crate_rows(crates))

    uncovered = least_covered_files(files, repo_root, limit)
    if uncovered:
        lines.extend(
            [
                "",
                f"### {len(uncovered)} files with the most uncovered lines",
                "",
                "| File | Uncovered lines | Line % |",
                "| --- | ---: | ---: |",
            ]
        )
        lines.extend(uncovered)

    return "\n".join(lines) + "\n"


def main(argv: list[str]) -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("json", type=Path, help="llvm-cov JSON export to summarize")
    parser.add_argument(
        "--repo-root",
        type=Path,
        default=Path(__file__).resolve().parent.parent,
        help="checkout root the exported filenames are reported relative to",
    )
    parser.add_argument(
        "--top-uncovered",
        type=int,
        default=15,
        help="how many of the least-covered files to list (0 omits the section)",
    )
    parser.add_argument(
        "--title",
        default="Rust coverage (report only)",
        help="heading the report opens with",
    )
    parser.add_argument(
        "--preamble",
        type=Path,
        default=None,
        help="Markdown file stating what this run measured, placed before the figures",
    )
    parser.add_argument(
        "--baseline",
        type=Path,
        default=None,
        help="earlier llvm-cov JSON export to report the totals against",
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
        help="why there is no baseline, stated in the report in place of the deltas",
    )
    arguments = parser.parse_args(argv)
    # A mislabelled delta is worse than no delta, so the label is not defaulted
    # to either value. This is a caller contract, not a runtime condition: the
    # workflows below build both flags in one place.
    if arguments.baseline is not None and arguments.baseline_label is None:
        parser.error("--baseline requires --baseline-label")

    document = json.loads(arguments.json.read_text(encoding="utf-8"))
    preamble = "" if arguments.preamble is None else arguments.preamble.read_text(encoding="utf-8")
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
            arguments.repo_root.resolve(),
            max(arguments.top_uncovered, 0),
            arguments.title,
            preamble,
            baseline=baseline,
            baseline_unavailable=unavailable,
        )
    )
    return 0


if __name__ == "__main__":
    raise SystemExit(main(sys.argv[1:]))
