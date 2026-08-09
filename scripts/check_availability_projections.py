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
2. any cell renders empty or as a placeholder, because a blank cell cannot be
   told apart from a projection nobody has filled in yet, which is the exact
   defect the table exists to make visible, or
3. a page other than the owner states one of the machine's projections without
   linking to the owner in the same block, which is a page authoring a
   competing account of a row.

Rule 3 is the one that prevents the class from regrowing. Rules 1 and 2 keep
the table total; rule 3 keeps it sole. A page that restates a row without
linking is exactly the shape that drifted before, and it is invisible to a link
checker, because the restatement cites nothing to check.

Why this parses Markdown with `markdown-it-py` rather than with regular
expressions: it was written the other way first, and the record is unambiguous.
Over two review rounds the hand-rolled version produced five defects that were
all one defect — it was not a Markdown parser. Escaped pipes inside a cell
counted as extra columns and failed a correct table. A collapsed reference link
`[x][]` was not resolved, so a real link read as absent. A reference definition
inside a fenced example was collected as a page-level definition, so an example
satisfied the link requirement for a competing projection. A cell holding only
an HTML comment counted as populated, which made the no-blank-cells guarantee
true of the source and false of the rendering the table actually advertises.
Each fix was correct and the next round found the next case, because the surface
being approximated is the CommonMark and GFM grammars, and those are large.

`markdown-it-py` and `mdit-py-plugins` are already first-class pinned
dependencies of this repository (`tooling/requirements-mdformat.txt`), installed
identically by CI and by devenv, and `.mdformat.toml` already records that a
plugin-less Markdown toolchain silently corrupts GFM tables. Approximating a
second, worse parser in `scripts/` was the mistake; deleting it removes four of
those five defects by construction rather than by patching, and makes rule 2
check the rendered cell rather than its source bytes.

What remains hand-written is the part no parser decides: which vocabulary counts
as a projection of this machine, and what the declared rows and columns are.
Those are contract questions, and they belong here.

A block is one top-level Markdown block — a paragraph, a list, a table, a
blockquote. Any link to the owner anywhere in the block satisfies rule 3 for
every projection token in it: the contract is that a reader meets the owner
where the projection is stated, not that each sentence carries its own link.

The projection vocabulary is deliberately narrow. It names the machine's
endings and the `on_pool_exhausted` values that select between them, and not the
general credential vocabulary, because a checker that fired on every mention of
a credential would be satisfied by decorative links and would stop meaning
anything.

Generated files are exempt: `docs/invariants.md` is written by
`tooling/generate_invariants.py`, so a violation there is a defect in the
generator's inputs and failing here would report it in a file no author edits.

