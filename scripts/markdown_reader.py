#!/usr/bin/env python3
"""Shared Markdown reading for the documentation checkers.

Two checkers in this directory ask structural questions of the specification
pages — where a section begins and what tier its heading declares, what a table
cell renders as, whether a block carries a link to a particular page. Both
began by approximating those answers with regular expressions, and both were
taught the same lesson one review round at a time: the surface being
approximated is CommonMark plus GFM, and it is a grammar rather than a list of
cases.

The record is specific. Escaped pipes counted as extra columns. A collapsed
reference link went unresolved, so a real link read as absent. A reference
definition inside a fenced example was collected as a page-level definition, so
an example satisfied a link requirement. A cell holding only an HTML comment
counted as populated. A fenced block was closed by a marker that did not open
it. A tier label written as a setext heading was invisible. Each fix was
correct and the next round found the next case.

`markdown-it-py` is already a pinned dependency of this repository
(`tooling/requirements-mdformat.txt`), installed identically by CI and by
devenv, and `.mdformat.toml` already records that a plugin-less Markdown
toolchain silently corrupts GFM tables. This module is the one place that
parser is configured and its tokens are turned into the handful of shapes the
checkers actually reason about.

It lives in a module rather than in each checker because the helpers were
briefly duplicated, and a fix applied to one copy and not the other is exactly
the cross-owner drift these checkers exist to prevent. Applying that lesson to
the tooling and not only to the prose seemed like the minimum consistency.

Nothing here decides policy. Which vocabulary counts, which tiers exist, and
which pages are enrolled are contract questions, and they stay in the checkers.
"""

from __future__ import annotations

import sys

try:
    from markdown_it import MarkdownIt
except ModuleNotFoundError:  # pragma: no cover - exercised by the CI wiring
    MarkdownIt = None  # type: ignore[assignment]


TOOLCHAIN_MESSAGE = (
    "markdown-it-py is not importable. The documentation checkers parse "
    "Markdown with the toolchain pinned in tooling/requirements-mdformat.txt "
    "rather than approximating it; install that file's requirements and rerun "
    "with its interpreter"
)


def require_toolchain(check_name: str) -> None:
    """Exit with an actionable message when the pinned parser is absent.

    Degrading to a regular-expression reader would reintroduce the defects
    this module exists to remove, and doing so silently would be worse than
    failing, so the checker stops instead.
    """
    if MarkdownIt is None:
        print(f"{check_name} FAILED:")
        print(f"  - {TOOLCHAIN_MESSAGE}")
        sys.exit(1)


def parser() -> MarkdownIt:
    """CommonMark plus GFM tables.

    `linkify` is deliberately not enabled: it is not in the pinned dependency
    set, and bare-URL autolinking bears on none of the questions asked here.
    """
    return MarkdownIt("commonmark").enable("table")


def rendered(children: list | None) -> str:
    """The text a reader sees for one inline token's children.

    HTML comments, emphasis delimiters, and link markup contribute nothing,
    which is what lets a caller tell a cell that renders blank from one whose
    source merely is not empty.
    """
    parts: list[str] = []
    for child in children or ():
        if child.type in ("text", "code_inline"):
            parts.append(child.content)
        elif child.type in ("softbreak", "hardbreak"):
            parts.append(" ")
        elif child.type == "image":
            parts.append(rendered(child.children))
    return "".join(parts)


def heading_level(token) -> int:
    """The depth of a heading token, for ATX and setext alike."""
    return int(token.tag[1:])


def line_of(token) -> int:
    """A token's one-based source line, or zero when it carries no map."""
    return (token.map[0] + 1) if token.map else 0


def table_cells(tokens: list, start: int) -> tuple[list[list[str]], int]:
    """Return the rendered rows of the table opening at `start`, and its end.

    Rows are returned as rendered text per cell, so a caller never sees the
    delimiters, the escapes, or the emphasis that produced them.
    """
    rows: list[list[str]] = []
    current: list[str] | None = None
    index = start
    while index < len(tokens) and tokens[index].type != "table_close":
        token = tokens[index]
        if token.type == "tr_open":
            current = []
        elif token.type == "tr_close" and current is not None:
            rows.append(current)
            current = None
        elif token.type in ("th_open", "td_open") and current is not None:
            current.append(rendered(tokens[index + 1].children))
        index += 1
    return rows, index


def top_level_blocks(tokens: list) -> list[tuple[int, str, list[str], list[list[str]]]]:
    """Group a token stream into top-level blocks.

    Each entry is the block's first source line, the text a reader sees, every
    link destination inside it, and the rendered rows of any table it is.
    Fenced and indented code are skipped whole: an example spelling a rule
    states nothing, and a definition inside an example is not a definition.
    Headings are returned as blocks so a caller can decide for itself whether a
    heading counts.
    """
    blocks: list[tuple[int, str, list[str], list[list[str]]]] = []
    index = 0
    while index < len(tokens):
        token = tokens[index]
        if token.level != 0:
            index += 1
            continue
        if token.type in ("fence", "code_block"):
            index += 1
            continue

        if token.type == "table_open":
            rows, end = table_cells(tokens, index)
            blocks.append((line_of(token), "", [], rows))
            index = end + 1
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
        blocks.append((line_of(token), "\n".join(text_parts), destinations, []))
        index = end
    return blocks
