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


def render(document: dict, repo_root: Path, limit: int, title: str, preamble: str = "") -> str:
    """Render the whole report, including its own honesty about what it omits.

    The caller supplies the preamble because what a number excludes is a
    property of the run that produced it — which suites executed, which were
    skipped — and only the caller knows that. It lands directly under the
    heading, ahead of the first figure, so no reader meets a percentage before
    meeting its denominator.
    """
    files = read_file_summaries(document)
    crates = aggregate_by_crate(files, repo_root)
    total = total_of(list(crates.values()))

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
    lines.extend(
        [
            "",
            "| Measure | Covered | Total | Percent |",
            "| --- | ---: | ---: | ---: |",
            f"| Lines | {total.lines.covered} | {total.lines.count} | {percent(total.lines)} |",
            f"| Functions | {total.functions.covered} | {total.functions.count} | {percent(total.functions)} |",
            f"| Regions | {total.regions.covered} | {total.regions.count} | {percent(total.regions)} |",
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
    arguments = parser.parse_args(argv)

    document = json.loads(arguments.json.read_text(encoding="utf-8"))
    preamble = "" if arguments.preamble is None else arguments.preamble.read_text(encoding="utf-8")
    sys.stdout.write(
        render(
            document,
            arguments.repo_root.resolve(),
            max(arguments.top_uncovered, 0),
            arguments.title,
            preamble,
        )
    )
    return 0


if __name__ == "__main__":
    raise SystemExit(main(sys.argv[1:]))
