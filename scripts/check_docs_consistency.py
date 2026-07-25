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
FENCE = re.compile(r"^ {0,3}(`{3,}|~{3,})")
REFERENCE_DEFINITION = re.compile(
    r"(?m)^ {0,3}\[([^\]\n]+)\]:[ \t]*(?:\r?\n[ \t]+)?"
    r"(?:<([^>\n]+)>|(\S+))"
)
DECISION_HEADING = re.compile(r"^(\d{4}-\d{2}-\d{2}) — (\S.*)$")
PR_TOKEN = re.compile(
    r"\bPR #([1-9][0-9]*)[ \t\r\n]+\("
    r"`([^\s`]+)`"
    r"\)"
)
VERIFICATION_LEAD = re.compile(
    r"\bverified\b"
    r"(?:(?![.!?](?:[ \t\r\n]|$)).)*?"
    r"\b(?:through|rooted[ \t]+at)[ \t\r\n]+"
    r"(?P<pr>\bPR[ \t]*#)",
    re.IGNORECASE | re.DOTALL,
)
TEST_GROUP = re.compile(
    r"\btests?[ \t]+"
    r"(?P<names>"
    r"`[A-Za-z_][A-Za-z0-9_:]*`"
    r"(?:[ \t]*(?:,[ \t]*(?:and[ \t]+)?|and[ \t]+)"
    r"`[A-Za-z_][A-Za-z0-9_:]*`)*"
    r")"
    r"[ \t]+in[ \t]+"
    r"(?P<link>\[[^\]\n]+\]\([^)]+\))",
    re.IGNORECASE,
)
TEST_IDENTIFIER = re.compile(r"^[A-Za-z_][A-Za-z0-9_:]*$")
NATURAL_TEST_BINDING = re.compile(
    r"(?:"
    r"`(?P<before>[A-Za-z_][A-Za-z0-9_:]*)`[ \t]+tests?\b"
    r"|"
    r"\btests?(?:[ \t]+named)?[ \t]+"
    r"`(?P<after>[A-Za-z_][A-Za-z0-9_:]*)`"
    r")"
    r"[ \t]+in[ \t]+"
    r"(?P<link>\[[^\]\n]+\]\([^)]+\))",
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


def mask_fenced_code(text: str) -> str:
    """Replace fenced code with spaces while preserving offsets and lines."""
    buffer = list(text)
    offset = 0
    fence_char: str | None = None
    fence_length = 0
    for line in text.splitlines(keepends=True):
        match = FENCE.match(line)
        if fence_char is None and match:
            fence_char = match.group(1)[0]
            fence_length = len(match.group(1))
            mask_range(buffer, offset, offset + len(line))
        elif fence_char is not None:
            mask_range(buffer, offset, offset + len(line))
            closing = re.match(
                rf"^ {{0,3}}{re.escape(fence_char)}{{{fence_length},}}[ \t]*"
                r"(?:\r?\n)?$",
                line,
            )
            if closing:
                fence_char = None
                fence_length = 0
        offset += len(line)
    return "".join(buffer)


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
        closing = text.find(delimiter, run_end)
        if closing == -1:
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


def find_link_close(text: str, start: int) -> int | None:
    """Find the outer close after a destination and optional Markdown title."""
    depth = 0
    quote: str | None = None
    index = start
    while index < len(text):
        character = text[index]
        if character == "\\":
            index += 2
            continue
        if quote is not None:
            if character == quote:
                quote = None
        elif character in "\"'":
            quote = character
        elif character == "(":
            depth += 1
        elif character == ")":
            if depth == 0:
                return index
            depth -= 1
        index += 1
    return None


def extract_inline_links(text: str) -> list[MarkdownLink]:
    """Parse inline Markdown link destinations without a Markdown dependency."""
    links: list[MarkdownLink] = []
    index = 0
    while index < len(text):
        if text[index] != "[" or (index and text[index - 1] == "\\"):
            index += 1
            continue
        label_end = find_closing_bracket(text, index)
        if label_end is None or label_end + 1 >= len(text):
            index += 1
            continue
        if text[label_end + 1] != "(":
            index = label_end + 1
            continue

        position = label_end + 2
        while position < len(text) and text[position].isspace():
            position += 1
        if position >= len(text):
            break

        if text[position] == "<":
            destination_start = position + 1
            position = destination_start
            while position < len(text):
                if text[position] == "\\":
                    position += 2
                    continue
                if text[position] == ">":
                    break
                position += 1
            if position >= len(text):
                index = label_end + 1
                continue
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
            index = label_end + 1
            continue
        links.append(
            MarkdownLink(
                label=text[index + 1 : label_end],
                destination=destination,
                offset=index,
            )
        )
        index = closing + 1
    return links


def extract_markdown_links(text: str) -> list[MarkdownLink]:
    """Return inline links and reference-definition destinations."""
    links = extract_inline_links(text)
    for match in REFERENCE_DEFINITION.finditer(text):
        if match.group(1).lstrip().startswith("^"):
            continue
        links.append(
            MarkdownLink(
                label=match.group(1),
                destination=match.group(2) or match.group(3),
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
    destination = re.sub(r"\\([\\`*{}\[\]()#+\-.!_>])", r"\1", destination)
    parsed = urlsplit(destination)
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


def render_heading_text(text: str) -> str:
    text = html.unescape(text)
    text = re.sub(r"`+([^`]*)`+", r"\1", text)
    text = re.sub(r"!?\[([^\]]*)\]\([^)]+\)", r"\1", text)
    text = re.sub(r"<[^>]*>", "", text)
    text = re.sub(r"\\(.)", r"\1", text)
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


@lru_cache(maxsize=None)
def heading_anchors(path: Path) -> frozenset[str]:
    text = mask_fenced_code(path.read_text(encoding="utf-8"))
    lines = text.splitlines()
    headings: list[str] = []
    for index, line in enumerate(lines):
        match = ATX_HEADING.match(line)
        if match:
            heading = re.sub(r"[ \t]+#+[ \t]*$", "", match.group(2)).strip()
            headings.append(heading)
        elif index and SETEXT_HEADING.match(line) and lines[index - 1].strip():
            headings.append(lines[index - 1].strip())

    used: set[str] = set()
    anchors: set[str] = set()
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


def named_tests(enforcement: str) -> list[tuple[str, MarkdownLink]]:
    """Extract explicitly file-bound test names from an Enforcement cell."""
    found: set[tuple[str, str]] = set()
    bindings: list[tuple[str, MarkdownLink]] = []

    for link in extract_inline_links(enforcement):
        raw_label = link.label.strip()
        label = raw_label[1:-1] if re.fullmatch(r"`[^`]+`", raw_label) else ""
        if TEST_IDENTIFIER.fullmatch(label) and "/" not in label:
            key = (label, link.destination)
            if key not in found:
                found.add(key)
                bindings.append((label, link))

    for match in TEST_GROUP.finditer(enforcement):
        links = extract_inline_links(match.group("link"))
        if len(links) != 1:
            continue
        linked = links[0]
        adjusted = MarkdownLink(
            label=linked.label,
            destination=linked.destination,
            offset=match.start("link") + linked.offset,
        )
        for name in re.findall(r"`([A-Za-z_][A-Za-z0-9_:]*)`", match.group("names")):
            key = (name, adjusted.destination)
            if key not in found:
                found.add(key)
                bindings.append((name, adjusted))

    for match in NATURAL_TEST_BINDING.finditer(enforcement):
        links = extract_inline_links(match.group("link"))
        if len(links) != 1:
            continue
        linked = links[0]
        adjusted = MarkdownLink(
            label=linked.label,
            destination=linked.destination,
            offset=match.start("link") + linked.offset,
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
    text = source.read_text(encoding="utf-8")
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
        for link in extract_inline_links(enforcement):
            resolved = resolve_relative_target(root, source, link.destination)
            if resolved is None:
                continue
            target, _ = resolved
            enforcement_links.add((number, link.destination))
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

        for test_name, link in named_tests(enforcement):
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
        parsed_text = mask_inline_code(mask_fenced_code(original))
        for link in extract_markdown_links(parsed_text):
            line = line_number(original, link.offset)
            resolved = resolve_relative_target(root, source, link.destination)
            if resolved is None:
                continue
            target, fragment = resolved
            if (
                source.resolve() == invariant_path
                and (line, link.destination) in enforcement_links
                and not fragment
            ):
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
            if target.is_file() and target.suffix.lower() in {".md", ".markdown"}:
                if fragment not in heading_anchors(target):
                    violations.append(
                        Violation(
                            source_label,
                            line,
                            "relative-link",
                            f"anchor `#{fragment}` does not exist in "
                            f"`{repository_path(root, target)}`",
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
    text = mask_fenced_code(source.read_text(encoding="utf-8"))
    violations: list[Violation] = []
    previous: tuple[date, int] | None = None
    entries = 0

    for number, line in enumerate(text.splitlines(), start=1):
        if not re.match(r"^##(?!#)(?:[ \t]|$)", line):
            continue
        entries += 1
        title = line[2:].strip()
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


def check_spec_verification_references(root: Path) -> list[Violation]:
    violations: list[Violation] = []
    for source in sorted((root / SPEC_DIR).rglob("*.md")):
        if source.name == "README.md":
            continue
        text = mask_fenced_code(source.read_text(encoding="utf-8"))
        source_label = repository_path(root, source)
        code_ranges = inline_code_ranges(text)
        valid_reference = False

        for reference in VERIFICATION_LEAD.finditer(text):
            candidate_start = reference.start("pr")
            if offset_in_ranges(reference.start(), code_ranges) or offset_in_ranges(
                candidate_start, code_ranges
            ):
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
