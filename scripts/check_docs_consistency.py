#!/usr/bin/env python3
"""Check mechanically verifiable links across the living-spec surface.

The check is deterministic and offline. It verifies:

1. every relative file citation in an invariant Enforcement cell resolves to a
   repository file; a code-spanned test name bound to a cited file must appear
   in that file,
2. every relative Markdown link in ``docs/**/*.md`` and the root ``AGENTS.md``
   resolves inside the repository, including GitHub-style heading fragments,
3. every H2 in ``docs/decisions.md`` is a valid dated entry and entry dates are
   non-increasing, and
4. every subsystem page under ``docs/spec/`` has an offline verification
   reference whose PR token uses ``PR #N (`branch-ref`)``.

External links, semantic freshness of verification references, and reverse
discovery of every INV-tagged test are deliberately outside this check. Run
from any directory; exits nonzero with one stable line per violation.

The input domain is mdformat-canonical Markdown: CI enforces ``mdformat
--check`` over the same files before this check runs, so shapes that
canonicalization cannot emit (unbalanced reference-definition destinations,
inline links split across blocks, semicolonless entity references, raw-HTML
attribute traps, Setext underlines adjacent to foreign containers) are outside
the parsing contract. Hardening against non-canonical Markdown is deliberately
out of scope while that gate holds.
"""

from __future__ import annotations

import html
import re
import string
import sys
import unicodedata
from dataclasses import dataclass
from datetime import date
from functools import lru_cache
from pathlib import Path
from urllib.parse import unquote, urlsplit

ROOT = Path(__file__).resolve().parent.parent
INVARIANTS = Path("docs/invariants.md")
DECISIONS = Path("docs/decisions.md")
SPEC_DIR = Path("docs/spec")

ATX_HEADING = re.compile(r"^ {0,3}(#{1,6})(?:[ \t]+|$)(.*)$")
SETEXT_HEADING = re.compile(r"^ {0,3}(?:=+|-+)[ \t]*$")
FENCE = re.compile(r"^ {0,3}(?P<fence>`{3,}|~{3,})")
AUTOLINK = re.compile(
    r"<((?:[A-Za-z][A-Za-z0-9+.-]{1,31}:[^<>\s]*|"
    r"[^<>\s@]+@[^<>\s@]+))>"
)
HEADING_CONTAINER = re.compile(
    r"^ {0,3}(?:>[ \t]?|(?:[-+*]|\d+[.)])[ \t]+)"
)
BLOCK_QUOTE_CONTAINER = re.compile(r"^ {0,3}>[ \t]?")
RAW_HTML_LITERAL_OPEN = re.compile(
    r"^ {0,3}<(?P<tag>pre|script|style|textarea)(?:[ \t\r\n>]|$)",
    re.IGNORECASE,
)
RAW_HTML_TEXT_OPEN = re.compile(
    r"^ {0,3}<(?P<tag>"
    r"iframe|noembed|noframes|script|style|textarea|title|xmp"
    r")(?:[ \t\r\n>]|$)",
    re.IGNORECASE,
)
RAW_HTML_BLOCK_TAG = re.compile(
    r"^ {0,3}</?(?:"
    r"address|article|aside|base|basefont|blockquote|body|caption|center|col|"
    r"colgroup|dd|details|dialog|dir|div|dl|dt|fieldset|figcaption|figure|"
    r"footer|form|frame|frameset|h[1-6]|head|header|hr|html|iframe|legend|li|"
    r"link|main|menu|menuitem|nav|noframes|ol|optgroup|option|p|param|search|"
    r"section|summary|table|tbody|td|tfoot|th|thead|title|tr|track|ul"
    r")(?:[ \t\r\n/>]|$)",
    re.IGNORECASE,
)
RAW_HTML_COMPLETE_TAG = re.compile(
    r"""^[ ]{0,3}(?:
        </[A-Za-z][A-Za-z0-9-]*[ \t]*>
        |
        <[A-Za-z][A-Za-z0-9-]*
        (?:
            [ \t\r\n]+[A-Za-z_:][A-Za-z0-9_.:-]*
            (?:
                [ \t\r\n]*=[ \t\r\n]*
                (?:[^ "'=<>`]+|'[^']*'|"[^"]*")
            )?
        )*
        [ \t\r\n]*/?>
    )[ \t]*(?:\r?\n)?$""",
    re.VERBOSE,
)
EXPLICIT_ANCHOR = re.compile(
    r"<a(?![A-Za-z0-9-])[^>]*[ \t\r\n](?:name|id)[ \t\r\n]*=[ \t\r\n]*"
    r'(?:"([^"]+)"|\'([^\']+)\'|([^\s>]+))',
    re.IGNORECASE,
)
REFERENCE_LABEL = r"(?:\\[^\r\n]|[^\]\\\r\n])+"
REFERENCE_DEFINITION = re.compile(
    rf"(?m)^[ \t]*\[(?P<label>{REFERENCE_LABEL})\]:[ \t]*"
    r"(?:\r?\n[ \t]+)?"
    r"(?:<(?P<angled_destination>(?:\\.|[^>\\\r\n])*)>|"
    r"(?P<destination>\S+))"
    r"(?:(?:[ \t]+|\r?\n[ \t]+)(?:"
    r'"(?:\\.|[^"\\\r\n])*"'
    r"|'(?:\\.|[^'\\\r\n])*'"
    r"|\((?:\\.|[^)\\\r\n])*\)"
    r"))?"
    r"[ \t]*(?=\r?$)"
)
LIST_ITEM = re.compile(r"^[ \t]*(?:[-+*]|\d+[.)])[ \t]+")
FENCE_LIST_CONTAINER = re.compile(r"^ {0,3}(?:[-+*]|\d+[.)])[ \t]+")
DECISION_HEADING = re.compile(r"^(\d{4}-\d{2}-\d{2}) — (\S.*)$")
PR_TOKEN = re.compile(
    r"\bPR #([1-9][0-9]*)[ \t\r\n]+\("
    r"`([^\s`]+)`"
    r"\)"
)
INLINE_MARKUP_OPENERS = r"[\[(<*_~`\"'“‘]*"
LIST_ITEM_BOUNDARY = r"\n[ \t]*(?:[-+*]|\d+[.)])[ \t]"
CLAUSE_BOUNDARY = (
    r"\n[ \t]*\n"
    rf"|{LIST_ITEM_BOUNDARY}"
    rf"|[.!?][ \t\r\n]+{INLINE_MARKUP_OPENERS}(?-i:[A-Z])"
    r"|;"
)
VERIFICATION_LEAD = re.compile(
    r"\bverified\b"
    rf"(?:(?!{CLAUSE_BOUNDARY}).)*?"
    r"\b(?:against|through)\b"
    rf"(?:(?!{CLAUSE_BOUNDARY}).)*?"
    r"(?P<pr>\bPR[ \t]*#)",
    re.IGNORECASE | re.DOTALL,
)
VERIFICATION_NEGATION = re.compile(
    r"(?:\b(?:not|never)(?:[ \t]+\w+){0,3}[ \t]+"
    r"|\bcannot(?:[ \t]+\w+){0,3}[ \t]+"
    r"|\bno(?:[ \t]+\w+){1,5}[ \t]+"
    r"|\b[A-Za-z]+n['’]t(?:[ \t]+\w+){0,3}[ \t]+)$",
    re.IGNORECASE,
)
EMPHASIS_DELIMITER = re.compile(r"[*_~]+")
THEMATIC_BREAK = re.compile(
    r"^ {0,3}(?:(?:\*[ \t]*){3,}|(?:-[ \t]*){3,}|(?:_[ \t]*){3,})$"
)
DECISION_ENTRY_HEADING = re.compile(r"^ {0,3}##(?!#)(?:[ \t]+|$)(?P<title>.*)$")
LIST_WRAPPED_ENTRY_HEADING = re.compile(
    r"^ {0,3}(?:(?:[-+*]|\d+[.)])[ \t]+)+##(?!#)(?:[ \t]|$)"
)
TEST_GROUP = re.compile(
    r"\btests?[ \t]+"
    r"(?P<names>"
    r"`[A-Za-z_][A-Za-z0-9_:]*`"
    r"(?:[ \t]*(?:,[ \t]*(?:and[ \t]+)?|and[ \t]+)"
    r"`[A-Za-z_][A-Za-z0-9_:]*`)*"
    r")"
    r"[ \t]+in\b",
    re.IGNORECASE,
)
NATURAL_TEST_BINDING = re.compile(
    r"(?:"
    r"`(?P<before>[A-Za-z_][A-Za-z0-9_:]*)`[ \t]+tests?\b"
    r"|"
    r"\btests?(?:[ \t]+named)?[ \t]+"
    r"`(?P<after>[A-Za-z_][A-Za-z0-9_:]*)`"
    r")"
    r"[ \t]+in\b",
    re.IGNORECASE,
)


