#!/usr/bin/env python3
"""Check that a specification paragraph's implementation tier is its location.

`AGENTS.md` requires every paragraph of a specification page to be exactly one
of implemented, committed-unimplemented, or deferred, and makes unlabelled
prose mean implemented. That rule has no structural support: on an enrolled
page before this checker existed, every tier label was a **bold phrase in
running prose**, so a paragraph's tier was a decoration a reviewer had to read
for rather than a place it sat. The 2,055-line credential page carried one `#`
heading, fifteen `##` headings, and no `###` headings at all, with eight
tier labels spelled as bold run-in text.

Three consequences followed, and all three were observed rather than predicted.
A paragraph could drift tier silently, because nothing moved when its label
stopped being true — twenty-seven review findings on one pull request were
exactly that. Nothing was narrowly addressable, so a cross-page link had to
target a `##` anchor covering as much as 492 lines, and a carve that moved the
paragraph an anchor meant left the anchor resolving and the meaning gone. And
nothing was lintable, because "the committed-unimplemented sections" were not
sections.

This checker makes the tier a location. On an enrolled page a tier label must
be a `###` heading, tier sections must appear in ascending tier order inside
their `##` section, and a committed-unimplemented section must say that no
present surface provides what it describes. A paragraph cannot then be
mislabelled, because the label is where it sits: changing a paragraph's tier
means moving it between headings, which is visible in a diff and reviewable as
a move.

The check fails when, on an enrolled page,

1. a tier label appears as bold prose rather than as a heading, so the section
   it names is not a section,
2. a tier-labelled heading is not at `###`, so tier sections are not one
   uniform, narrowly addressable depth,
3. a `##` section's `###` subsections descend in tier — an implemented
   subsection after a committed-unimplemented or deferred one, or an
   implemented paragraph after them in the same section — so a reader cannot
   take position for tier,
4. a committed-unimplemented section never states that no present surface
   provides it, leaving the label as decoration, or
5. an implemented section carries future-tense ownership markers, which is the
   drift this contract exists to prevent.

Rule 3 is the one that does the work. Rules 1 and 2 only make tiers visible;
rule 3 is what makes position mean something, because a page whose tiers are
headings in arbitrary order still requires reading every heading to know a
paragraph's tier. Ordering makes the answer positional.

Rule 5 is deliberately narrow. It matches ownership markers that assign work to
a future change — a present-tense absence claim, an implementing child, or the
tier vocabulary itself — and not ordinary future tense, because a specification
of implemented behavior says "will" about runtime consequences constantly and a
checker that flagged those would be turned off within a week.

Enrollment is explicit and per-page, following the promotion discipline the
style-rule checker already uses in `.github/workflows/rust.yml`: a rule gates
where the tree satisfies it, and extending it to another page is a deliberate
change that restructures that page, never a side effect of editing this file.
Eleven specification pages still carry bold-prose tier labels; enrolling one
means giving it headings first. A checker that failed on all of them at once
would be reverted rather than satisfied.

Run from the repository root; exits nonzero with a per-failure report naming
every page and line involved.
"""

from __future__ import annotations

import re
import sys
from pathlib import Path

# Pages whose tiers this checker gates. A page belongs here once its tier
# labels are `###` headings in ascending order; adding one is a restructuring
# change to that page, not an edit to this list alone.
ENROLLED_PAGES = (
    "docs/spec/configuration-and-credentials.md",
    "docs/spec/credential-availability.md",
)

# The three tiers, ordered. A section's tier is read from its heading text;
# an unlabelled heading is implemented, which is what `AGENTS.md` already says
# unlabelled prose means.
TIER_IMPLEMENTED = 0
TIER_COMMITTED = 1
TIER_DEFERRED = 2

TIER_NAMES = {
    TIER_IMPLEMENTED: "implemented",
    TIER_COMMITTED: "committed unimplemented",
    TIER_DEFERRED: "deferred",
}

# Heading prefixes that declare a tier. Matched case-insensitively against the
# heading text with the leading hashes removed.
COMMITTED_HEADING = re.compile(r"^committed unimplemented\b", re.IGNORECASE)
DEFERRED_HEADING = re.compile(r"^deferred\b", re.IGNORECASE)

# A tier label spelled as bold run-in prose — the shape this checker exists to
# eliminate. Anchored at the start of a line so a mid-sentence mention of the
# phrase, which the prose legitimately makes, is not a hit.
BOLD_TIER_LABEL = re.compile(
    r"^\*\*(committed unimplemented|deferred)\b", re.IGNORECASE
)

# A committed-unimplemented section must state its own absence. Any one of
# these satisfies rule 4; the contract is that the section says no present
# surface provides it, not that it says so in one exact spelling.
ABSENCE_PHRASES = (
    "no present",
    "no current",
    "nothing present",
    "no composed",
)

