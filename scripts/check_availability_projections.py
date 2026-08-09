#!/usr/bin/env python3
"""Check that the credential-availability machine has one owner and no holes.

The credential-pool feature is a distributed state machine. Seven specification
pages each own a total function over it — the phase algebra, the continuation
origins and lock order, the transcript producer list, the wire vocabulary, the
terminal-evidence algebra, the pool grammar — and for a long time each page
described the machine from its own side, in prose, as its own closed inventory.

That shape has a failure mode with a name. Adding one ending on one page leaves
six closed inventories silently wrong, and nothing detects it, because each page
remains internally consistent and every cross-page anchor still resolves. The
disagreement surfaces only when a reader happens to hold two pages open at once,
which is one pair per review round. Across forty-three rounds on one pull
request that mechanism produced twenty-nine percent of all findings and refilled
itself: a quarter of the findings were created by the previous round's accepted
fix, on a different page from the one the fix touched.

`docs/spec/credential-availability.md` replaces that arrangement with one table
whose rows are the complete closed set of endings and whose columns are exactly
those projections. This checker guards the two properties that make the table
worth having, and it guards them mechanically because an ungated documentation
convention in this repository has been observed to drift back within days.

The check fails when

1. the table's shape has drifted — a row or column is missing, renamed, or
   reordered, or a row and the declared inventory disagree — so the grid is no
   longer the closed partition it claims to be,
2. any cell is empty or a placeholder, because a blank cell cannot be told
   apart from a projection nobody has filled in yet, which is the exact defect
   the table exists to make visible, or
3. a page other than the owner states one of the machine's projections without
   linking to the owner in the same block, which is a page authoring a
   competing account of a row.

Rule 3 is the one that prevents the class from regrowing. Rules 1 and 2 keep
the table total; rule 3 keeps it sole. A page that restates a row without
linking is exactly the shape that drifted before, and it is invisible to a link
checker, because the restatement cites nothing to check.

A block is a run of lines with no blank line inside it — a paragraph, a list, or
a table. Any link to the owner page anywhere in the block satisfies rule 3 for
every projection token in it: the contract is that a reader meets the owner
where the projection is stated, not that each sentence carries its own link.

The projection vocabulary is deliberately narrow. It names the machine's
endings and the `on_pool_exhausted` values that select between them, and not the
general credential vocabulary, because a checker that fired on every mention of
a credential would be satisfied by decorative links and would stop meaning
anything. Terms the other pages legitimately own as their own algebra — the
phase names, the evidence variants — are outside it; what is inside it is the
statement of which ending a pool selection reaches.

Generated files are exempt: `docs/invariants.md` is written by
`tooling/generate_invariants.py`, so a violation there is a defect in the
generator's inputs and failing here would report it in a file no author edits.

Run from the repository root; exits nonzero with a per-failure report naming
every page and line involved.
"""

from __future__ import annotations

import re
import sys
from pathlib import Path

OWNER_PAGE = Path("docs/spec/credential-availability.md")

# The heading whose section carries the machine table. Matched by prefix so
# that the tier label's exact punctuation is not part of the contract.
TABLE_HEADING_PREFIX = "### Committed unimplemented functionality"

# The closed set of endings, in the order the table states them. A selection
# attempt ends in exactly one; the table is a partition, not a checklist.
EXPECTED_ROWS = (
    "selected",
    "contended-wait",
    "exhausted-wait",
    "pre-call fail",
    "post-failure fail",
    "successor",
    "terminal",
)

# The projections, in column order. Each is a total function over the endings
# that some other specification page owns; the last records which build
# supplies the row.
EXPECTED_COLUMNS = (
    "Outcome",
    "Turn phase and attempt disposition",
    "Wake condition",
    "Continuation origin, durable records, locks",
    "Transcript producer and entry",
    "Wire projection",
    "Terminal evidence and cause",
    "Tier and implementing child",
)

# Cell contents that read as filled but say nothing. An inapplicable
# projection must say that it is inapplicable and why; these do neither.
PLACEHOLDER_CELLS = {
    "",
    "-",
    "--",
    "---",
    "—",
    "–",
    "n/a",
    "na",
    "none",
    "tbd",
    "todo",
    "?",
    ".",
}

# Statements of which ending a pool selection reaches. A page using one of
# these is projecting a row of the table.
PROJECTION_TOKENS = (
    "on_pool_exhausted",
    "contended wait",
    "contended-wait",
    "exhausted wait",
    "exhausted-wait",
    "availability wait",
    "credential-availability wait",
    "availability successor",
    "credential_pool_exhausted",
    "pool exhaustion",
    "exhausted pool",
)

# Files that no author edits by hand.
GENERATED_FILES = (Path("docs/invariants.md"),)

FENCE = re.compile(r"^\s*(```|~~~)")