@dataclass(frozen=True, order=True)
class Violation:
    """One deterministic, repository-relative failure."""

    path: str
    line: int
    category: str
    message: str

    def render(self) -> str:
        return f"{self.path}:{self.line}: {self.category}: {self.message}"


@dataclass(frozen=True)
class MarkdownLink:
    """One parsed inline link or reference-definition destination."""

    label: str
    destination: str
    offset: int


def repository_path(root: Path, path: Path) -> str:
    """Render a path relative to the checked repository when possible."""
    try:
        return path.relative_to(root).as_posix()
    except ValueError:
        return str(path)


def line_number(text: str, offset: int) -> int:
    return text.count("\n", 0, offset) + 1


def mask_range(buffer: list[str], start: int, end: int) -> None:
    for index in range(start, end):
        if buffer[index] not in "\r\n":
            buffer[index] = " "


def strip_block_quote_containers(line: str) -> str:
    """Remove repeated block-quote prefixes without consuming list markers."""
    return block_quote_context(line)[0]


def block_quote_context(line: str, limit: int | None = None) -> tuple[str, int]:
    """Remove up to ``limit`` quote prefixes and return the removed depth."""
    depth = 0
    while True:
        if limit is not None and depth == limit:
            return line, depth
        match = BLOCK_QUOTE_CONTAINER.match(line)
        if match is None:
            return line, depth
        line = line[match.end() :]
        depth += 1


def mask_reference_container_prefixes(text: str) -> str:
    """Replace quote/list prefixes while preserving reference-definition offsets."""
    buffer = list(text)
    offset = 0
    for line in text.splitlines(keepends=True):
        consumed = 0
        while True:
            remainder = line[consumed:]
            match = BLOCK_QUOTE_CONTAINER.match(remainder)
            if match is None:
                match = FENCE_LIST_CONTAINER.match(remainder)
            if match is None:
                break
            end = consumed + match.end()
            mask_range(buffer, offset + consumed, offset + end)
            consumed = end
        offset += len(line)
    return "".join(buffer)


def fence_opening_context(
    line: str, list_content_columns: list[int]
) -> tuple[str, int, int]:
    """Return fence content after active Markdown quote/list containers."""
    content, quote_depth = block_quote_context(line)
    if not content.strip():
        return (
            content,
            list_content_columns[-1] if list_content_columns else 0,
            quote_depth,
        )

    prefix = re.match(r"^[ \t]*", content).group(0)
    leading = indentation_columns(prefix)
    while list_content_columns and leading < list_content_columns[-1]:
        list_content_columns.pop()

    marker = LIST_ITEM.match(content)
    marker_is_container = marker is not None and (
        (not list_content_columns and leading <= 3)
        or (
            list_content_columns
            and 0 <= leading - list_content_columns[-1] <= 3
        )
    )
    if marker_is_container:
        remainder = content
        content_column = 0
        while True:
            marker = LIST_ITEM.match(remainder)
            if marker is None:
                break
            content_column += indentation_columns(marker.group(0))
            list_content_columns.append(content_column)
            remainder = remainder[marker.end() :]
        return remainder, content_column, quote_depth

    if list_content_columns:
        content_column = list_content_columns[-1]
        return (
            remove_indentation(content, content_column),
            content_column,
            quote_depth,
        )
    return content, 0, quote_depth