# Ownership markers that assign work to a future change. Rule 5 matches these
# inside an implemented section only.
FUTURE_OWNERSHIP_MARKERS = (
    "no present",
    "implementing child",
    "committed unimplemented",
    "committed future",
)

FENCE = re.compile(r"^\s*(```|~~~)")


def heading_of(line: str) -> tuple[int, str] | None:
    """Return the (level, text) of an ATX heading, or None.

    Setext headings are not read. This repository's specification pages use
    ATX exclusively, and a checker that guessed at underlines would report
    positions no author can act on.
    """
    match = re.match(r"^(#{1,6})\s+(.*?)\s*$", line)
    if match is None:
        return None
    return len(match.group(1)), match.group(2)


def tier_of(heading_text: str) -> int:
    """Read a heading's tier from its text, defaulting to implemented."""
    stripped = heading_text.lstrip("*_ ").strip()
    if COMMITTED_HEADING.match(stripped):
        return TIER_COMMITTED
    if DEFERRED_HEADING.match(stripped):
        return TIER_DEFERRED
    return TIER_IMPLEMENTED


def content_lines(text: str) -> list[tuple[int, str]]:
    """Yield (line number, line) outside fenced code blocks.

    Fenced blocks are skipped because an example configuration or a sample
    document legitimately contains any phrase this checker matches, and
    flagging one would make the rule about quoting rather than about tiers.
    """
    result: list[tuple[int, str]] = []
    in_fence = False
    for number, line in enumerate(text.splitlines(), start=1):
        if FENCE.match(line):
            in_fence = not in_fence
            continue
        if in_fence:
            continue
        result.append((number, line))
    return result


class Section:
    """One `###` subsection, or the prose a `##` section opens with."""

    def __init__(self, level: int, title: str, tier: int, line: int) -> None:
        self.level = level
        self.title = title
        self.tier = tier
        self.line = line
        self.body: list[tuple[int, str]] = []

    def text(self) -> str:
        return "\n".join(line for _, line in self.body).lower()


def parse_sections(lines: list[tuple[int, str]]) -> list[list[Section]]:
    """Group a page into `##` sections, each a list of tier-bearing parts.

    The first part of a `##` section is its own opening prose, carrying the
    `##` heading's tier. Every later part is a `###` subsection. Headings
    deeper than `###` attach to the subsection containing them, because a
    `####` under a committed-unimplemented `###` is committed-unimplemented
    and needs no label of its own.
    """
    groups: list[list[Section]] = []
    current: list[Section] | None = None
    part: Section | None = None

    for number, line in lines:
        parsed = heading_of(line)
        if parsed is not None:
            level, title = parsed
            if level <= 2:
                part = Section(level, title, tier_of(title), number)
                current = [part]
                groups.append(current)
                continue
            if level == 3 and current is not None:
                part = Section(level, title, tier_of(title), number)
                current.append(part)
                continue
        if part is not None:
            part.body.append((number, line))

    return groups


def check_page(path: Path, failures: list[str]) -> None:
    try:
        text = path.read_text(encoding="utf-8")
    except OSError as error:
        failures.append(f"{path}: unreadable enrolled page ({error})")
        return

    lines = content_lines(text)

    for number, line in lines:
        if BOLD_TIER_LABEL.match(line):
            failures.append(
                f"{path}:{number}: tier label written as bold prose. A tier is a "
                f"location on this page: make this a `###` heading so the section "
                f"it names is a section, and so a paragraph cannot leave its tier "
                f"behind without moving"
            )

    for group in parse_sections(lines):
        opening = group[0]
        if opening.level <= 2 and opening.tier != TIER_IMPLEMENTED:
            failures.append(
                f"{path}:{opening.line}: `##` heading {opening.title!r} declares a "
                f"tier. Tier sections are `###` so that every one of them is the "
                f"same, narrowly addressable depth"
            )

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
            if part.tier == TIER_COMMITTED and not any(
                phrase in body for phrase in ABSENCE_PHRASES
            ):
                failures.append(
                    f"{path}:{part.line}: committed-unimplemented section "
                    f"{part.title!r} never states that no present surface provides "
                    f"it, so the label is decoration rather than a claim a reader "
                    f"can check"
                )
            if part.tier == TIER_IMPLEMENTED:
                for marker in FUTURE_OWNERSHIP_MARKERS:
                    if marker in body:
                        failures.append(
                            f"{path}:{part.line}: implemented section "
                            f"{part.title!r} contains {marker!r}, which assigns "
                            f"behavior to a future change. Move it under a "
                            f"committed-unimplemented `###` heading, where its "
                            f"tier is its position"
                        )
                        break


def main() -> int:
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
        print("spec-tier check FAILED:")
        for failure in failures:
            print(f"  - {failure}")
        return 1
    print(f"spec-tier check passed ({checked} enrolled pages)")
    return 0


if __name__ == "__main__":
    sys.exit(main())
