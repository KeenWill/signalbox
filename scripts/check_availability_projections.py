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
from urllib.parse import unquote, urlsplit

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

# Link extraction. A block satisfies rule 3 by carrying a navigable link whose
# destination resolves to the owner page — never by merely containing its
# filename, since a block asserting that the owner does not cover something
# would otherwise satisfy the very gate it violates.
CODE_SPAN = re.compile(r"`+[^`]*`+")
INLINE_DESTINATION = re.compile(r"\]\(\s*<?([^)\s>]+)")
REFERENCE_DEFINITION = re.compile(r"^ {0,3}\[([^\]]+)\]:\s*<?([^\s>]+)", re.M)
COLLAPSED_REFERENCE = re.compile(r"\]\[([^\]]*)\]")
SHORTCUT_REFERENCE = re.compile(r"\[([^\]^]+)\](?![\(\[:])")


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


def is_escaped(text: str, index: int) -> bool:
    """Report whether the character at ``index`` is backslash-escaped."""
    backslashes = 0
    position = index - 1
    while position >= 0 and text[position] == "\\":
        backslashes += 1
        position -= 1
    return backslashes % 2 == 1


def split_row(line: str) -> list[str]:
    """Split one GFM table row into its cells, honouring escaped pipes.

    A cell may legitimately contain a pipe by escaping it, and GFM resolves the
    escape before rendering, so such a row has one fewer column than a naive
    split reports. Splitting on every pipe would fail a table that renders with
    exactly the declared eight columns — a checker blocking CI over a document
    that is correct, which is the way a gate gets deleted rather than fixed.

    The leading and trailing pipes mdformat always emits are dropped first, and
    a trailing pipe closes the row only when it is not itself escaped. Escapes
    resolve during the scan, so an escaped pipe contributes a literal pipe to
    the cell while an escaped backslash contributes a backslash and leaves any
    following pipe to act as a delimiter.
    """
    stripped = line.strip()
    if stripped.startswith("|"):
        stripped = stripped[1:]
    if stripped.endswith("|") and not is_escaped(stripped, len(stripped) - 1):
        stripped = stripped[:-1]

    cells: list[str] = []
    current: list[str] = []
    index = 0
    while index < len(stripped):
        character = stripped[index]
        if character == "\\" and index + 1 < len(stripped):
            current.append(stripped[index + 1])
            index += 2
            continue
        if character == "|":
            cells.append("".join(current).strip())
            current = []
            index += 1
            continue
        current.append(character)
        index += 1
    cells.append("".join(current).strip())
    return cells


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


def reference_definitions(text: str) -> dict[str, str]:
    """Collect a page's reference-link definitions by lowercased label.

    Definitions are gathered per page rather than per block because a
    reference-style link is routinely defined far from where it is used, and a
    gate that ignored them would reject a correctly linked page.
    """
    return {
        match.group(1).strip().lower(): match.group(2)
        for match in REFERENCE_DEFINITION.finditer(text)
    }


def resolves_to_owner(destination: str, page: Path) -> bool:
    """Report whether a link destination names the owner page.

    Resolution is relative to the linking page, so `credential-availability.md`
    from within `docs/spec` and `spec/credential-availability.md` from `docs`
    both count, and a same-named file in another directory does not. A fragment
    is dropped first; a destination that is only a fragment points inside the
    linking page and never at the owner.
    """
    target = destination.split("#", 1)[0].strip()
    if not target:
        return False
    parsed = urlsplit(target)
    if parsed.scheme or target.startswith("//"):
        return False
    try:
        resolved = (page.parent / unquote(target)).resolve()
    except (OSError, ValueError):
        return False
    return resolved == OWNER_PAGE.resolve()


def links_to_owner(block: str, page: Path, definitions: dict[str, str]) -> bool:
    """Report whether a block carries a real link to the owner page.

    Code spans are removed first, so a backticked mention of the filename — or
    of link-looking punctuation inside a code sample — cannot satisfy the rule.
    Token detection deliberately does not do this: a projection is almost
    always named in backticks, so stripping code spans there would blind the
    rule it exists to enforce.
    """
    text = CODE_SPAN.sub(" ", block)

    destinations = [match.group(1) for match in INLINE_DESTINATION.finditer(text)]
    for pattern in (COLLAPSED_REFERENCE, SHORTCUT_REFERENCE):
        for match in pattern.finditer(text):
            label = match.group(1).strip().lower()
            if label in definitions:
                destinations.append(definitions[label])

    return any(resolves_to_owner(target, page) for target in destinations)


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
        definitions = reference_definitions(text)
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
            if links_to_owner(prose, page, definitions):
                continue
            listing = ", ".join(repr(token) for token in present)
            failures.append(
                f"{page}:{start}: states the credential-availability projection "
                f"{listing} without a link whose destination resolves to "
                f"docs/spec/credential-availability.md. Name the column this page "
                f"projects and link the owner, rather than authoring a second "
                f"account of the row (naming the file without linking it does not "
                f"satisfy this)"
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