def remove_indentation(line: str, columns: int) -> str:
    """Remove exactly ``columns`` leading indentation columns when present."""
    position = 0
    consumed = 0
    while position < len(line) and consumed < columns:
        character = line[position]
        if character not in " \t":
            return line
        next_column = (
            consumed + 4 - (consumed % 4)
            if character == "\t"
            else consumed + 1
        )
        if next_column > columns:
            return line
        consumed = next_column
        position += 1
    return line[position:] if consumed == columns else line


def mask_fenced_code(text: str) -> str:
    """Replace fenced code with spaces while preserving offsets and lines."""
    buffer = list(text)
    offset = 0
    fence_char: str | None = None
    fence_length = 0
    fence_container_indent = 0
    fence_quote_depth = 0
    list_content_columns: list[int] = []
    for line in text.splitlines(keepends=True):
        if fence_char is None:
            (
                container_content,
                container_indent,
                container_quote_depth,
            ) = fence_opening_context(line, list_content_columns)
            match = FENCE.match(container_content)
        else:
            quoted_content, quote_depth = block_quote_context(
                line, fence_quote_depth
            )
            leading = indentation_columns(
                re.match(r"^[ \t]*", quoted_content).group(0)
            )
            container_ended = quote_depth < fence_quote_depth or (
                fence_container_indent
                and quoted_content.strip()
                and leading < fence_container_indent
            )
            if container_ended:
                fence_char = None
                fence_length = 0
                fence_container_indent = 0
                fence_quote_depth = 0
                (
                    container_content,
                    container_indent,
                    container_quote_depth,
                ) = fence_opening_context(line, list_content_columns)
                match = FENCE.match(container_content)
            else:
                container_content = remove_indentation(
                    quoted_content,
                    fence_container_indent,
                )
                container_indent = 0
                container_quote_depth = 0
                match = None

        if fence_char is None and match is not None:
            fence = match.group("fence")
            if fence[0] == "`" and "`" in container_content[match.end() :]:
                offset += len(line)
                continue
            fence_char = fence[0]
            fence_length = len(fence)
            fence_container_indent = container_indent
            fence_quote_depth = container_quote_depth
            mask_range(buffer, offset, offset + len(line))
        elif fence_char is not None:
            mask_range(buffer, offset, offset + len(line))
            closing = re.match(
                rf"^ {{0,3}}{re.escape(fence_char)}{{{fence_length},}}[ \t]*"
                r"(?:\r?\n)?$",
                container_content,
            )
            if closing:
                fence_char = None
                fence_length = 0
                fence_container_indent = 0
                fence_quote_depth = 0
        offset += len(line)
    return "".join(buffer)


def opens_list_item(content: str) -> bool:
    """Return whether a non-blank line starts a new, non-empty list item."""
    marker = LIST_ITEM.match(content)
    return marker is not None and bool(content[marker.end() :].strip())


@lru_cache(maxsize=8)
def block_boundaries(text: str) -> tuple[int, ...]:
    """Return offsets at which inline parsing cannot continue.

    A code span is inline content of one leaf block, so it can never span a
    blank line or reach across an ATX heading, which always interrupts a
    paragraph. Both edges of such a line are boundaries.

    A list-item marker likewise opens a new block: sibling items are separate
    leaf blocks, so their inline content cannot pair across the marker. Only
    the leading edge of such a line is a boundary, because the item's own
    inline content continues on the same line after the marker.
    """
    boundaries: list[int] = []
    offset = 0
    for line in text.splitlines(keepends=True):
        content = strip_block_quote_containers(line.rstrip("\r\n"))
        if not content.strip() or ATX_HEADING.match(content):
            boundaries.append(offset)
            boundaries.append(offset + len(line))
        elif opens_list_item(content):
            boundaries.append(offset)
        offset += len(line)
    return tuple(boundaries)


def block_limit(text: str, offset: int) -> int:
    """Return the first offset after ``offset`` that inline parsing cannot cross."""
    return next(
        (
            boundary
            for boundary in block_boundaries(text)
            if boundary > offset
        ),
        len(text),
    )


def inline_code_ranges(text: str) -> list[tuple[int, int]]:
    """Return complete inline-code ranges while preserving source offsets."""
    ranges: list[tuple[int, int]] = []
    index = 0
    while index < len(text):
        if text[index] != "`":
            index += 1
            continue
        run_end = index
        while run_end < len(text) and text[run_end] == "`":
            run_end += 1
        delimiter = text[index:run_end]
        limit = block_limit(text, index)
        closing: int | None = None
        candidate = run_end
        while True:
            candidate = text.find("`", candidate)
            if candidate == -1 or candidate >= limit:
                break
            candidate_end = candidate
            while candidate_end < len(text) and text[candidate_end] == "`":
                candidate_end += 1
            if candidate_end - candidate == len(delimiter):
                closing = candidate
                break
            candidate = candidate_end
        if closing is None:
            index = run_end
            continue
        end = closing + len(delimiter)
        ranges.append((index, end))
        index = end
    return ranges


def offset_in_ranges(offset: int, ranges: list[tuple[int, int]]) -> bool:
    return any(start <= offset < end for start, end in ranges)


def mask_inline_code(text: str) -> str:
    """Replace complete inline-code spans with spaces, preserving offsets."""
    buffer = list(text)
    for start, end in inline_code_ranges(text):
        mask_range(buffer, start, end)
    return "".join(buffer)


def mask_html_comments(text: str) -> str:
    """Replace HTML comments outside inline code, preserving offsets."""
    buffer = list(text)
    code_ranges = inline_code_ranges(text)
    index = 0
    while True:
        opening = text.find("<!--", index)
        if opening == -1:
            break
        containing = next(
            (
                (start, end)
                for start, end in code_ranges
                if start <= opening < end
            ),
            None,
        )
        if containing is not None:
            index = containing[1]
            continue
        closing = text.find("-->", opening + 4)
        end = len(text) if closing == -1 else closing + 3
        mask_range(buffer, opening, end)
        index = end
    return "".join(buffer)