def content_lines(text: str) -> list[tuple[int, str]]:
    """Yield (line number, line) outside fenced code blocks.

    A fenced example configuration legitimately spells `on_pool_exhausted`
    without projecting anything, and requiring a link inside a TOML sample
    would make the rule about quoting rather than about ownership.
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


def split_row(line: str) -> list[str]:
    """Split one GFM table row into its cells.

    The leading and trailing pipes that mdformat always emits are dropped
    before splitting, so a well-formed row yields exactly its cells.
    """
    stripped = line.strip()
    if stripped.startswith("|"):
        stripped = stripped[1:]
    if stripped.endswith("|"):
        stripped = stripped[:-1]
    return [cell.strip() for cell in stripped.split("|")]


def is_delimiter_row(cells: list[str]) -> bool:
    return bool(cells) and all(
        set(cell) <= set("-: ") and "-" in cell for cell in cells
    )


def find_table(lines: list[tuple[int, str]], failures: list[str]) -> list[list[str]]:
    """Return the machine table's rows, or an empty list with a failure filed."""
    in_section = False
    rows: list[list[str]] = []
    for number, line in lines:
        if line.startswith("#"):
            if line.startswith(TABLE_HEADING_PREFIX):
                in_section = True
                continue
            if in_section and rows:
                break
            in_section = line.startswith(TABLE_HEADING_PREFIX)
            continue
        if not in_section:
            continue
        if line.strip().startswith("|"):
            cells = split_row(line)
            if not is_delimiter_row(cells):
                rows.append([str(number)] + cells)
        elif rows:
            break

    if not rows:
        failures.append(
            f"{OWNER_PAGE}: no machine table found under a heading starting "
            f"{TABLE_HEADING_PREFIX!r}. The table is this page's whole normative "
            f"statement; without it nothing else on the page binds"
        )
    return rows


def check_table(failures: list[str]) -> None:
    try:
        text = OWNER_PAGE.read_text(encoding="utf-8")
    except OSError as error:
        failures.append(f"{OWNER_PAGE}: unreadable owner page ({error})")
        return

    rows = find_table(content_lines(text), failures)
    if not rows:
        return

    header = rows[0]
    header_line, header_cells = header[0], header[1:]
    if tuple(header_cells) != EXPECTED_COLUMNS:
        failures.append(
            f"{OWNER_PAGE}:{header_line}: the table's columns are "
            f"{header_cells}, not the declared projections "
            f"{list(EXPECTED_COLUMNS)}. Every column is a projection some other "
            f"page owns; renaming or dropping one silently removes that page's "
            f"obligation"
        )
        return

    body = rows[1:]
    if len(body) != len(EXPECTED_ROWS):
        failures.append(
            f"{OWNER_PAGE}:{header_line}: the table states {len(body)} endings, "
            f"not the declared {len(EXPECTED_ROWS)}. The rows are a partition of "
            f"what a selection attempt can end as, so adding or dropping one is a "
            f"change to the machine and must change EXPECTED_ROWS here too"
        )
        return

    for expected, row in zip(EXPECTED_ROWS, body):
        line_number, cells = row[0], row[1:]
        if len(cells) != len(EXPECTED_COLUMNS):
            failures.append(
                f"{OWNER_PAGE}:{line_number}: row {expected!r} has {len(cells)} "
                f"cells, not {len(EXPECTED_COLUMNS)}"
            )
            continue
        if expected not in cells[0]:
            failures.append(
                f"{OWNER_PAGE}:{line_number}: expected the ending {expected!r} "
                f"here; found {cells[0]!r}. Rows are declared in order so that a "
                f"reordering cannot quietly change which ending a column "
                f"describes"
            )
        for column, cell in zip(EXPECTED_COLUMNS[1:], cells[1:]):
            if cell.strip().lower().strip("*`") in PLACEHOLDER_CELLS:
                failures.append(
                    f"{OWNER_PAGE}:{line_number}: {column!r} is empty for ending "
                    f"{expected!r}. An inapplicable projection says that it is "
                    f"inapplicable and why; a blank cannot be told apart from one "
                    f"nobody has filled in"
                )


def blocks(lines: list[tuple[int, str]]) -> list[tuple[int, str]]:
    """Group content lines into blank-line-separated blocks."""
    result: list[tuple[int, str]] = []
    start: int | None = None
    buffer: list[str] = []
    for number, line in lines:
        if line.strip():
            if start is None:
                start = number
            buffer.append(line)
            continue
        if start is not None:
            result.append((start, "\n".join(buffer)))
            start, buffer = None, []
    if start is not None:
        result.append((start, "\n".join(buffer)))
    return result


def check_links(failures: list[str]) -> int:
    checked = 0
    pages = sorted(Path("docs").rglob("*.md"))
    for page in pages:
        if page == OWNER_PAGE or page in GENERATED_FILES:
            continue
        try:
            text = page.read_text(encoding="utf-8")
        except OSError as error:
            failures.append(f"{page}: unreadable ({error})")
            continue
        checked += 1
        for start, block in blocks(content_lines(text)):
            # A heading names a topic; it does not state a projection, and
            # requiring a link inside one would put the owner's URL in every
            # table of contents without telling a reader anything.
            prose = "\n".join(
                line for line in block.splitlines() if not line.startswith("#")
            )
            lowered = prose.lower()
            present = [token for token in PROJECTION_TOKENS if token in lowered]
            if not present:
                continue
            if "credential-availability.md" in prose:
                continue
            listing = ", ".join(repr(token) for token in present)
            failures.append(
                f"{page}:{start}: states the credential-availability projection "
                f"{listing} without linking to "
                f"docs/spec/credential-availability.md. Name the column this page "
                f"projects and link the owner, rather than authoring a second "
                f"account of the row"
            )
    return checked


def main() -> int:
    failures: list[str] = []
    if not OWNER_PAGE.exists():
        print("availability-projection check FAILED:")
        print(f"  - {OWNER_PAGE} is missing; the machine has no owner")
        return 1

    check_table(failures)
    checked = check_links(failures)

    if failures:
        print("availability-projection check FAILED:")
        for failure in failures:
            print(f"  - {failure}")
        return 1
    print(
        f"availability-projection check passed "
        f"({len(EXPECTED_ROWS)} endings x {len(EXPECTED_COLUMNS) - 1} projections, "
        f"{checked} pages checked for unlinked restatement)"
    )
    return 0


if __name__ == "__main__":
    sys.exit(main())
