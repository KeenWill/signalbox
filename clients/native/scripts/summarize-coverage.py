#!/usr/bin/env python3
"""Render one `xcrun xccov view --report --json` document as Markdown.

Xcode measures every target the scheme builds, the `.xctest` bundles included,
and its own total counts them. A number that counts test bundles largely
measures how thoroughly the tests execute themselves, so this splits the
product targets from the test bundles and totals only the product targets.

The report measures; it decides nothing. There is no threshold here and no
exit code that depends on a percentage.
"""

from __future__ import annotations

import argparse
import json
import sys
from pathlib import Path

# Xcode names a unit- or UI-test bundle target with this extension, and a
# product target with .app, .framework, or .xctest-free names. The split is by
# extension because that is what the report document carries; nothing else in
# it distinguishes product code from test code.
TEST_BUNDLE_SUFFIX = ".xctest"


def is_test_bundle(target: dict) -> bool:
    return str(target.get("name", "")).endswith(TEST_BUNDLE_SUFFIX)


def percent(covered: int, executable: int) -> str:
    """Covered share, with an empty target reported as fully covered."""
    if executable == 0:
        return "100.00%"
    return f"{100.0 * covered / executable:.2f}%"


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


def render(document: dict, repository_root: Path, limit: int, title: str) -> str:
    products = [target for target in document.get("targets", []) if not is_test_bundle(target)]
    bundles = [target for target in document.get("targets", []) if is_test_bundle(target)]
    covered = sum(target["coveredLines"] for target in products)
    executable = sum(target["executableLines"] for target in products)

    lines = [
        f"## {title}",
        "",
        "Report only. This measurement has no threshold, gates no merge, and",
        "fails no check; it exists so untested code stays visible.",
        "",
        f"**{percent(covered, executable)}** of product lines covered ({covered}/{executable}).",
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
    arguments = parser.parse_args(argv)

    document = json.loads(arguments.json.read_text(encoding="utf-8"))
    sys.stdout.write(
        render(
            document,
            arguments.repository_root.resolve(),
            max(arguments.top_uncovered, 0),
            arguments.title,
        )
    )
    return 0


if __name__ == "__main__":
    raise SystemExit(main(sys.argv[1:]))