def mask_raw_html_blocks(text: str) -> str:
    """Replace CommonMark literal HTML blocks while preserving source lines."""
    buffer = list(text)
    offset = 0
    terminator: re.Pattern[str] | None = None
    blank_terminated = False
    previous_blank = True
    for line in text.splitlines(keepends=True):
        container_content = strip_heading_containers(line)
        blank = not container_content.strip()

        if blank_terminated and blank:
            blank_terminated = False
        elif blank_terminated:
            mask_range(buffer, offset, offset + len(line))
        elif terminator is not None:
            mask_range(buffer, offset, offset + len(line))
            if terminator.search(container_content):
                terminator = None
        else:
            opening = RAW_HTML_LITERAL_OPEN.match(container_content)
            if opening is not None:
                tag = opening.group("tag")
                terminator = re.compile(
                    rf"</{re.escape(tag)}[ \t\r\n]*>",
                    re.IGNORECASE,
                )
            elif re.match(r"^ {0,3}<\?", container_content):
                terminator = re.compile(r"\?>")
            elif re.match(r"^ {0,3}<![A-Z]", container_content):
                terminator = re.compile(r">")
            elif re.match(r"^ {0,3}<!\[CDATA\[", container_content):
                terminator = re.compile(r"\]\]>")
            elif RAW_HTML_BLOCK_TAG.match(container_content):
                blank_terminated = True
            elif previous_blank and RAW_HTML_COMPLETE_TAG.match(
                container_content
            ):
                blank_terminated = True

            if terminator is not None or blank_terminated:
                mask_range(buffer, offset, offset + len(line))
                if terminator is not None and terminator.search(
                    container_content
                ):
                    terminator = None

        previous_blank = blank
        offset += len(line)
    return "".join(buffer)


def mask_raw_text_html_blocks(text: str) -> str:
    """Mask HTML raw-text elements whose contents cannot define anchors."""
    buffer = list(text)
    offset = 0
    terminator: re.Pattern[str] | None = None
    for line in text.splitlines(keepends=True):
        container_content = strip_heading_containers(line)
        if terminator is None:
            opening = RAW_HTML_TEXT_OPEN.match(container_content)
            if opening is not None:
                tag = opening.group("tag")
                terminator = re.compile(
                    rf"</{re.escape(tag)}[ \t\r\n]*>",
                    re.IGNORECASE,
                )

        if terminator is not None:
            mask_range(buffer, offset, offset + len(line))
            if terminator.search(container_content):
                terminator = None
        offset += len(line)
    return "".join(buffer)


def indentation_columns(prefix: str) -> int:
    columns = 0
    for character in prefix:
        columns = columns + 4 - (columns % 4) if character == "\t" else columns + 1
    return columns


def opens_paragraph(content: str) -> bool:
    """Return whether a non-blank line leaves a paragraph open to continue."""
    stripped = content.lstrip(" \t")
    return not (
        ATX_HEADING.match(stripped)
        or THEMATIC_BREAK.match(stripped)
        or SETEXT_HEADING.match(stripped)
    )


def mask_indented_code(text: str) -> str:
    """Replace CommonMark-style indented code blocks, preserving offsets.

    Enclosing list items are tracked as a stack of content columns, the same
    shape ``fence_opening_context`` keeps. Dedenting leaves only the items the
    line escaped, so a sibling marker re-establishes its own content column
    instead of dropping the reader back to the document margin and reading the
    item's continuation paragraphs as indented code.
    """
    buffer = list(text)
    offset = 0
    paragraph_open = False
    in_code = False
    list_content_columns: list[int] = []

    for line in text.splitlines(keepends=True):
        content = strip_block_quote_containers(line.rstrip("\r\n"))
        if not content.strip():
            if in_code:
                mask_range(buffer, offset, offset + len(line))
            paragraph_open = False
            offset += len(line)
            continue

        prefix = re.match(r"^[ \t]*", content).group(0)
        leading = indentation_columns(prefix)
        while list_content_columns and leading < list_content_columns[-1]:
            list_content_columns.pop()
            in_code = False
        relative = (
            leading - list_content_columns[-1] if list_content_columns else leading
        )
        marker = LIST_ITEM.match(content)
        if marker is not None and relative <= 3:
            list_content_columns.append(indentation_columns(marker.group(0)))
            in_code = False
            paragraph_open = True
            offset += len(line)
            continue

        indented = relative >= 4
        if indented and (in_code or not paragraph_open):
            in_code = True
            mask_range(buffer, offset, offset + len(line))
        else:
            in_code = False
            paragraph_open = opens_paragraph(content)

        offset += len(line)
    return "".join(buffer)


def mask_block_content(text: str) -> str:
    """Mask code and literal HTML content in Markdown source."""
    return mask_indented_code(
        mask_raw_html_blocks(mask_html_comments(mask_fenced_code(text)))
    )


def find_closing_bracket(text: str, start: int) -> int | None:
    depth = 1
    index = start + 1
    while index < len(text):
        if text[index] == "\\":
            index += 2
            continue
        if text[index] == "[":
            depth += 1
        elif text[index] == "]":
            depth -= 1
            if depth == 0:
                return index
        index += 1
    return None


def is_escaped(text: str, offset: int) -> bool:
    """Return whether an opener follows an odd-length backslash run."""
    backslashes = 0
    offset -= 1
    while offset >= 0 and text[offset] == "\\":
        backslashes += 1
        offset -= 1
    return backslashes % 2 == 1


def find_link_close(text: str, start: int) -> int | None:
    """Validate an optional Markdown title and return the outer close."""
    if start >= len(text):
        return None
    if text[start] == ")":
        return start
    if not text[start].isspace():
        return None

    position = start
    while position < len(text) and text[position].isspace():
        position += 1
    if position >= len(text):
        return None
    if text[position] == ")":
        return position

    opener = text[position]
    closer = {"\"": "\"", "'": "'", "(": ")"}.get(opener)
    if closer is None:
        return None
    position += 1
    while position < len(text):
        if text[position] == "\\":
            position += 2
            continue
        if text[position] == closer:
            position += 1
            break
        position += 1
    else:
        return None

    while position < len(text) and text[position].isspace():
        position += 1
    return position if position < len(text) and text[position] == ")" else None


