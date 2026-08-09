#!/usr/bin/env python3
"""Check that a specification paragraph's implementation tier is its location.

`AGENTS.md` requires every paragraph of a specification page to be exactly one
of implemented, committed-unimplemented, or deferred, and makes unlabelled
prose mean implemented. That rule had no structural support: on an enrolled
page before this checker existed, every tier label was a **bold phrase in
running prose**, so a paragraph's tier was a decoration a reader had to look
for rather than a place it sat. The 2,055-line credential page carried one `#`
heading, fifteen `##` headings, and no `###` headings at all, with eight tier
labels spelled as bold run-in text.

Three consequences followed, all observed rather than predicted. A paragraph
could drift tier silently, because nothing moved when its label stopped being
true — twenty-seven review findings on one pull request were exactly that.
Nothing was narrowly addressable, so a cross-page link had to target a `##`
anchor covering as much as 492 lines, and a carve that moved the paragraph an
anchor meant left the anchor resolving and the meaning gone. And nothing was
lintable, because "the committed-unimplemented sections" were not sections.

This checker makes the tier a location. A tier label must be a `###` heading,
tier sections must ascend inside their `##` section, and a
committed-unimplemented section must claim that no present surface provides
what it describes. A paragraph cannot then be mislabelled, because the label is
where it sits: changing its tier means moving it, which is visible in a diff.

The check fails when, on an enrolled page,

1. a tier label opens a paragraph in bold rather than heading a section, so the
   section it names is not a section,
2. a heading declares a tier at any depth other than `###`, so tier sections
   are not one uniform, narrowly addressable depth and none is classified by
   what it happens to be nested under,
3. a `##` section's subsections descend in tier, so a reader cannot take
   position for tier,
4. a committed-unimplemented section never claims that no present surface
   provides it, leaving the label as decoration,
5. an implemented section carries future-tense ownership markers, which is the
   drift this contract exists to prevent, or
6. a table cell inside a non-implemented section classifies its row as
   implemented, giving one row two tiers.

Rule 3 is the one that does the work. Rules 1, 2 and 6 only make tiers
unambiguous; rule 3 is what makes position mean something, because a page whose
tiers are headings in arbitrary order still requires reading every heading to
learn a paragraph's tier.

Rules 4 and 5 both turn on a sentence-level absence claim rather than on the
words appearing anywhere. "No present composition parses a pool" is a claim
about this build; "a historical call with no present usage axis" is ordinary
description that asserts nothing about any build. Matching both spellings made
rule 5 cry wolf and rule 4 accept a section that never made its claim — the
same fragment failing in opposite directions, which is why both now require the
phrase to open a sentence.

Why this parses Markdown with `markdown-it-py` rather than with regular
expressions: the sibling projection checker learned that lesson first, over
five findings that were all one defect, and this checker was deliberately left
hand-rolled on the evidence that none of its own findings had been about
parsing. That held for exactly one round. When a fenced-block finding arrived,
inspecting the surface rather than waiting for it to be reported turned up
three more latent instances of the same grammar: an escaped pipe inside a table
cell read as a cell boundary — the identical defect already fixed in the
sibling — a tier label written as a setext heading was invisible, and a heading
inside a block quote counted as a section heading. One report is a weak signal;
four instances in a surface of fences, headings, lists and tables is a grammar.
Shared reading lives in `markdown_reader.py` so a fix cannot land in one
checker and miss the other.

Enrollment is explicit and per-page, following the promotion discipline the
style-rule checker already uses in `.github/workflows/rust.yml`: a rule gates
where the tree satisfies it, and extending it to another page is a deliberate
change that restructures that page, never a side effect of editing this file.
Ten specification pages still carry bold-prose tier labels; enrolling one means
giving it headings first. A checker that failed on all of them at once would be
reverted rather than satisfied.

Run from the repository root with the pinned Markdown toolchain on the path;
exits nonzero with a per-failure report naming every page and line involved.
"""

from __future__ import annotations

import re
import sys
from pathlib import Path

from markdown_reader import (
    heading_level,
    line_of,
    parser,
    rendered,
    require_toolchain,
    table_cells,
)

CHECK_NAME = "spec-tier check"

# Pages whose tiers this checker gates. A page belongs here once its tier
# labels are `###` headings in ascending order; adding one is a restructuring
# change to that page, not an edit to this list alone.
ENROLLED_PAGES = (
    "docs/spec/configuration-and-credentials.md",
    "docs/spec/credential-availability.md",
)