Run from the repository root with the pinned Markdown toolchain on the path;
exits nonzero with a per-failure report naming every page and line involved.
"""

from __future__ import annotations

import sys
from pathlib import Path
from urllib.parse import unquote, urlsplit

from markdown_reader import (
    line_of,
    parser,
    rendered,
    require_toolchain,
    table_cells,
)

CHECK_NAME = "availability-projection check"

OWNER_PAGE = Path("docs/spec/credential-availability.md")

# The heading whose section carries the machine table. Matched by prefix so
# that the tier label's exact punctuation is not part of the contract.
TABLE_HEADING_PREFIX = "Committed unimplemented functionality"

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

# Rendered cell contents that read as filled but say nothing. An inapplicable
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

# Emphasis punctuation carries no content of its own, so a cell built only from
# it renders blank even though its source is not empty.
DECORATION = "*_~` \t"

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
    # Two more row names, distinctive enough that a page spelling one is
    # quoting this machine's closed vocabulary rather than using ordinary
    # words. `selected` and `terminal` stay out for the opposite reason: both
    # are common English on these pages, and a rule that fired on them would be
    # satisfied by decorative links and would stop meaning anything. The
    # hyphenated `availability-successor` stays out too — it is used as a
    # compound modifier ("availability-successor storage") rather than as a
    # statement of which ending an attempt reached.
    "pre-call fail",
    "post-failure fail",
)

# Files that no author edits by hand.
GENERATED_FILES = (Path("docs/invariants.md"),)

# The em dash that separates an outcome's name from its gloss in the first
# column. The name is compared exactly; the gloss is prose.
OUTCOME_SEPARATOR = "—"


def is_blank(text: str) -> bool:
    """Report whether a rendered cell says nothing."""
    stripped = text.strip().strip(DECORATION).strip()
    return stripped.lower() in PLACEHOLDER_CELLS


def source_cell_count(line: str) -> int:
    """Count a table row's cells in the source, before GFM truncation.

    This is the one place a raw scan is unavoidable, and it is here because
    the parser cannot answer the question. GFM truncates a row carrying more
    cells than the header rather than rejecting it, so an unescaped pipe does
    not add a column — it silently discards the trailing cell's content and
    leaves a well-formed row of the declared width. The parsed token stream
    has already lost that, so the count is taken from the source and compared
    with the header's, computed identically.

    Only one escape matters inside a table row, `\\|`, so the scan is a single
    bounded rule rather than an approximation of the grammar.
    """
    stripped = line.strip()
    cells = 1
    index = 0
    while index < len(stripped):
        if stripped[index] == "\\":
            index += 2
            continue
        if stripped[index] == "|":
            cells += 1
        index += 1
    return cells


def table_rows(tokens: list) -> tuple[list[list[str]], int]:
    """Return the machine table's rendered rows and its source line.

    The table is the first one inside a section whose heading declares the
    committed-unimplemented tier, which is where the page places it.
    """
    in_section = False
    index = 0
    while index < len(tokens):
        token = tokens[index]
        if token.type == "heading_open":
            text = rendered(tokens[index + 1].children)
            in_section = TABLE_HEADING_PREFIX.lower() in text.lower()
        elif token.type == "table_open" and in_section:
            rows, _ = table_cells(tokens, index)
            return rows, line_of(token)
        index += 1
    return [], 0


def check_table(failures: list[str]) -> None:
    try:
        text = OWNER_PAGE.read_text(encoding="utf-8")
    except OSError as error:
        failures.append(f"{OWNER_PAGE}: unreadable owner page ({error})")
        return

    source_lines = text.splitlines()
    rows, line = table_rows(parser().parse(text))
    if rows:
        widths = [
            source_cell_count(source_lines[index])
            for index in range(line - 1, min(line - 1 + len(rows) + 1, len(source_lines)))
            if source_lines[index].lstrip().startswith("|")
        ]
        if widths and any(width > widths[0] for width in widths[1:]):
            failures.append(
                f"{OWNER_PAGE}:{line}: a table row carries more cells in the "
                f"source than the header declares. GFM truncates the surplus "
                f"rather than rejecting it, so the row still renders at the "
                f"declared width and a projection's content is silently "
                f"discarded. Escape a pipe meant as content as `\\|`"
            )
    if not rows:
        failures.append(
            f"{OWNER_PAGE}: no machine table found under a heading declaring the "
            f"committed-unimplemented tier. The table is this page's whole "
            f"normative statement; without it nothing else on the page binds"
        )
        return

    header, body = rows[0], rows[1:]
    if tuple(header) != EXPECTED_COLUMNS:
        failures.append(
            f"{OWNER_PAGE}:{line}: the table's columns are {header}, not the "
            f"declared projections {list(EXPECTED_COLUMNS)}. Every column is a "
            f"projection some other page owns; renaming or dropping one silently "
            f"removes that page's obligation"
        )
        return

    if len(body) != len(EXPECTED_ROWS):
        failures.append(
            f"{OWNER_PAGE}:{line}: the table states {len(body)} endings, not the "
            f"declared {len(EXPECTED_ROWS)}. The rows are a partition of what a "
            f"selection attempt can end as, so adding or dropping one is a change "
            f"to the machine and must change EXPECTED_ROWS here too"
        )
        return

    for expected, cells in zip(EXPECTED_ROWS, body):
        if len(cells) != len(EXPECTED_COLUMNS):
            failures.append(
                f"{OWNER_PAGE}:{line}: row {expected!r} has {len(cells)} cells, "
                f"not {len(EXPECTED_COLUMNS)}"
            )
            continue
        name = cells[0].split(OUTCOME_SEPARATOR, 1)[0].strip().strip(DECORATION)
        if name != expected:
            failures.append(
                f"{OWNER_PAGE}:{line}: expected the ending {expected!r} here; "
                f"found {name!r}. Rows are declared in order and compared by "
                f"name, so neither a reordering nor a rename can quietly change "
                f"which ending a column describes"
            )
        for column, cell in zip(EXPECTED_COLUMNS[1:], cells[1:]):
            if is_blank(cell):
                failures.append(
                    f"{OWNER_PAGE}:{line}: {column!r} renders empty for ending "
                    f"{expected!r}. An inapplicable projection says that it is "
                    f"inapplicable and why; a blank cannot be told apart from one "
                    f"nobody has filled in"
                )


def top_level_blocks(tokens: list) -> list[tuple[int, str, list[str]]]:
    """Group a token stream into top-level blocks.

    Each entry is the block's first source line, the text a reader sees, and
    every link destination inside it. Fenced and indented code are skipped
    whole: an example spelling a projection projects nothing, and a definition
    inside an example is not a definition.
    """
    blocks: list[tuple[int, str, list[str]]] = []
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
            # A heading names a topic; it does not state a projection, and
            # requiring a link inside one would put the owner's URL in every
            # table of contents without telling a reader anything.
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

        text_parts: list[str] = []
        destinations: list[str] = []
        for inner in tokens[index:end]:
            if inner.type == "inline":
                text_parts.append(rendered(inner.children))
                for child in inner.children or ():
                    if child.type == "link_open":
                        destinations.append(child.attrGet("href") or "")
        blocks.append((line_of(token), "\n".join(text_parts), destinations))
        index = end
    return blocks


def resolves_to_owner(destination: str, page: Path) -> bool:
    """Report whether a link destination names the owner page.

    Resolution is relative to the linking page, so `credential-availability.md`
    from within `docs/spec` and `spec/credential-availability.md` from `docs`
    both count, and a same-named file in another directory does not. A
    fragment-only destination points inside the linking page, never at the
    owner.
    """
    target = destination.split("#", 1)[0].strip()
    if not target:
        return False
    if urlsplit(target).scheme or target.startswith("//"):
        return False
    try:
        resolved = (page.parent / unquote(target)).resolve()
    except (OSError, ValueError):
        return False
    return resolved == OWNER_PAGE.resolve()


def check_links(failures: list[str]) -> int:
    checked = 0
    markdown = parser()
    for page in sorted(Path("docs").rglob("*.md")):
        if page == OWNER_PAGE or page in GENERATED_FILES:
            continue
        try:
            text = page.read_text(encoding="utf-8")
        except OSError as error:
            failures.append(f"{page}: unreadable ({error})")
            continue
        checked += 1
        for line, prose, destinations in top_level_blocks(markdown.parse(text)):
            lowered = prose.lower()
            present = [token for token in PROJECTION_TOKENS if token in lowered]
            if not present:
                continue
            if any(resolves_to_owner(target, page) for target in destinations):
                continue
            listing = ", ".join(repr(token) for token in present)
            failures.append(
                f"{page}:{line}: states the credential-availability projection "
                f"{listing} without a link whose destination resolves to "
                f"{OWNER_PAGE}. Name the column this page projects and link the "
                f"owner, rather than authoring a second account of the row "
                f"(naming the file without linking it does not satisfy this)"
            )
    return checked


def main() -> int:
    require_toolchain(CHECK_NAME)
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