def parse_inline_link_at(
    text: str, index: int
) -> tuple[MarkdownLink, int] | None:
    """Parse one inline link or image whose label opens at ``index``."""
    label_end = find_closing_bracket(text, index)
    if (
        label_end is None
        or label_end + 1 >= len(text)
        or text[label_end + 1] != "("
    ):
        return None

    position = label_end + 2
    while position < len(text) and text[position].isspace():
        position += 1
    if position >= len(text):
        return None

    if text[position] == "<":
        destination_start = position + 1
        position = destination_start
        while position < len(text):
            if text[position] in "\r\n":
                return None
            if text[position] == "\\":
                if (
                    position + 1 < len(text)
                    and text[position + 1] in "\r\n"
                ):
                    return None
                position += 2
                continue
            if text[position] == ">":
                break
            position += 1
        if position >= len(text):
            return None
        destination = text[destination_start:position]
        position += 1
    else:
        destination_start = position
        depth = 0
        while position < len(text):
            character = text[position]
            if character == "\\":
                position += 2
                continue
            if character in "<>":
                return None
            if character == "(":
                depth += 1
            elif character == ")":
                if depth == 0:
                    break
                depth -= 1
            elif character.isspace() and depth == 0:
                break
            position += 1
        destination = text[destination_start:position]

    closing = find_link_close(text, position)
    if closing is None:
        return None
    return (
        MarkdownLink(
            label=text[index + 1 : label_end],
            destination=destination,
            offset=index,
        ),
        closing,
    )


def extract_inline_links(text: str) -> list[MarkdownLink]:
    """Parse inline link and image destinations without a dependency."""
    links: list[MarkdownLink] = []
    search_from = 0
    while True:
        image_open = text.find("![", search_from)
        if image_open == -1:
            break
        if not is_escaped(text, image_open):
            parsed = parse_inline_link_at(text, image_open + 1)
            if parsed is not None:
                links.append(parsed[0])
        search_from = image_open + 2

    index = 0
    while index < len(text):
        if text[index] != "[" or is_escaped(text, index):
            index += 1
            continue
        if (
            index
            and text[index - 1] == "!"
            and not is_escaped(text, index - 1)
        ):
            index += 1
            continue
        parsed = parse_inline_link_at(text, index)
        if parsed is None:
            index += 1
            continue
        link, closing = parsed
        links.append(link)
        index = closing + 1
    return sorted(links, key=lambda link: link.offset)


def unescape_markdown_punctuation(text: str) -> str:
    """Remove backslashes that escape CommonMark's ASCII punctuation set."""
    return re.sub(
        rf"\\([{re.escape(string.punctuation)}])",
        r"\1",
        text,
    )


def normalize_reference_label(label: str) -> str:
    label = unescape_markdown_punctuation(label)
    return " ".join(label.split()).casefold()


def reference_definitions(text: str) -> dict[str, MarkdownLink]:
    """Return the first non-footnote definition for each reference label."""
    definitions: dict[str, MarkdownLink] = {}
    reference_text = mask_reference_container_prefixes(text)
    for match in REFERENCE_DEFINITION.finditer(reference_text):
        label = match.group("label")
        if label.lstrip().startswith("^"):
            continue
        destination = (
            match.group("angled_destination")
            if match.group("angled_destination") is not None
            else match.group("destination")
        )
        normalized = normalize_reference_label(label)
        definitions.setdefault(
            normalized,
            MarkdownLink(
                label=label,
                destination=destination,
                offset=match.start(),
            ),
        )
    return definitions


def extract_reference_links(
    text: str, definitions: dict[str, MarkdownLink]
) -> list[MarkdownLink]:
    """Resolve full, collapsed, and shortcut links through known definitions."""
    links: list[MarkdownLink] = []
    index = 0
    while index < len(text):
        if text[index] != "[" or is_escaped(text, index):
            index += 1
            continue
        if index and text[index - 1] == "!" and not is_escaped(text, index - 1):
            index += 1
            continue

        label_end = find_closing_bracket(text, index)
        if label_end is None:
            index += 1
            continue
        label = text[index + 1 : label_end]
        reference_label = label
        end = label_end + 1
        if end < len(text) and text[end] == "(":
            index = end + 1
            continue
        if end < len(text) and text[end] == "[":
            reference_end = find_closing_bracket(text, end)
            if reference_end is None:
                index = end + 1
                continue
            reference_label = text[end + 1 : reference_end] or label
            end = reference_end + 1
        elif end < len(text) and text[end] == ":":
            index = end + 1
            continue

        definition = definitions.get(normalize_reference_label(reference_label))
        if definition is not None:
            links.append(
                MarkdownLink(
                    label=label,
                    destination=definition.destination,
                    offset=definition.offset,
                )
            )
        index = end
    return links


def extract_resolved_links(
    text: str, definitions: dict[str, MarkdownLink]
) -> list[MarkdownLink]:
    links = extract_inline_links(text)
    links.extend(extract_reference_links(text, definitions))
    return links


def resolved_link_at(
    text: str, index: int, definitions: dict[str, MarkdownLink]
) -> MarkdownLink | None:
    """Resolve the single Markdown link immediately following ``index``."""
    while index < len(text) and text[index] in " \t":
        index += 1
    if index >= len(text) or text[index] != "[" or is_escaped(text, index):
        return None

    inline = parse_inline_link_at(text, index)
    if inline is not None:
        return inline[0]

    label_end = find_closing_bracket(text, index)
    if label_end is None:
        return None
    label = text[index + 1 : label_end]
    reference_label = label
    end = label_end + 1
    if end < len(text) and text[end] == "(":
        return None
    if end < len(text) and text[end] == "[":
        reference_end = find_closing_bracket(text, end)
        if reference_end is None:
            return None
        reference_label = text[end + 1 : reference_end] or label
    elif end < len(text) and text[end] == ":":
        return None

    definition = definitions.get(normalize_reference_label(reference_label))
    if definition is None:
        return None
    return MarkdownLink(
        label=label,
        destination=definition.destination,
        offset=index,
    )