TIER_IMPLEMENTED = 0
TIER_COMMITTED = 1
TIER_DEFERRED = 2

TIER_NAMES = {
    TIER_IMPLEMENTED: "implemented",
    TIER_COMMITTED: "committed unimplemented",
    TIER_DEFERRED: "deferred",
}

# Text that declares a tier, matched against rendered heading or lead-in text.
COMMITTED_LABEL = re.compile(r"^committed unimplemented\b", re.IGNORECASE)
DEFERRED_LABEL = re.compile(r"^deferred\b", re.IGNORECASE)

# The depth a tier label binds at, and only this depth.
TIER_HEADING_DEPTH = 3

# A claim that no present surface provides something, counted only where it
# opens a sentence. Mid-sentence the same words are description.
ABSENCE_CLAIM = re.compile(
    r"(?:^|(?<=[.:;!?]\s))(?:no|nothing) (?:present|current|composed)\b",
    re.IGNORECASE | re.MULTILINE,
)

# Ownership markers that assign work to a future change, for implemented
# sections only.
FUTURE_OWNERSHIP_MARKERS = (
    ABSENCE_CLAIM,
    re.compile(r"\bimplementing child\b", re.IGNORECASE),
    re.compile(r"\bcommitted unimplemented\b", re.IGNORECASE),
    re.compile(r"\bcommitted future\b", re.IGNORECASE),
)

# Sections whose whole purpose is to point at what is not implemented. Rule 5
# cannot apply to them: `docs/spec/README.md` requires each page to surface its
# deferred and undecided items as pointers in exactly this section.
TIER_FREE_SECTIONS = frozenset({"Open edges"})

# A cell that opens by classifying its row as implemented. `\bimplemented`
# does not match "unimplemented", so an ordinary
# "Committed unimplemented — ..." tier cell is not a hit.
IMPLEMENTED_CELL = re.compile(r"^\s*implemented\b", re.IGNORECASE)


def tier_of(text: str) -> int:
    """Read a tier from rendered text, defaulting to implemented."""
    stripped = text.strip()
    if COMMITTED_LABEL.match(stripped):
        return TIER_COMMITTED
    if DEFERRED_LABEL.match(stripped):
        return TIER_DEFERRED
    return TIER_IMPLEMENTED


class Section:
    """One tier-bearing part: a `##` section's opening prose, or a `###`."""

    def __init__(self, level: int, title: str, tier: int, line: int) -> None:
        self.level = level
        self.title = title
        self.tier = tier
        self.line = line
        self.body: list[str] = []
        self.cells: list[str] = []

    def text(self) -> str:
        return "\n".join(self.body)


def read_page(tokens: list, failures: list[str], path: Path) -> list[list[Section]]:
    """Group a page into `##` sections of tier-bearing parts.

    Only top-level headings open a section: one inside a block quote is quoted
    material, not a section of this page. Fenced and indented code never reach
    any rule, so an example may spell whatever it needs to.
    """
    groups: list[list[Section]] = []
    current: list[Section] | None = None
    part: Section | None = None

    index = 0
    while index < len(tokens):
        token = tokens[index]
        if token.level != 0:
            index += 1
            continue

        if token.type in ("fence", "code_block"):
            index += 1
            continue

        if token.type == "heading_open":
            level = heading_level(token)
            title = rendered(tokens[index + 1].children)
            tier = tier_of(title)
            if tier != TIER_IMPLEMENTED and level != TIER_HEADING_DEPTH:
                failures.append(
                    f"{path}:{line_of(token)}: heading {title!r} declares a tier "
                    f"at depth {level}. Tier sections are `###` and only `###`, so "
                    f"that every one of them is the same, narrowly addressable "
                    f"depth and none is classified by what it is nested under"
                )
            if level <= 2:
                part = Section(level, title, tier, line_of(token))
                current = [part]
                groups.append(current)
            elif level == TIER_HEADING_DEPTH and current is not None:
                part = Section(level, title, tier, line_of(token))
                current.append(part)
            index += 3
            continue

        if token.type == "table_open":
            rows, end = table_cells(tokens, index)
            if part is not None:
                for row in rows:
                    part.cells.extend(row)
            index = end + 1
            continue

        if token.type == "paragraph_open":
            inline = tokens[index + 1]
            text = rendered(inline.children)
            # markdown-it emits a leading empty text token before an opening
            # delimiter, so the lead is the first child carrying anything.
            children = [
                child
                for child in (inline.children or [])
                if child.type != "text" or child.content
            ]
            leads_bold = bool(children) and children[0].type == "strong_open"
            if leads_bold and tier_of(text) != TIER_IMPLEMENTED:
                failures.append(
                    f"{path}:{line_of(token)}: tier label written as bold prose. A "
                    f"tier is a location on this page: make this a `###` heading "
                    f"so the section it names is a section, and so a paragraph "
                    f"cannot leave its tier behind without moving"
                )
            if part is not None:
                part.body.append(text)
            index += 3
            continue

        end = index
        if token.nesting == 1:
            depth = 0
            while end < len(tokens):
                depth += tokens[end].nesting
                end += 1
                if depth == 0:
                    break
        else:
            end = index + 1
        if part is not None:
            for inner in tokens[index:end]:
                if inner.type == "inline":
                    part.body.append(rendered(inner.children))
        index = end

    return groups