def extract_markdown_links(text: str) -> list[MarkdownLink]:
    """Return inline links and reference-definition destinations."""
    links = extract_inline_links(text)
    reference_text = mask_reference_container_prefixes(text)
    for match in REFERENCE_DEFINITION.finditer(reference_text):
        if match.group("label").lstrip().startswith("^"):
            continue
        destination = (
            match.group("angled_destination")
            if match.group("angled_destination") is not None
            else match.group("destination")
        )
        links.append(
            MarkdownLink(
                label=match.group("label"),
                destination=destination,
                offset=match.start(),
            )
        )
    return sorted(links, key=lambda link: link.offset)


def markdown_sources(root: Path) -> list[Path]:
    sources = sorted((root / "docs").rglob("*.md"))
    agents = root / "AGENTS.md"
    if agents.is_file():
        sources.append(agents)
    return sorted(sources)


def split_destination(destination: str) -> tuple[str, str] | None:
    """Return decoded path/fragment for a relative destination, else None."""
    destination = html.unescape(unescape_markdown_punctuation(destination))
    try:
        parsed = urlsplit(destination)
    except ValueError:
        return None
    if parsed.scheme or parsed.netloc or destination.startswith(("/", "//")):
        return None
    return unquote(parsed.path), unquote(parsed.fragment)


def resolve_relative_target(
    root: Path, source: Path, destination: str
) -> tuple[Path, str] | None:
    parts = split_destination(destination)
    if parts is None:
        return None
    path_text, fragment = parts
    target = source if not path_text else source.parent / path_text
    return target.resolve(), fragment


def is_inside(root: Path, path: Path) -> bool:
    root = root.resolve()
    return path == root or root in path.parents


def render_link_labels(text: str) -> str:
    """Replace inline/reference link syntax with its balanced visible label."""
    rendered: list[str] = []
    copy_from = 0
    index = 0
    while index < len(text):
        image = text[index : index + 2] == "!["
        label_open = index + 1 if image else index
        if (
            text[label_open : label_open + 1] != "["
            or is_escaped(text, index)
        ):
            index += 1
            continue

        label_end = find_closing_bracket(text, label_open)
        if label_end is None:
            index += 1
            continue
        end = label_end + 1
        if end < len(text) and text[end] == "(":
            parsed = parse_inline_link_at(text, label_open)
            if parsed is None:
                index += 1
                continue
            link, closing = parsed
            end = closing + 1
        elif end < len(text) and text[end] == "[":
            reference_end = find_closing_bracket(text, end)
            if reference_end is None:
                index += 1
                continue
            link = MarkdownLink(
                label=text[label_open + 1 : label_end],
                destination="",
                offset=label_open,
            )
            end = reference_end + 1
        else:
            index += 1
            continue

        rendered.append(text[copy_from:index])
        rendered.append(link.label)
        copy_from = end
        index = end

    rendered.append(text[copy_from:])
    return "".join(rendered)


def protect_heading_code_spans(text: str) -> tuple[str, list[tuple[str, str]]]:
    """Replace code spans with placeholders and return their rendered text."""
    rendered: list[str] = []
    replacements: list[tuple[str, str]] = []
    copy_from = 0
    for start, end in inline_code_ranges(text):
        delimiter_length = 1
        while text[start + delimiter_length] == "`":
            delimiter_length += 1
        content = text[start + delimiter_length : end - delimiter_length]
        content = re.sub(r"[ \t\r\n]+", " ", content)
        if content.startswith(" ") and content.endswith(" ") and content.strip():
            content = content[1:-1]
        placeholder = f"\x00{len(replacements)}\x00"
        rendered.append(text[copy_from:start])
        rendered.append(placeholder)
        replacements.append((placeholder, content))
        copy_from = end
    rendered.append(text[copy_from:])
    return "".join(rendered), replacements


def render_heading_text(text: str) -> str:
    text, code_spans = protect_heading_code_spans(text)
    text = html.unescape(text)
    text = render_link_labels(text)
    text = AUTOLINK.sub(r"\1", text)
    text = re.sub(r"<[^>]*>", "", text)
    text = re.sub(r"\\(.)", r"\1", text)
    for placeholder, content in code_spans:
        text = text.replace(placeholder, content)
    return text


def github_slug(text: str) -> str:
    """Approximate GitHub's documented heading-id transformation."""
    rendered = render_heading_text(text).lower()
    characters: list[str] = []
    for character in rendered:
        if character in "-_":
            characters.append(character)
            continue
        category = unicodedata.category(character)
        if character in string.punctuation or category.startswith(("P", "C")):
            continue
        characters.append("-" if character.isspace() else character)
    return "".join(characters)


def strip_heading_containers(line: str) -> str:
    """Remove block-quote/list prefixes that can contain a Markdown heading."""
    while True:
        match = HEADING_CONTAINER.match(line)
        if match is None:
            return line
        line = line[match.end() :]


@lru_cache(maxsize=None)
def heading_anchors(path: Path) -> frozenset[str]:
    original = path.read_text(encoding="utf-8")
    literal_text = mask_html_comments(mask_fenced_code(original))
    anchor_text = mask_raw_text_html_blocks(
        mask_inline_code(mask_indented_code(literal_text))
    )
    explicit_anchors = {
        html.unescape(
            next(value for value in match.groups() if value is not None)
        )
        for match in EXPLICIT_ANCHOR.finditer(anchor_text)
    }
    text = mask_indented_code(mask_raw_html_blocks(literal_text))
    lines = [strip_heading_containers(line) for line in text.splitlines()]
    headings: list[str] = []
    for index, line in enumerate(lines):
        match = ATX_HEADING.match(line)
        if match:
            heading = re.sub(r"[ \t]+#+[ \t]*$", "", match.group(2)).strip()
            headings.append(heading)
        elif index and SETEXT_HEADING.match(line) and lines[index - 1].strip():
            headings.append(lines[index - 1].strip())

    used: set[str] = set()
    anchors = explicit_anchors
    for heading in headings:
        base = github_slug(heading)
        candidate = base
        suffix = 1
        while candidate in used:
            candidate = f"{base}-{suffix}"
            suffix += 1
        used.add(candidate)
        anchors.add(candidate)
    return frozenset(anchors)


def split_table_row(line: str) -> list[str]:
    """Split one GFM table row while respecting escapes and code spans."""
    content = line.strip()
    if content.startswith("|"):
        content = content[1:]
    if content.endswith("|"):
        content = content[:-1]
    cells: list[str] = []
    current: list[str] = []
    code_delimiter = 0
    index = 0
    while index < len(content):
        character = content[index]
        if character == "\\" and index + 1 < len(content):
            current.extend(content[index : index + 2])
            index += 2
            continue
        if character == "`":
            run_end = index
            while run_end < len(content) and content[run_end] == "`":
                run_end += 1
            run_length = run_end - index
            if code_delimiter == 0:
                code_delimiter = run_length
            elif code_delimiter == run_length:
                code_delimiter = 0
            current.extend(content[index:run_end])
            index = run_end
            continue
        if character == "|" and code_delimiter == 0:
            cells.append("".join(current).strip())
            current = []
        else:
            current.append(character)
        index += 1
    cells.append("".join(current).strip())
    return cells


def named_tests(
    enforcement: str, definitions: dict[str, MarkdownLink]
) -> list[tuple[str, MarkdownLink]]:
    """Extract contextually identified, file-bound tests from an Enforcement cell."""
    found: set[tuple[str, str]] = set()
    bindings: list[tuple[str, MarkdownLink]] = []

    for match in TEST_GROUP.finditer(enforcement):
        linked = resolved_link_at(enforcement, match.end(), definitions)
        if linked is None:
            continue
        adjusted = MarkdownLink(
            label=linked.label,
            destination=linked.destination,
            offset=linked.offset,
        )
        for name in re.findall(r"`([A-Za-z_][A-Za-z0-9_:]*)`", match.group("names")):
            key = (name, adjusted.destination)
            if key not in found:
                found.add(key)
                bindings.append((name, adjusted))

    for match in NATURAL_TEST_BINDING.finditer(enforcement):
        linked = resolved_link_at(enforcement, match.end(), definitions)
        if linked is None:
            continue
        adjusted = MarkdownLink(
            label=linked.label,
            destination=linked.destination,
            offset=linked.offset,
        )
        name = match.group("before") or match.group("after")
        key = (name, adjusted.destination)
        if key not in found:
            found.add(key)
            bindings.append((name, adjusted))
    return bindings


def check_invariant_citations(
    root: Path,
) -> tuple[list[Violation], set[tuple[int, str]]]:
    source = root / INVARIANTS
    text = mask_block_content(source.read_text(encoding="utf-8"))
    definitions = reference_definitions(text)
    violations: list[Violation] = []
    enforcement_links: set[tuple[int, str]] = set()

    for number, line in enumerate(text.splitlines(), start=1):
        if not re.match(r"^\|[ \t]*INV-[0-9]{3}[ \t]*\|", line):
            continue
        cells = split_table_row(line)
        if len(cells) != 5:
            violations.append(
                Violation(
                    INVARIANTS.as_posix(),
                    number,
                    "invariant-citation",
                    f"expected 5 table cells, found {len(cells)}",
                )
            )
            continue
        invariant = cells[0]
        enforcement = cells[4]
        citation_enforcement = mask_inline_code(enforcement)
        for link in extract_resolved_links(citation_enforcement, definitions):
            resolved = resolve_relative_target(root, source, link.destination)
            if resolved is None:
                continue
            target, _ = resolved
            enforcement_links.add((number, link.destination))
            enforcement_links.add(
                (line_number(text, link.offset), link.destination)
            )
            if not is_inside(root, target):
                violations.append(
                    Violation(
                        INVARIANTS.as_posix(),
                        number,
                        "invariant-citation",
                        f"{invariant} citation escapes the repository: "
                        f"`{link.destination}`",
                    )
                )
            elif not target.is_file():
                violations.append(
                    Violation(
                        INVARIANTS.as_posix(),
                        number,
                        "invariant-citation",
                        f"{invariant} cited file does not exist: "
                        f"`{link.destination}`",
                    )
                )

        for test_name, link in named_tests(enforcement, definitions):
            resolved = resolve_relative_target(root, source, link.destination)
            if resolved is None:
                continue
            target, _ = resolved
            if not is_inside(root, target) or not target.is_file():
                continue
            terminal_name = test_name.rsplit("::", 1)[-1]
            target_text = target.read_text(encoding="utf-8", errors="replace")
            if not re.search(rf"\b{re.escape(terminal_name)}\b", target_text):
                violations.append(
                    Violation(
                        INVARIANTS.as_posix(),
                        number,
                        "invariant-test",
                        f"{invariant} names test `{test_name}`, but "
                        f"`{repository_path(root, target)}` does not contain "
                        f"`{terminal_name}`",
                    )
                )

    return violations, enforcement_links


def check_relative_links(
    root: Path, enforcement_links: set[tuple[int, str]]
) -> list[Violation]:
    violations: list[Violation] = []
    invariant_path = (root / INVARIANTS).resolve()

    for source in markdown_sources(root):
        original = source.read_text(encoding="utf-8")
        parsed_text = mask_inline_code(mask_block_content(original))
        for link in extract_markdown_links(parsed_text):
            line = line_number(original, link.offset)
            resolved = resolve_relative_target(root, source, link.destination)
            if resolved is None:
                continue
            target, fragment = resolved
            enforcement_citation = (
                source.resolve() == invariant_path
                and (line, link.destination) in enforcement_links
            )
            if enforcement_citation and (
                not is_inside(root, target) or not target.is_file()
            ):
                continue
            if enforcement_citation and not fragment:
                continue
            source_label = repository_path(root, source)
            if not is_inside(root, target):
                violations.append(
                    Violation(
                        source_label,
                        line,
                        "relative-link",
                        f"target escapes the repository: `{link.destination}`",
                    )
                )
                continue
            if not target.exists():
                violations.append(
                    Violation(
                        source_label,
                        line,
                        "relative-link",
                        f"target does not exist: `{link.destination}`",
                    )
                )
                continue
            if not fragment:
                continue
            anchor_target = target
            if target.is_dir() and (target / "README.md").is_file():
                anchor_target = target / "README.md"
            if anchor_target.is_file() and anchor_target.suffix.lower() in {
                ".md",
                ".markdown",
            }:
                if fragment not in heading_anchors(anchor_target):
                    violations.append(
                        Violation(
                            source_label,
                            line,
                            "relative-link",
                            f"anchor `#{fragment}` does not exist in "
                            f"`{repository_path(root, anchor_target)}`",
                        )
                    )
                continue
            line_anchor = re.fullmatch(r"L([1-9][0-9]*)(?:-L([1-9][0-9]*))?", fragment)
            if target.is_file() and line_anchor:
                first = int(line_anchor.group(1))
                last = int(line_anchor.group(2) or first)
                available = len(
                    target.read_text(encoding="utf-8", errors="replace").splitlines()
                )
                if first <= last <= available:
                    continue
            violations.append(
                Violation(
                    source_label,
                    line,
                    "relative-link",
                    f"anchor `#{fragment}` does not resolve in "
                    f"`{repository_path(root, target)}`",
                )
            )
    return violations