def check_page(path: Path, failures: list[str]) -> None:
    try:
        text = path.read_text(encoding="utf-8")
    except OSError as error:
        failures.append(f"{path}: unreadable enrolled page ({error})")
        return

    for group in read_page(parser().parse(text), failures, path):
        opening = group[0]
        highest = TIER_IMPLEMENTED
        highest_title = opening.title

        for part in group:
            if part.tier < highest:
                failures.append(
                    f"{path}:{part.line}: {TIER_NAMES[part.tier]} section "
                    f"{part.title!r} follows {TIER_NAMES[highest]} section "
                    f"{highest_title!r} inside `## {opening.title}`. Tier sections "
                    f"ascend, so that a paragraph's position gives its tier "
                    f"without reading back"
                )
            else:
                highest = part.tier
                highest_title = part.title

            body = part.text()

            if part.tier == TIER_COMMITTED and ABSENCE_CLAIM.search(body) is None:
                failures.append(
                    f"{path}:{part.line}: committed-unimplemented section "
                    f"{part.title!r} never claims that no present surface provides "
                    f"it. The label is a claim a reader can check, so it needs a "
                    f"sentence that makes it — an absence named in passing inside "
                    f"another sentence is description, not the claim"
                )

            if part.tier != TIER_IMPLEMENTED:
                for cell in part.cells:
                    if IMPLEMENTED_CELL.match(cell):
                        failures.append(
                            f"{path}:{part.line}: {TIER_NAMES[part.tier]} section "
                            f"{part.title!r} contains a table cell classifying its "
                            f"row as implemented. A tier column and a tier heading "
                            f"are two mechanisms for one fact; when they disagree "
                            f"the row has two tiers. State the implemented "
                            f"baseline in prose outside this section and leave the "
                            f"cell to the constraint this section owns"
                        )
                        break

            # A page's own preamble states its scope, which by the convention
            # in `docs/spec/README.md` includes naming the committed
            # unimplemented functionality the page covers. Rule 5 cannot apply
            # there without making that convention unsatisfiable; it applies
            # from the first `##` section onward, where behavior is described.
            preamble = part.level <= 1
            if (
                part.tier == TIER_IMPLEMENTED
                and not preamble
                and part.title not in TIER_FREE_SECTIONS
            ):
                for marker in FUTURE_OWNERSHIP_MARKERS:
                    found = marker.search(body)
                    if found is not None:
                        failures.append(
                            f"{path}:{part.line}: implemented section "
                            f"{part.title!r} contains {found.group(0)!r}, which "
                            f"assigns behavior to a future change. Move it under a "
                            f"committed-unimplemented `###` heading, where its "
                            f"tier is its position"
                        )
                        break


def main() -> int:
    require_toolchain(CHECK_NAME)
    failures: list[str] = []
    checked = 0
    for page in ENROLLED_PAGES:
        path = Path(page)
        if not path.exists():
            failures.append(
                f"{page}: enrolled page is missing. Remove it from ENROLLED_PAGES "
                f"deliberately, or restore the page"
            )
            continue
        checked += 1
        check_page(path, failures)

    if failures:
        print(f"{CHECK_NAME} FAILED:")
        for failure in failures:
            print(f"  - {failure}")
        return 1
    print(f"{CHECK_NAME} passed ({checked} enrolled pages)")
    return 0


if __name__ == "__main__":
    sys.exit(main())