def check_decision_order(root: Path) -> list[Violation]:
    source = root / DECISIONS
    text = mask_block_content(source.read_text(encoding="utf-8"))
    lines = text.splitlines()
    violations: list[Violation] = []
    previous: tuple[date, int] | None = None
    entries = 0

    for number, line in enumerate(lines, start=1):
        if (
            number < len(lines)
            and line.strip()
            and ATX_HEADING.match(line) is None
            and LIST_ITEM.match(line) is None
            and BLOCK_QUOTE_CONTAINER.match(line) is None
            and re.match(r"^ {0,3}-+[ \t]*$", lines[number])
        ):
            entries += 1
            violations.append(
                Violation(
                    DECISIONS.as_posix(),
                    number,
                    "decision-order",
                    "entry heading must be `## YYYY-MM-DD — <title>`; "
                    "Setext H2 headings are not permitted",
                )
            )
            continue
        if LIST_WRAPPED_ENTRY_HEADING.match(line):
            entries += 1
            violations.append(
                Violation(
                    DECISIONS.as_posix(),
                    number,
                    "decision-order",
                    "entry heading must be `## YYYY-MM-DD — <title>`; H2 "
                    "headings nested inside a list are not permitted",
                )
            )
            continue
        heading = DECISION_ENTRY_HEADING.match(line)
        if heading is None:
            continue
        entries += 1
        title = heading.group("title").strip()
        match = DECISION_HEADING.fullmatch(title)
        if not match:
            violations.append(
                Violation(
                    DECISIONS.as_posix(),
                    number,
                    "decision-order",
                    "entry heading must be `## YYYY-MM-DD — <title>`",
                )
            )
            continue
        try:
            entry_date = date.fromisoformat(match.group(1))
        except ValueError:
            violations.append(
                Violation(
                    DECISIONS.as_posix(),
                    number,
                    "decision-order",
                    f"invalid ISO date `{match.group(1)}`",
                )
            )
            continue
        if previous is not None and entry_date > previous[0]:
            violations.append(
                Violation(
                    DECISIONS.as_posix(),
                    number,
                    "decision-order",
                    f"entry date {entry_date.isoformat()} is newer than the "
                    f"preceding {previous[0].isoformat()} entry at line "
                    f"{previous[1]}",
                )
            )
        previous = (entry_date, number)

    if entries == 0:
        violations.append(
            Violation(
                DECISIONS.as_posix(),
                1,
                "decision-order",
                "no dated decision entries found",
            )
        )
    return violations


def verification_is_negated(text: str, offset: int) -> bool:
    """Recognize nearby plain-language negation of ``verified``.

    Emphasis delimiters are removed first so that rendered negations such as
    ``**not** verified`` read the same as their plain-text form.
    """
    preceding = EMPHASIS_DELIMITER.sub("", text[:offset])
    return VERIFICATION_NEGATION.search(preceding) is not None


def check_spec_verification_references(root: Path) -> list[Violation]:
    violations: list[Violation] = []
    specification_index = (root / SPEC_DIR / "README.md").resolve()
    for source in sorted((root / SPEC_DIR).rglob("*.md")):
        if source.resolve() == specification_index:
            continue
        text = mask_block_content(source.read_text(encoding="utf-8"))
        source_label = repository_path(root, source)
        code_ranges = inline_code_ranges(text)
        valid_reference = False

        for reference in VERIFICATION_LEAD.finditer(text):
            candidate_start = reference.start("pr")
            if offset_in_ranges(reference.start(), code_ranges) or offset_in_ranges(
                candidate_start, code_ranges
            ):
                continue
            if verification_is_negated(text, reference.start()):
                continue
            token = PR_TOKEN.match(text, candidate_start)
            if token is not None:
                valid_reference = True
                continue
            violations.append(
                Violation(
                    source_label,
                    line_number(text, candidate_start),
                    "spec-verification",
                    "verification reference must use "
                    "`PR #N (`branch-ref`)` with a positive decimal PR "
                    "number and a non-whitespace branch ref",
                )
            )

        if not valid_reference:
            violations.append(
                Violation(
                    source_label,
                    1,
                    "spec-verification",
                    "missing `verified ... PR #N (`branch-ref`)` reference",
                )
            )
    return violations


def run_checks(root: Path = ROOT) -> list[Violation]:
    root = root.resolve()
    heading_anchors.cache_clear()
    invariant_failures, enforcement_links = check_invariant_citations(root)
    failures = invariant_failures
    failures.extend(check_relative_links(root, enforcement_links))
    failures.extend(check_decision_order(root))
    failures.extend(check_spec_verification_references(root))
    return sorted(set(failures))


def main() -> int:
    failures = run_checks()
    if failures:
        print("docs-consistency check FAILED:")
        for failure in failures:
            print(f"  - {failure.render()}")
        print(
            "Repair the cited documentation or extend "
            "scripts/check_docs_consistency.py with the reviewed syntax."
        )
        return 1
    print("docs-consistency check passed")
    return 0


if __name__ == "__main__":
    sys.exit(main())
