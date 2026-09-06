#!/usr/bin/env python3
"""Check mechanically verifiable links across the living-spec surface.

The check is deterministic and offline. It verifies:

1. every relative file citation in an invariant Enforcement cell resolves to a
   repository file; a code-spanned test name bound to a cited file must appear
   in that file, every ``INV-NNN``-tagged enforcement citation contains that
   tag, and every Rust test file carrying an INV tag is cited by that row,
2. every relative Markdown link in ``docs/**/*.md`` and the root ``AGENTS.md``
   resolves inside the repository, including GitHub-style heading fragments.
External links and semantic freshness beyond reachability are outside this
check. Run from any directory; exits nonzero with one stable line per
violation.

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
import os
import re
import string
import subprocess
import sys
import tomllib
import unicodedata
from dataclasses import dataclass
from functools import lru_cache
from pathlib import Path
from urllib.parse import unquote, urlsplit

import postgres_integration_suites

ROOT = Path(__file__).resolve().parent.parent
INVARIANTS = Path("docs/invariants.md")

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
BLOCK_QUOTE_PREFIX = r" {0,3}>[ \t]?"
BLOCK_QUOTE_CONTAINER = re.compile(rf"^{BLOCK_QUOTE_PREFIX}")
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
THEMATIC_BREAK = re.compile(
    r"^ {0,3}(?:(?:\*[ \t]*){3,}|(?:-[ \t]*){3,}|(?:_[ \t]*){3,})$"
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
INVARIANT_TAG = re.compile(
    r"(?<![^\W_])INV[-_]?(?P<number>[0-9]{3})(?![^\W_])",
    re.IGNORECASE,
)
def _unicode_mark_class() -> str:
    """Return a character class of the marks Rust's XID_Continue admits.

    Python's ``\\w`` covers XID_Start and the letters, digits, and connectors
    of XID_Continue but omits the combining marks, so decomposed identifiers
    such as ``cafe`` followed by U+0301 would otherwise end early. The class is
    derived from the interpreter's own Unicode data rather than transcribed.
    """
    marks = [
        code
        for code in range(0x300, sys.maxunicode + 1)
        if unicodedata.category(chr(code)) in ("Mn", "Mc")
    ]
    ranges: list[tuple[int, int]] = []
    for code in marks:
        if ranges and code == ranges[-1][1] + 1:
            ranges[-1] = (ranges[-1][0], code)
        else:
            ranges.append((code, code))
    return "".join(
        re.escape(chr(first))
        if first == last
        else f"{re.escape(chr(first))}-{re.escape(chr(last))}"
        for first, last in ranges
    )


RUST_IDENTIFIER_MARKS = _unicode_mark_class()
RUST_IDENTIFIER_PATTERN = (
    rf"(?:r#)?(?![0-9])[^\W{RUST_IDENTIFIER_MARKS}]"
    rf"[\w{RUST_IDENTIFIER_MARKS}]*"
)
RUST_ATTRIBUTE_OPEN = r"#[ \t\r\n]*\["
RUST_CHARACTER_LITERAL = re.compile(
    r"'(?:\\(?:x[0-9A-Fa-f]{2}|u\{[0-9A-Fa-f_]{1,6}\}|.)|[^\\\r\n])'"
)
RUST_TEST_DECLARATION = re.compile(
    r"(?P<prefix>(?:"
    r"^[ \t]*///[^\n]*(?:\n|$)"
    r"|^[ \t]*/\*\*(?:[^*]|\*(?!/))*\*/[ \t]*(?:\n|$)"
    rf"|^[ \t]*{RUST_ATTRIBUTE_OPEN}[^\]]*\][ \t]*(?:\n|$)"
    r"|^[ \t]*//(?!/)[^\n]*(?:\n|$)"
    r"|^[ \t]*/\*(?!\*)(?:[^*]|\*(?!/))*\*/[ \t]*(?:\n|$)"
    r"|^[ \t]*(?:\n|$)"
    # An attribute may also share the declaration's line, so this last
    # alternative ends the prefix without consuming a line break.
    rf"|^[ \t]*{RUST_ATTRIBUTE_OPEN}[^\]]*\][ \t]*"
    r")+)"
    r"[ \t]*(?:pub(?:\([^)]*\))?[ \t\r\n]+)?"
    r"(?:(?:const|async|unsafe)[ \t\r\n]+)*"
    r'(?:extern(?:[ \t\r\n]+"[^"\n]*")?[ \t\r\n]+)?'
    rf"fn[ \t\r\n]+(?P<name>{RUST_IDENTIFIER_PATTERN})",
    re.MULTILINE,
)
# Rust resolves an attribute path case-sensitively, so a distinct macro
# named `Test` is not the built-in attribute.
RUST_TEST_ATTRIBUTE = re.compile(
    rf"{RUST_ATTRIBUTE_OPEN}[ \t\r\n]*(?:::[ \t\r\n]*)?"
    rf"(?:{RUST_IDENTIFIER_PATTERN}[ \t\r\n]*::[ \t\r\n]*)*"
    r"(?:r#)?test(?=[ \t\r\n(\]])[^\]]*\]"
)
RUST_ATTRIBUTE = re.compile(
    rf"{RUST_ATTRIBUTE_OPEN}(?P<meta>[^\]]*)\]", re.DOTALL
)
RUST_CFG_META = re.compile(
    r"^cfg[ \t\r\n]*\((?P<body>.*)\)[ \t\r\n]*$", re.DOTALL
)
RUST_CFG_ATTR_META = re.compile(
    r"^cfg_attr[ \t\r\n]*\((?P<body>.*)\)[ \t\r\n]*$", re.DOTALL
)
RUST_CFG_PREDICATE = re.compile(
    r"^(?P<operator>all|any|not)[ \t\r\n]*"
    r"\((?P<body>.*)\)[ \t\r\n]*$",
    re.DOTALL,
)
RUST_CFG_VALUE = re.compile(
    r'^(?P<name>[A-Za-z_][A-Za-z0-9_]*)[ \t\r\n]*='
    r'[ \t\r\n]*"(?P<value>[^"\n]*)"$'
)
CI_CFG_NAMES = {
    "debug_assertions": True,
    "test": True,
    "unix": True,
    "windows": False,
}
CI_CFG_VALUES = {
    "panic": "unwind",
    "target_abi": "",
    "target_arch": "x86_64",
    "target_endian": "little",
    "target_env": "gnu",
    "target_family": "unix",
    "target_os": "linux",
    "target_pointer_width": "64",
    "target_vendor": "unknown",
}
RUST_TEST_META = re.compile(
    r"^(?:::[ \t\r\n]*)?"
    rf"(?:{RUST_IDENTIFIER_PATTERN}[ \t\r\n]*::[ \t\r\n]*)*"
    r"(?:r#)?test(?=[ \t\r\n(]|$)"
)
RUST_META_NAME = re.compile(
    rf"{RUST_IDENTIFIER_PATTERN}(?=[ \t\r\n(]|$)"
)
RUST_META_ITEM = re.compile(
    rf"(?:::)?(?:{RUST_IDENTIFIER_PATTERN}[ \t\r\n]*::[ \t\r\n]*)*"
    rf"{RUST_IDENTIFIER_PATTERN}"
)
RUST_USE_ITEM = re.compile(
    rf"(?P<attributes>(?:{RUST_ATTRIBUTE_OPEN}[^\]]*\][ \t\r\n]*)*)"
    r"(?P<visibility>\bpub(?:\([^)]*\))?[ \t\r\n]+)?"
    r"\buse\b(?P<body>[^;]*);",
    re.DOTALL,
)
RUST_TEST_ALIAS = re.compile(
    rf"\btest[ \t\r\n]+as[ \t\r\n]+(?P<alias>{RUST_IDENTIFIER_PATTERN})"
)
RUST_CRATE_IMPORT = re.compile(
    rf"\bcrate[ \t\r\n]*::[ \t\r\n]*(?P<name>{RUST_IDENTIFIER_PATTERN})"
    r"(?![ \t\r\n]*(?:::|as\b))"
)
RUST_CRATE_GROUP = re.compile(r"\bcrate[ \t\r\n]*::[ \t\r\n]*\{")
RUST_BARE_IDENTIFIER = re.compile(RUST_IDENTIFIER_PATTERN)
RUST_RAW_STRING_OPEN = re.compile(r'(?:br|rb|cr|r)(?P<hashes>#{0,255})"')
RUST_INLINE_MODULE = re.compile(
    rf"(?P<attributes>(?:{RUST_ATTRIBUTE_OPEN}[^\]]*\][ \t\r\n]*)*)"
    rf"\b(?:pub(?:\([^)]*\))?[ \t\r\n]+)?"
    rf"mod[ \t\r\n]+(?P<name>{RUST_IDENTIFIER_PATTERN})[ \t\r\n]*\{{"
)
RUST_OUT_OF_LINE_MODULE = re.compile(
    rf"(?P<attributes>(?:{RUST_ATTRIBUTE_OPEN}[^\]]*\][ \t\r\n]*)*)"
    rf"\b(?:pub(?:\([^)]*\))?[ \t\r\n]+)?"
    rf"mod[ \t\r\n]+(?P<name>{RUST_IDENTIFIER_PATTERN})[ \t\r\n]*;"
)
RUST_PATH_META = re.compile(
    r"^path[ \t\r\n]*=[ \t\r\n]*"
    r"(?:\"(?P<plain>[^\"\n]*)\"|r(?P<hashes>\#*)\"(?P<raw>.*?)\"(?P=hashes))$",
    re.DOTALL,
)
RUST_CRATE_ROOT_NAMES = ("mod.rs", "lib.rs", "main.rs")
RUST_MACRO_RULES = re.compile(
    rf"\bmacro_rules![ \t\r\n]*(?P<name>{RUST_IDENTIFIER_PATTERN})"
    r"[ \t\r\n]*(?P<opening>[\(\[\{])"
)
RUST_INCLUDE_OPEN = re.compile(
    rf"(?P<attributes>(?:{RUST_ATTRIBUTE_OPEN}[^\]]*\][ \t\r\n]*)*)"
    r"\binclude![ \t\r\n]*(?P<opening>[\(\[\{])"
)
RUST_STRING_LITERAL = re.compile(
    r"[ \t\r\n]*"
    r"(?:\"(?P<plain>[^\"\n]*)\"|r(?P<hashes>\#*)\"(?P<raw>.*?)\"(?P=hashes))",
    re.DOTALL,
)
RUST_PROC_MACRO_ATTRIBUTE = re.compile(
    rf"{RUST_ATTRIBUTE_OPEN}[ \t\r\n]*proc_macro(?:_attribute|_derive)?"
    r"(?=[ \t\r\n(\]])[^\]]*\]"
)
RUST_MACRO_INVOCATION = re.compile(
    rf"\b(?P<name>{RUST_IDENTIFIER_PATTERN})![ \t\r\n]*(?P<opening>[\(\[\{{])"
)
RUST_FORWARDED_ATTRIBUTE = re.compile(
    rf"{RUST_ATTRIBUTE_OPEN}[^\]]*\$[^\]]*\]", re.DOTALL
)
RUST_METAVARIABLE = re.compile(rf"\$(?P<name>{RUST_IDENTIFIER_PATTERN})")
RUST_METAVARIABLE_BINDING = re.compile(
    rf"\$(?P<name>{RUST_IDENTIFIER_PATTERN})[ \t\r\n]*:"
    rf"[ \t\r\n]*{RUST_IDENTIFIER_PATTERN}"
)
RUST_METAVARIABLE_REPETITION = re.compile(r"\$[ \t\r\n]*[\(\[\{]")
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
class InlineModule:
    """One brace-delimited `mod name { ... }` and its build state."""

    opening: int
    closing: int
    name: str
    disabled: bool
    ci_disabled: bool


@dataclass(frozen=True)
class ScopedTestAlias:
    """One local `test` attribute name and the block that can see it."""

    opening: int
    closing: int
    name: str


@dataclass(eq=False)
class RustSource:
    """One repository Rust file, read and lexically prepared once."""

    path: Path
    label: str
    text: str
    code: str
    delimiters: dict[int, int]
    invocations: dict[str, list[int]]
    aliases: list[ScopedTestAlias]
    module_prefixes: tuple[tuple[str, ...], ...]
    ignored_test_selections: tuple[IgnoredTestSelection, ...]


@dataclass(frozen=True)
class MarkdownLink:
    """One parsed inline link or reference-definition destination."""

    label: str
    destination: str
    offset: int
    definition_offset: int | None = None


class TrackedFilesError(RuntimeError):
    """Git could not provide a trustworthy tracked-file inventory."""


def tracked_files(root: Path) -> list[Path]:
    """Return tracked files beneath ``root`` using the repository index.

    Git is the authority for the validator input domain. A missing executable,
    non-repository directory, failed inventory, or empty inventory is a hard
    error so validation can never pass after examining no source files.
    """
    requested_root = root.resolve()
    try:
        repository = subprocess.run(
            ["git", "rev-parse", "--show-toplevel"],
            cwd=requested_root,
            check=False,
            capture_output=True,
            text=True,
        )
    except OSError as error:
        raise TrackedFilesError(
            f"Git tracked-file discovery is unavailable: {error}"
        ) from error
    if repository.returncode != 0 or not repository.stdout.strip():
        raise TrackedFilesError(
            "Git tracked-file discovery failed: the requested path is not a Git repository"
        )

    repository_root = Path(repository.stdout.strip()).resolve()
    try:
        inventory = subprocess.run(
            ["git", "ls-files", "--full-name", "-z"],
            cwd=repository_root,
            check=False,
            capture_output=True,
            text=True,
        )
    except OSError as error:
        raise TrackedFilesError(
            f"Git tracked-file discovery is unavailable: {error}"
        ) from error
    if inventory.returncode != 0:
        detail = inventory.stderr.strip() or "git ls-files failed"
        raise TrackedFilesError(f"Git tracked-file discovery failed: {detail}")
    labels = [label for label in inventory.stdout.split("\0") if label]
    if not labels:
        raise TrackedFilesError("Git tracked-file discovery returned no files")

    files: list[Path] = []
    for label in labels:
        path = (repository_root / label).resolve()
        try:
            path.relative_to(requested_root)
        except ValueError:
            continue
        if path.is_file():
            files.append(path)
    return sorted(files)


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


def rust_block_comment_end(text: str, start: int) -> int:
    """Return the end of one possibly nested Rust block comment."""
    end = start + 2
    depth = 1
    while end < len(text) and depth:
        if text.startswith("/*", end):
            depth += 1
            end += 2
        elif text.startswith("*/", end):
            depth -= 1
            end += 2
        else:
            end += 1
    return end


def rust_outer_line_doc_at(text: str, index: int) -> bool:
    """Return whether ``index`` begins an outer line documentation comment."""
    return text.startswith("///", index) and not text.startswith("////", index)


def rust_outer_block_doc_at(text: str, index: int) -> bool:
    """Return whether ``index`` begins an outer block documentation comment."""
    return text.startswith("/**", index) and not text.startswith("/***", index)


def mask_rust_non_code(
    text: str,
    *,
    preserve_doc_comments: bool = False,
    preserve_literals: bool = False,
) -> str:
    """Mask Rust comments and literals while preserving offsets and newlines.

    ``preserve_literals`` keeps string and character literals readable while
    still removing comments, which is what a caller inspecting what a macro
    writes into its output needs.
    """
    buffer = list(text)
    index = 0
    while index < len(text):
        if text.startswith("//", index):
            end = text.find("\n", index + 2)
            end = len(text) if end == -1 else end
            if (
                not preserve_doc_comments
                or not rust_outer_line_doc_at(text, index)
            ):
                mask_range(buffer, index, end)
            index = end
            continue
        if text.startswith("/*", index):
            end = rust_block_comment_end(text, index)
            if (
                not preserve_doc_comments
                or not rust_outer_block_doc_at(text, index)
            ):
                mask_range(buffer, index, end)
            index = end
            continue

        token_boundary = index == 0 or not (
            text[index - 1].isalnum() or text[index - 1] == "_"
        )
        raw = RUST_RAW_STRING_OPEN.match(text, index) if token_boundary else None
        if raw is not None:
            closer = '"' + raw.group("hashes")
            end = text.find(closer, raw.end())
            end = len(text) if end == -1 else end + len(closer)
            if not preserve_literals:
                mask_range(buffer, index, end)
            index = end
            continue

        quote_start = index
        if token_boundary and text.startswith(("b\"", "c\""), index):
            quote_start += 1
        if text[quote_start : quote_start + 1] == '"':
            end = quote_start + 1
            while end < len(text):
                if text[end] == "\\":
                    end = min(end + 2, len(text))
                elif text[end] == '"':
                    end += 1
                    break
                else:
                    end += 1
            if not preserve_literals:
                mask_range(buffer, index, end)
            index = end
            continue

        char_start = index + 1 if text.startswith("b'", index) else index
        if token_boundary and text[char_start : char_start + 1] == "'":
            # A quote also opens a lifetime or a loop label, which must not
            # swallow the source up to the next unrelated quote, so only a
            # closer in character-literal position ends the token.
            end = RUST_CHARACTER_LITERAL.match(text, char_start)
            if end is None:
                index += 1
                continue
            if not preserve_literals:
                mask_range(buffer, index, end.end())
            index = end.end()
            continue

        index += 1
    return "".join(buffer)


def rust_comment_text(text: str) -> str:
    """Return only Rust comments while preserving source offsets and lines."""
    without_comments = mask_rust_non_code(text, preserve_literals=True)
    comments = ["\n" if character == "\n" else " " for character in text]
    for index, (character, visible) in enumerate(
        zip(text, without_comments, strict=True)
    ):
        if character not in " \t\r\n" and visible == " ":
            comments[index] = character
    return "".join(comments)


def rust_doc_comments(prefix: str) -> list[str]:
    """Return attached outer Rust doc comments, including nested block docs."""
    metadata = mask_rust_non_code(prefix, preserve_doc_comments=True)
    comments: list[str] = []
    index = 0
    while index < len(metadata):
        line_start = metadata.rfind("\n", 0, index) + 1
        at_line_indent = not metadata[line_start:index].strip()
        if at_line_indent and rust_outer_line_doc_at(metadata, index):
            end = metadata.find("\n", index + 3)
            end = len(metadata) if end == -1 else end
            comments.append(prefix[index:end])
            index = end
            continue
        if at_line_indent and rust_outer_block_doc_at(metadata, index):
            end = rust_block_comment_end(metadata, index)
            comments.append(prefix[index:end])
            index = end
            continue
        index += 1
    return comments


def split_rust_meta_items(body: str) -> list[str]:
    """Split one attribute argument list at top-level commas."""
    items: list[str] = []
    start = 0
    depth = 0
    for index, character in enumerate(body):
        if character in "([{":
            depth += 1
        elif character in ")]}" and depth:
            depth -= 1
        elif character == "," and depth == 0:
            items.append(body[start:index])
            start = index + 1
    items.append(body[start:])
    return items


def rust_cfg_truth(meta: str) -> bool | None:
    """Evaluate cfg predicates against the repository's Ubuntu CI harness.

    The generated catalog names tests that can fail in authoritative CI, whose
    Rust jobs use Ubuntu x86-64 and all features. Unknown custom predicates stay
    unknown; combinators can still settle a result without guessing them.
    """
    meta = meta.strip()
    if meta in CI_CFG_NAMES:
        return CI_CFG_NAMES[meta]
    value = RUST_CFG_VALUE.fullmatch(meta)
    if value is not None:
        if value.group("name") == "feature":
            return True
        expected = CI_CFG_VALUES.get(value.group("name"))
        return None if expected is None else value.group("value") == expected
    predicate = RUST_CFG_PREDICATE.fullmatch(meta)
    if predicate is None:
        return None
    body = predicate.group("body")
    items = [] if not body.strip() else split_rust_meta_items(body)
    values = [rust_cfg_truth(item) for item in items]
    operator = predicate.group("operator")
    if operator == "not":
        if len(values) != 1 or values[0] is None:
            return None
        return not values[0]
    if operator == "all":
        if False in values:
            return False
        if all(value is True for value in values):
            return True
        return None
    if True in values:
        return True
    if all(value is False for value in values):
        return False
    return None


def rust_exported_test_aliases(code: str) -> frozenset[str]:
    """Return the names this file re-exports a `test` attribute under.

    Only a `pub use` rename is reachable as `crate::<name>` from another
    module of the same crate, which is the one import shape resolved here.
    """
    return frozenset(
        alias.group("alias")
        for item in RUST_USE_ITEM.finditer(code)
        if item.group("visibility") is not None
        and not rust_item_is_disabled(item.group("attributes"))
        for alias in RUST_TEST_ALIAS.finditer(item.group("body"))
    )


def rust_crate_import_names(body: str) -> set[str]:
    """Return the names one `use` body imports directly from the crate root.

    Both `use crate::name;` and a `use crate::{...}` group count, and in the
    group only an item that is exactly one identifier — never a qualified
    path, a rename, or a nested group — names the root's own item.
    """
    names = {match.group("name") for match in RUST_CRATE_IMPORT.finditer(body)}
    delimiters = rust_matching_delimiters(body)
    for group in RUST_CRATE_GROUP.finditer(body):
        opening = group.end() - 1
        closing = delimiters.get(opening)
        if closing is None:
            continue
        for item in split_rust_meta_items(body[opening + 1 : closing]):
            if RUST_BARE_IDENTIFIER.fullmatch(item.strip()) is not None:
                names.add(item.strip())
    return names


def rust_test_attribute_aliases(
    code: str, exported: frozenset[str] = frozenset()
) -> list[ScopedTestAlias]:
    """Return each local name a `use` item gives an imported `test` attribute.

    ``use tokio::test as async_test;`` registers tests under `#[async_test]`.
    An alias reaches only the block its `use` item sits in, and everything
    nested inside it, so a sibling module may bind the same local name to
    something else without being read as a test. A `use crate::<name>;` whose
    name this file's own crate root re-exports as a `test` attribute carries
    that reading in, which is how a re-exported alias arrives; any longer or
    renamed path is left alone, since its target is not this resolution's to
    decide.
    """
    delimiters = rust_matching_delimiters(code)
    blocks = sorted(
        (opening, closing)
        for opening, closing in delimiters.items()
        if code[opening] == "{"
    )
    aliases: list[ScopedTestAlias] = []
    for item in RUST_USE_ITEM.finditer(code):
        if rust_item_is_disabled(item.group("attributes")):
            continue
        enclosing = [
            (opening, closing)
            for opening, closing in blocks
            if opening < item.start() < closing
        ]
        opening, closing = (
            max(enclosing) if enclosing else (-1, len(code))
        )
        body = item.group("body")
        names = {
            alias.group("alias")
            for alias in RUST_TEST_ALIAS.finditer(body)
        }
        names.update(
            name
            for name in rust_crate_import_names(body)
            if name in exported
        )
        for name in sorted(names):
            aliases.append(ScopedTestAlias(opening, closing, name))
    return aliases


def rust_visible_test_aliases(
    aliases: list[ScopedTestAlias], offset: int
) -> frozenset[str]:
    """Return the alias names in scope at one declaration offset."""
    return frozenset(
        alias.name
        for alias in aliases
        if alias.opening < offset < alias.closing
    )


def rust_meta_names_test(meta: str, aliases: frozenset[str]) -> bool:
    """Return whether one meta names `test` by path or by an imported alias."""
    if RUST_TEST_META.match(meta) is not None:
        return True
    name = RUST_META_NAME.match(meta)
    return name is not None and name.group(0) in aliases


def rust_meta_applies_test(
    meta: str, aliases: frozenset[str] = frozenset()
) -> bool:
    """Return whether one direct or recursively conditional meta is `test`."""
    meta = meta.strip()
    if rust_meta_names_test(meta, aliases):
        return True
    cfg_attr = RUST_CFG_ATTR_META.fullmatch(meta)
    if cfg_attr is None:
        return False
    items = split_rust_meta_items(cfg_attr.group("body"))
    if not items or rust_cfg_truth(items[0]) is not True:
        return False
    return any(rust_meta_applies_test(item, aliases) for item in items[1:])


def rust_top_level_meta_items(text: str) -> list[str]:
    """Return the path-headed meta items appearing at the top nesting level.

    A matcher whose argument separators cannot be attributed leaves the whole
    token sequence as one candidate, so each top-level item is offered on its
    own; tokens nested inside a delimited group belong to their own item and
    are never lifted out of it.
    """
    delimiters = rust_matching_delimiters(text)
    items: list[str] = []
    index = 0
    while index < len(text):
        if text[index] == "#":
            probe = index + 1
            while probe < len(text) and text[probe] in " \t\r\n":
                probe += 1
            closing = (
                delimiters.get(probe)
                if probe < len(text) and text[probe] == "["
                else None
            )
            if closing is None:
                index += 1
                continue
            # An attribute group's contents are metadata in their own right,
            # so a `#[test]` written into an invocation is read, not skipped.
            items.extend(
                rust_top_level_meta_items(text[probe + 1 : closing])
            )
            index = closing + 1
            continue
        if text[index] in "([{":
            closing = delimiters.get(index)
            index = len(text) if closing is None else closing + 1
            continue
        match = RUST_META_ITEM.match(text, index)
        if match is None:
            index += 1
            continue
        end = match.end()
        probe = end
        while probe < len(text) and text[probe] in " \t\r\n":
            probe += 1
        if probe < len(text) and text[probe] in "([{":
            closing = delimiters.get(probe)
            if closing is not None:
                end = closing + 1
        items.append(text[match.start() : end])
        index = end
    return items


def rust_raw_attributes(text: str, masked: str) -> str:
    """Return raw attached attributes found in the comment-masked prefix."""
    return "\n".join(
        text[attribute.start() : attribute.end()]
        for attribute in RUST_ATTRIBUTE.finditer(masked)
    )


def rust_attributes_apply_test(
    prefix: str, aliases: frozenset[str] = frozenset()
) -> bool:
    """Return whether an attached attribute applies a test attribute."""
    for attribute in RUST_ATTRIBUTE.finditer(prefix):
        if rust_meta_applies_test(attribute.group("meta"), aliases):
            return True
    return False


def rust_meta_module_paths(meta: str) -> list[str]:
    """Return the module files one meta names, directly or under `cfg_attr`.

    The subject is the fixed CI test build, so known target predicates select
    their exact path and a truly unknown condition contributes candidates
    conservatively rather than letting one shadow the rest.
    """
    meta = meta.strip()
    direct = RUST_PATH_META.fullmatch(meta)
    if direct is not None:
        plain = direct.group("plain")
        return [direct.group("raw") if plain is None else plain]
    cfg_attr = RUST_CFG_ATTR_META.fullmatch(meta)
    if cfg_attr is None:
        return []
    items = split_rust_meta_items(cfg_attr.group("body"))
    if not items or rust_cfg_truth(items[0]) is False:
        return []
    found: list[str] = []
    for item in items[1:]:
        found.extend(rust_meta_module_paths(item))
    return found


def rust_module_path_attributes(attributes: str) -> list[str]:
    """Return every module file one attached attribute run may name."""
    found: list[str] = []
    for attribute in RUST_ATTRIBUTE.finditer(attributes):
        for path in rust_meta_module_paths(attribute.group("meta")):
            if path not in found:
                found.append(path)
    return found


def rust_meta_disables_item(meta: str) -> bool:
    """Return whether one active meta universally disables its item."""
    meta = meta.strip()
    cfg = RUST_CFG_META.fullmatch(meta)
    if cfg is not None:
        return rust_cfg_truth(cfg.group("body")) is False
    cfg_attr = RUST_CFG_ATTR_META.fullmatch(meta)
    if cfg_attr is None:
        return False
    items = split_rust_meta_items(cfg_attr.group("body"))
    if not items or rust_cfg_truth(items[0]) is not True:
        return False
    return any(rust_meta_disables_item(item) for item in items[1:])


def rust_item_is_disabled(prefix: str) -> bool:
    """Return whether attached cfg metadata disables an item in every build."""
    for attribute in RUST_ATTRIBUTE.finditer(prefix):
        if rust_meta_disables_item(attribute.group("meta")):
            return True
    return False


def rust_meta_disables_ci_item(meta: str) -> bool:
    """Return whether metadata can exclude an item from the fixed CI target."""
    meta = meta.strip()
    cfg = RUST_CFG_META.fullmatch(meta)
    if cfg is not None:
        return rust_cfg_truth(cfg.group("body")) is not True
    cfg_attr = RUST_CFG_ATTR_META.fullmatch(meta)
    if cfg_attr is None:
        return False
    items = split_rust_meta_items(cfg_attr.group("body"))
    condition = None if not items else rust_cfg_truth(items[0])
    if condition is False:
        return False
    return any(rust_meta_disables_ci_item(item) for item in items[1:])


def rust_item_is_disabled_in_ci(prefix: str) -> bool:
    """Return whether attached cfg metadata can remove an item from CI."""
    return any(
        rust_meta_disables_ci_item(attribute.group("meta"))
        for attribute in RUST_ATTRIBUTE.finditer(prefix)
    )


def rust_meta_may_ignore_test(meta: str) -> bool:
    """Return whether active or build-dependent metadata may ignore a test."""
    meta = meta.strip()
    if re.fullmatch(r"ignore(?:[ \t\r\n]*=.*)?", meta, re.DOTALL):
        return True
    cfg_attr = RUST_CFG_ATTR_META.fullmatch(meta)
    if cfg_attr is None:
        return False
    items = split_rust_meta_items(cfg_attr.group("body"))
    if not items or rust_cfg_truth(items[0]) is False:
        return False
    return any(rust_meta_may_ignore_test(item) for item in items[1:])


def rust_item_may_be_ignored(prefix: str) -> bool:
    """Return whether attached metadata can exclude a test from an ordinary run."""
    return any(
        rust_meta_may_ignore_test(attribute.group("meta"))
        for attribute in RUST_ATTRIBUTE.finditer(prefix)
    )


def rust_inline_module_spans(code: str, text: str | None = None) -> list[InlineModule]:
    """Return brace-delimited inline module spans from masked Rust code."""
    stack: list[int] = []
    closing_brace: dict[int, int] = {}
    for index, character in enumerate(code):
        if character == "{":
            stack.append(index)
        elif character == "}" and stack:
            closing_brace[stack.pop()] = index

    spans: list[InlineModule] = []
    for module in RUST_INLINE_MODULE.finditer(code):
        opening = code.rfind("{", module.start(), module.end())
        closing = closing_brace.get(opening)
        if closing is not None:
            attributes = module.group("attributes")
            if text is not None:
                attributes = text[
                    module.start("attributes") : module.end("attributes")
                ]
            spans.append(
                InlineModule(
                    opening,
                    closing,
                    module.group("name"),
                    rust_item_is_disabled(attributes),
                    rust_item_is_disabled_in_ci(attributes),
                )
            )
    return spans


def rust_matching_delimiters(code: str) -> dict[int, int]:
    """Return matching Rust delimiter offsets from masked source."""
    stack: list[tuple[str, int]] = []
    pairs: dict[int, int] = {}
    opening_for = {")": "(", "]": "[", "}": "{"}
    for index, character in enumerate(code):
        if character in "([{":
            stack.append((character, index))
        elif character in ")]}" and stack:
            opening, offset = stack[-1]
            if opening == opening_for[character]:
                stack.pop()
                pairs[offset] = index
    return pairs


def rust_macro_rule_spans(
    code: str, body_start: int, body_end: int, delimiters: dict[int, int]
) -> list[tuple[tuple[int, int], tuple[int, int]]]:
    """Return the matcher and transcriber spans of each `macro_rules` rule.

    An empty result means the definition does not read as a plain sequence of
    ``(matcher) => {transcriber};`` rules, which callers treat as unknown
    rather than as a definition without rules.
    """
    rules: list[tuple[tuple[int, int], tuple[int, int]]] = []
    index = body_start
    while index < body_end:
        while index < body_end and code[index] in " \t\r\n;":
            index += 1
        if index >= body_end:
            return rules
        if code[index] not in "([{":
            return []
        matcher_end = delimiters.get(index)
        if matcher_end is None or matcher_end >= body_end:
            return []
        matcher = (index + 1, matcher_end)
        index = matcher_end + 1
        while index < body_end and code[index] in " \t\r\n":
            index += 1
        if not code.startswith("=>", index):
            return []
        index += 2
        while index < body_end and code[index] in " \t\r\n":
            index += 1
        if index >= body_end or code[index] not in "([{":
            return []
        transcriber_end = delimiters.get(index)
        if transcriber_end is None or transcriber_end >= body_end:
            return []
        rules.append((matcher, (index + 1, transcriber_end)))
        index = transcriber_end + 1
    return rules


def rust_forwarded_metavariables(transcriber: str) -> set[str]:
    """Return the metavariable names one transcriber places inside attributes."""
    names: set[str] = set()
    for attribute in RUST_FORWARDED_ATTRIBUTE.finditer(transcriber):
        for metavariable in RUST_METAVARIABLE.finditer(attribute.group(0)):
            names.add(metavariable.group("name"))
    return names


def rust_matcher_argument_positions(matcher: str) -> dict[str, int] | None:
    """Return each metavariable's argument position in a plain list matcher.

    ``None`` means the matcher does not read as a comma-separated list whose
    metavariable fragments are exactly one binding each, so no position can be
    attributed to a forwarded metavariable.
    """
    if RUST_METAVARIABLE_REPETITION.search(matcher) is not None:
        return None
    positions: dict[str, int] = {}
    if not matcher.strip():
        return positions
    for position, fragment in enumerate(split_rust_meta_items(matcher)):
        binding = RUST_METAVARIABLE_BINDING.fullmatch(fragment.strip())
        if binding is None:
            if RUST_METAVARIABLE.search(fragment) is not None:
                return None
            continue
        if binding.group("name") in positions:
            return None
        positions[binding.group("name")] = position
    return positions


def rust_forwarded_argument_positions(
    code: str, body_start: int, body_end: int, delimiters: dict[int, int]
) -> set[int] | None:
    """Return the invocation positions a macro forwards into its attributes.

    ``None`` means the binding between a forwarded metavariable and one
    invocation argument is not determinable, so every argument is inspected.
    """
    rules = rust_macro_rule_spans(code, body_start, body_end, delimiters)
    if not rules:
        return None
    positions: set[int] = set()
    for (matcher_start, matcher_end), (body_open, body_close) in rules:
        forwarded = rust_forwarded_metavariables(code[body_open:body_close])
        if not forwarded:
            continue
        bound = rust_matcher_argument_positions(
            code[matcher_start:matcher_end]
        )
        if bound is None:
            return None
        for metavariable in forwarded:
            if metavariable not in bound:
                return None
            positions.add(bound[metavariable])
    return positions


def rust_macro_invocation_applies_test(
    definition: RustSource,
    name: str,
    definition_start: int,
    body_opening: int,
    sites: list[RustSource],
) -> bool:
    """Return whether a forwarding macro is invoked with test metadata.

    A macro is invocable from any file that can name it, so every repository
    source is a candidate call site, not only the one that defines it.
    """
    code = definition.code
    definition_end = definition.delimiters.get(body_opening)
    if definition_end is None:
        return False
    definition_body = code[definition_start:definition_end]
    if RUST_FORWARDED_ATTRIBUTE.search(definition_body) is None:
        return False
    forwarded_positions = rust_forwarded_argument_positions(
        code, body_opening + 1, definition_end, definition.delimiters
    )
    for site in sites:
        for opening in site.invocations.get(name, ()):
            if site is definition and (
                definition_start <= opening < definition_end
            ):
                continue
            closing = site.delimiters.get(opening)
            if closing is None:
                continue
            body = site.code[opening + 1 : closing]
            arguments = split_rust_meta_items(body)
            if forwarded_positions is None:
                candidates = rust_top_level_meta_items(body)
            else:
                candidates = [
                    arguments[position]
                    for position in sorted(forwarded_positions)
                    if position < len(arguments)
                ]
            aliases = rust_visible_test_aliases(site.aliases, opening)
            if any(
                rust_meta_applies_test(candidate, aliases)
                for candidate in candidates
            ):
                return True
    return False


def rust_enclosing_modules(
    spans: list[InlineModule], offset: int
) -> list[InlineModule]:
    """Return outer-to-inner inline modules enclosing one declaration."""
    return sorted(
        (span for span in spans if span.opening < offset < span.closing),
        key=lambda span: span.opening,
    )


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
            # Skip the image's complete span, reference part included.
            # Advancing one character re-enters the image's own construct, and
            # `![alt][owner]` then parses `[owner]` as a shortcut link — which
            # counts the image as a citation and defeats the exclusion this
            # branch exists to apply.
            image_label_end = find_closing_bracket(text, index)
            if image_label_end is None:
                index += 1
                continue
            image_end = image_label_end + 1
            if image_end < len(text) and text[image_end] == "[":
                image_reference_end = find_closing_bracket(text, image_end)
                if image_reference_end is not None:
                    image_end = image_reference_end + 1
            index = image_end
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
                    offset=index,
                    definition_offset=definition.offset,
                )
            )
        index = end
    return links


def mask_markdown_link_constructs(text: str) -> str:
    """Blank every link construct, leaving the prose that separates them.

    A tagged claim is made by the prose or by one link's own label, so the
    prose scan must not read a neighbouring label as if it were prose.
    Offsets are preserved so link positions stay comparable.
    """
    buffer = list(text)
    index = 0
    while index < len(text):
        if text[index] != "[" or is_escaped(text, index):
            index += 1
            continue
        inline = parse_inline_link_at(text, index)
        if inline is not None:
            mask_range(buffer, index, inline[1] + 1)
            index = inline[1] + 1
            continue
        label_end = find_closing_bracket(text, index)
        if label_end is None:
            index += 1
            continue
        end = label_end + 1
        if end < len(text) and text[end] == "[":
            reference_end = find_closing_bracket(text, end)
            if reference_end is not None:
                end = reference_end + 1
        mask_range(buffer, index, end)
        index = end
    return "".join(buffer)


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
    """Return tracked Markdown inputs in ``docs`` plus the root guidance."""
    docs = (root / "docs").resolve()
    agents = (root / "AGENTS.md").resolve()
    return [
        path
        for path in tracked_files(root)
        if path.suffix == ".md"
        and (path == agents or docs in path.parents)
    ]


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


def rust_test_invariant_tags(
    text: str,
    module_prefixes: tuple[tuple[str, ...], ...] = ((),),
    aliases: list[ScopedTestAlias] | None = None,
    ignored_test_selections: tuple[IgnoredTestSelection, ...] = (),
) -> list[tuple[str, int]]:
    """Return declaration-local INV tags and lines from Rust tests.

    Only the test name and its attached doc comments declare tags. Module names
    are context, not test annotations, even when they happen to contain INV text.
    """
    found: dict[str, int] = {}
    code = mask_rust_non_code(text)
    module_spans = rust_inline_module_spans(code, text)
    if aliases is None:
        aliases = rust_test_attribute_aliases(code)
    for declaration in RUST_TEST_DECLARATION.finditer(code):
        raw_prefix = text[
            declaration.start("prefix") : declaration.end("prefix")
        ]
        code_prefix = declaration.group("prefix")
        attributes = rust_raw_attributes(raw_prefix, code_prefix)
        visible = rust_visible_test_aliases(aliases, declaration.start())
        if (
            RUST_TEST_ATTRIBUTE.search(code_prefix) is None
            and not rust_attributes_apply_test(attributes, visible)
        ):
            continue
        if rust_item_is_disabled_in_ci(attributes):
            continue
        ignored = rust_item_may_be_ignored(attributes)
        if ignored and not ignored_test_selections:
            continue
        enclosing = rust_enclosing_modules(module_spans, declaration.start())
        if any(module.ci_disabled for module in enclosing):
            continue
        if ignored:
            local_name = tuple(
                [*(module.name for module in enclosing), declaration.group("name")]
            )
            registered_names = tuple(
                "::".join((*prefix, *local_name)) for prefix in module_prefixes
            )
            if not any(
                not any(
                    skipped in registered_name for skipped in selection.skips
                )
                for selection in ignored_test_selections
                for registered_name in registered_names
            ):
                continue
        doc_comments = "\n".join(rust_doc_comments(raw_prefix))
        declaration_line = line_number(text, declaration.start("name"))
        material = "\n".join([doc_comments, declaration.group("name")])
        for tag in INVARIANT_TAG.finditer(material):
            invariant = f"INV-{tag.group('number')}"
            found.setdefault(invariant, declaration_line)
    return sorted(found.items())


def rust_module_file(directories: list[Path], relatives: list[str]) -> Path | None:
    """Return the first candidate file one directory and name pair resolves."""
    for directory in directories:
        for relative in relatives:
            # Normalized lexically so a `..` in `#[path]` still names the same
            # entry the file scan produced, without resolving symlinks.
            candidate = Path(os.path.normpath(directory / relative))
            if candidate.is_file():
                return candidate
    return None


def rust_module_children(
    declaring: Path,
    name: str,
    attributes: str,
    inline: tuple[str, ...] = (),
) -> list[Path]:
    """Resolve one out-of-line `mod name;` declaration to its source files.

    Child modules of a crate root or a `mod.rs` live beside their declaring
    file; children of any other file live in a directory named for it. Both
    directories are offered in that order so the resolution needs no crate
    metadata, and a declaration nested in inline modules resolves under a
    directory named for each of them, exactly as the compiler descends.

    A declaration whose `#[path]` attributes name several files — one per
    target configuration, say — keeps every one that exists, since which the
    build selects is not this checker's to decide. Without such an attribute
    the conventional pair is an either/or and the first match wins.
    """
    if declaring.name in RUST_CRATE_ROOT_NAMES:
        directories = [declaring.parent]
    else:
        directories = [declaring.parent / declaring.stem, declaring.parent]
    directories = [directory.joinpath(*inline) for directory in directories]
    explicit = rust_module_path_attributes(attributes)
    if not explicit:
        conventional = rust_module_file(
            directories, [f"{name}.rs", f"{name}/mod.rs"]
        )
        return [] if conventional is None else [conventional]
    found: list[Path] = []
    for relative in explicit:
        candidate = rust_module_file(directories, [relative])
        if candidate is not None and candidate not in found:
            found.append(candidate)
    return found


def rust_module_graph(
    sources: list[RustSource], target_roots: frozenset[Path] = frozenset()
) -> tuple[dict[Path, tuple[tuple[str, ...], ...]], dict[Path, set[Path]]]:
    """Return each file's out-of-line module paths and the roots reaching it.

    A file several active declarations reach keeps every one of those paths,
    since the harness registers its tests once under each. Paths are visited
    breadth-first from the files no declaration reaches, and a path never
    revisits a file it already contains, so only simple paths are walked and
    a cyclic declaration terminates instead of growing forever. Recording each
    path once keeps the result independent of filesystem iteration order.
    """
    children: dict[Path, list[tuple[tuple[str, ...], Path]]] = {}
    declared: set[Path] = set()
    for source in sources:
        module_spans = rust_inline_module_spans(source.code, source.text)
        for module in RUST_OUT_OF_LINE_MODULE.finditer(source.code):
            # Masking preserves offsets but blanks string literals, so cfg
            # and `#[path]` metadata are read back from the raw source.
            raw_attributes = source.text[
                module.start("attributes") : module.end("attributes")
            ]
            if rust_item_is_disabled_in_ci(raw_attributes):
                continue
            enclosing = rust_enclosing_modules(
                module_spans, module.start("name")
            )
            if any(item.ci_disabled for item in enclosing):
                continue
            inline = tuple(item.name for item in enclosing)
            for child in rust_module_children(
                source.path, module.group("name"), raw_attributes, inline
            ):
                if child == source.path:
                    continue
                children.setdefault(source.path, []).append(
                    ((*inline, module.group("name")), child)
                )
                declared.add(child)
        for include in RUST_INCLUDE_OPEN.finditer(source.code):
            # Masking blanks the literal, so the destination is read raw
            # from the offset the masked scan located.
            literal = RUST_STRING_LITERAL.match(
                source.text, include.end("opening")
            )
            if literal is None:
                continue
            plain = literal.group("plain")
            relative = literal.group("raw") if plain is None else plain
            raw_attributes = source.text[
                include.start("attributes") : include.end("attributes")
            ]
            if rust_item_is_disabled_in_ci(raw_attributes):
                continue
            enclosing = rust_enclosing_modules(
                module_spans, include.end("opening")
            )
            if any(item.ci_disabled for item in enclosing):
                continue
            # `include!` splices into the module that includes it, so the
            # included file carries that module path with no new segment.
            included = Path(
                os.path.normpath(source.path.parent / relative)
            )
            if not included.is_file() or included == source.path:
                continue
            children.setdefault(source.path, []).append(
                (tuple(item.name for item in enclosing), included)
            )
            declared.add(included)

    prefixes: dict[Path, list[tuple[str, ...]]] = {}
    roots: dict[Path, set[Path]] = {}
    # Where the repository declares Cargo targets, they are the only roots:
    # a source-shaped fixture no target reaches is not a crate. A tree with
    # no manifest at all keeps the plain reading, every undeclared file a
    # root of its own.
    if target_roots:
        seeds = [
            source.path for source in sources if source.path in target_roots
        ]
    else:
        seeds = [
            source.path
            for source in sources
            if source.path not in declared
        ]
    pending: list[tuple[Path, tuple[str, ...], frozenset[Path], Path]] = [
        (path, (), frozenset({path}), path) for path in seeds
    ]
    while pending:
        path, prefix, walked, root = pending.pop(0)
        roots.setdefault(path, set()).add(root)
        recorded = prefixes.setdefault(path, [])
        if prefix in recorded:
            continue
        recorded.append(prefix)
        for names, child in children.get(path, []):
            if child in walked:
                continue
            pending.append(
                (child, (*prefix, *names), walked | {child}, root)
            )
    return (
        {path: tuple(paths) for path, paths in prefixes.items()},
        roots,
    )


@dataclass(frozen=True)
class IgnoredTestSelection:
    """One suite's ignored-test selection within one Cargo target.

    Binary includes and excludes partition same-package targets before each
    skip entry narrows registered test names the way both libtest's `--skip`
    and nextest's `not test(...)` do.
    """

    skips: tuple[str, ...]
    includes: tuple[str, ...]
    excludes: tuple[str, ...]


@dataclass(frozen=True)
class IgnoredTestRun:
    """One manifest suite that executes ignored tests in authoritative CI."""

    package: str
    features: tuple[str, ...]
    selection: IgnoredTestSelection


def workflow_ignored_test_runs(root: Path) -> list[IgnoredTestRun]:
    """Read the ignored-test selections authoritative CI executes.

    Ground truth is `.github/postgres-integration-suites.toml`, not the
    workflow: a suite's package, features, and skips live in the manifest that
    the workflow's own jobs derive their archive plan and shard matrix from.
    `check_suite_manifest` gates the two sides against each other so the
    manifest cannot drift from what CI runs.

    Each suite archives its package and the run filter selects the manifest's
    included targets, removes its excluded targets, then removes its skipped
    test names. With no binary fields this is the whole package.

    A repository with no manifest runs no ignored tests in CI.
    `check_suite_manifest` rejects a manifest that has gone missing from a
    repository whose CI still expects one.
    """
    if not (root / postgres_integration_suites.MANIFEST).is_file():
        return []
    runs = [
        IgnoredTestRun(
            package=suite.package,
            features=suite.features,
            selection=IgnoredTestSelection(
                skips=tuple(sorted(suite.skip)),
                includes=tuple(sorted(suite.include_binaries)),
                excludes=tuple(sorted(suite.exclude_binaries)),
            ),
        )
        for suite in postgres_integration_suites.load_suites(root)
    ]
    workflow_path = root / postgres_integration_suites.WORKFLOW
    if not workflow_path.is_file():
        return runs
    workflow = workflow_path.read_text(encoding="utf-8")
    executed_commands = [
        tokens
        for command, _, _ in postgres_integration_suites.workflow_shell_commands(
            workflow
        )
        for tokens in postgres_integration_suites.simple_commands(command)
    ]
    if any(
        arguments is not None
        and postgres_integration_suites.runs_file_media_isolation_tests(arguments)
        for tokens in executed_commands
        for arguments in [postgres_integration_suites.cargo_test_arguments(tokens)]
    ):
        runs.append(
            IgnoredTestRun(
                package="signalbox-file-media-processor-runtime",
                features=("test-worker",),
                selection=IgnoredTestSelection(
                    skips=(),
                    includes=("isolation",),
                    excludes=(),
                ),
            )
        )
    return runs


def declared_package(package: Path) -> dict:
    """Read one package manifest, or an empty document if it cannot be read."""
    try:
        return tomllib.loads((package / "Cargo.toml").read_text(encoding="utf-8"))
    except (OSError, tomllib.TOMLDecodeError):
        return {}


def enabled_package_features(package: Path, requested: tuple[str, ...]) -> set[str]:
    """Return the features one suite actually enables for a package.

    A suite names the features it adds; Cargo also enables `default` — no
    invocation here passes `--no-default-features` — and every feature a
    reachable feature turns on. Feature-to-feature edges are followed;
    `dep:name` and `package/feature` entries enable something other than a
    feature of this package and are not.
    """
    declared = declared_package(package).get("features")
    table = declared if isinstance(declared, dict) else {}
    enabled = set(requested)
    if "default" in table:
        enabled.add("default")
    pending = list(enabled)
    while pending:
        for entry in table.get(pending.pop(), []):
            if not isinstance(entry, str) or ":" in entry or "/" in entry:
                continue
            if entry not in enabled:
                enabled.add(entry)
                pending.append(entry)
    return enabled


def target_required_features(package: Path) -> dict[Path, tuple[str, ...]]:
    """Map each explicitly declared Cargo target root to its required features.

    Cargo skips a target whose `required-features` are not all enabled — it
    builds nothing and reports success. A target skipped that way enforces
    nothing, so the invariant index must not credit it.
    """
    declared = declared_package(package)
    required: dict[Path, tuple[str, ...]] = {}
    for table, directory in zip(CARGO_TARGET_TABLES, CARGO_TARGET_DIRECTORIES):
        entries = declared.get(table)
        if not isinstance(entries, list):
            continue
        for entry in entries:
            if not isinstance(entry, dict):
                continue
            features = entry.get("required-features")
            if not isinstance(features, list):
                continue
            root = declared_target_root(package, entry, directory)
            if root is None:
                continue
            required[root] = tuple(
                feature for feature in features if isinstance(feature, str)
            )
    return required


def declared_target_root(package: Path, entry: dict, directory: str) -> Path | None:
    """Resolve one declared Cargo target's root file.

    `path` is optional: a target table that only names its target still binds
    to the conventional root Cargo infers, and Cargo still skips it when its
    `required-features` are unmet. Reading only explicit paths would let the
    inferred spelling escape the feature check entirely.
    """
    relative = entry.get("path")
    if isinstance(relative, str):
        candidate = Path(os.path.normpath(package / relative))
        return candidate if candidate.is_file() else None
    name = entry.get("name")
    if not isinstance(name, str):
        return None
    candidates = (
        package / directory / f"{name}.rs",
        package / directory / name / "main.rs",
    )
    return next((path for path in candidates if path.is_file()), None)


def cargo_package_directories(root: Path) -> dict[str, Path]:
    """Return each Cargo package name and its manifest directory."""
    packages: dict[str, Path] = {}
    for manifest in (
        path for path in tracked_files(root) if path.name == "Cargo.toml"
    ):
        try:
            declared = tomllib.loads(manifest.read_text(encoding="utf-8"))
        except (OSError, tomllib.TOMLDecodeError):
            continue
        package = declared.get("package")
        name = package.get("name") if isinstance(package, dict) else None
        if isinstance(name, str):
            packages[name] = manifest.parent
    return packages


def cargo_test_roots(package: Path) -> list[Path]:
    """Return Cargo target roots selected by `cargo test --tests`."""
    roots: list[Path] = []
    for target in cargo_target_roots(package):
        relative = target.relative_to(package)
        if relative == Path("build.rs") or relative.parts[0] in (
            "benches",
            "examples",
        ):
            continue
        roots.append(target)
    return roots


def cargo_test_target_names(package: Path) -> dict[Path, str]:
    """Map each test root to the target name Cargo exposes to nextest."""
    declared = declared_package(package)
    package_table = declared.get("package")
    package_name = (
        package_table.get("name") if isinstance(package_table, dict) else None
    )
    names: dict[Path, str] = {}
    for target in cargo_test_roots(package):
        relative = target.relative_to(package)
        if relative == Path("src/lib.rs") and isinstance(package_name, str):
            names[target] = package_name.replace("-", "_")
        elif relative == Path("src/main.rs") and isinstance(package_name, str):
            names[target] = package_name
        elif relative.parts[0] in ("src", "tests"):
            names[target] = (
                relative.parent.name if relative.name == "main.rs" else target.stem
            )
    library = declared.get("lib")
    if isinstance(library, dict):
        root = declared_target_root(package, library, "src")
        if root is None and (package / "src/lib.rs").is_file():
            root = package / "src/lib.rs"
        name = library.get("name")
        if not isinstance(name, str) and isinstance(package_name, str):
            name = package_name.replace("-", "_")
        if root is not None and isinstance(name, str):
            names[root] = name
    for table, directory in (("bin", "src/bin"), ("test", "tests")):
        entries = declared.get(table)
        if not isinstance(entries, list):
            continue
        for entry in entries:
            if not isinstance(entry, dict):
                continue
            root = declared_target_root(package, entry, directory)
            name = entry.get("name")
            if root is not None and isinstance(name, str):
                names[root] = name
    return names


def ignored_test_selections_by_target(
    root: Path,
) -> dict[Path, tuple[IgnoredTestSelection, ...]]:
    """Map Cargo target roots to the ignored-test runs authoritative CI executes."""
    packages = cargo_package_directories(root)
    selected: dict[Path, set[IgnoredTestSelection]] = {}
    for run in workflow_ignored_test_runs(root):
        package = packages.get(run.package)
        if package is None:
            continue
        enabled = enabled_package_features(package, run.features)
        required = target_required_features(package)
        target_names = cargo_test_target_names(package)
        for target in cargo_test_roots(package):
            if not set(required.get(target, ())) <= enabled:
                continue
            target_name = target_names.get(target)
            if target_name is None:
                continue
            if run.selection.includes and target_name not in run.selection.includes:
                continue
            if target_name in run.selection.excludes:
                continue
            selected.setdefault(target, set()).add(run.selection)
    return {
        target: tuple(
            sorted(
                selections,
                key=lambda selection: (
                    selection.includes,
                    selection.excludes,
                    selection.skips,
                ),
            )
        )
        for target, selections in selected.items()
    }


CARGO_TARGET_TABLES = ("bin", "test", "bench", "example")
CARGO_TARGET_DIRECTORIES = ("src/bin", "tests", "benches", "examples")
CARGO_CONVENTIONAL_ROOTS = ("src/lib.rs", "src/main.rs", "build.rs")


def cargo_target_roots(root: Path) -> list[Path]:
    """Return the Rust files Cargo compiles as a target root.

    Discovery starts from what Cargo builds, so a source-shaped fixture no
    target reaches is not read as a crate of its own. The conventional layout
    is enumerated directly and each manifest's explicit `path` entries are
    added, which together cover every target Cargo resolves without invoking
    it.
    """
    roots: set[Path] = set()
    tracked = tracked_files(root)
    tracked_set = frozenset(tracked)
    for manifest in (path for path in tracked if path.name == "Cargo.toml"):
        package = manifest.parent
        for relative in CARGO_CONVENTIONAL_ROOTS:
            candidate = package / relative
            if candidate in tracked_set:
                roots.add(candidate)
        for path in tracked:
            try:
                relative = path.relative_to(package)
            except ValueError:
                continue
            if path.suffix != ".rs" or not relative.parts:
                continue
            if any(
                relative.parent == Path(directory)
                or (
                    len(relative.parts) == len(Path(directory).parts) + 2
                    and relative.name == "main.rs"
                    and relative.parts[: len(Path(directory).parts)]
                    == Path(directory).parts
                )
                for directory in CARGO_TARGET_DIRECTORIES
            ):
                roots.add(path)
        try:
            declared = tomllib.loads(
                manifest.read_text(encoding="utf-8", errors="replace")
            )
        except (OSError, tomllib.TOMLDecodeError):
            continue
        tables: list[dict[str, object]] = []
        library = declared.get("lib")
        if isinstance(library, dict):
            tables.append(library)
        for name in CARGO_TARGET_TABLES:
            entries = declared.get(name)
            if isinstance(entries, list):
                tables.extend(
                    entry for entry in entries if isinstance(entry, dict)
                )
        for table in tables:
            path = table.get("path")
            if isinstance(path, str):
                roots.add(Path(os.path.normpath(package / path)))
    return sorted(path for path in roots if path.is_file())


def rust_sources(root: Path) -> list[RustSource]:
    """Read and lexically prepare every repository Rust file exactly once.

    Module paths and test-attribute aliases are resolved in a second pass,
    because a `pub use` rename in a crate root names an attribute the modules
    beneath it import.
    """
    paths = [path for path in tracked_files(root) if path.suffix == ".rs"]
    prepared: list[RustSource] = []
    for path in paths:
        text = path.read_text(encoding="utf-8", errors="replace")
        code = mask_rust_non_code(text)
        invocations: dict[str, list[int]] = {}
        for invocation in RUST_MACRO_INVOCATION.finditer(code):
            invocations.setdefault(invocation.group("name"), []).append(
                invocation.start("opening")
            )
        prepared.append(
            RustSource(
                path=path,
                label=repository_path(root, path),
                text=text,
                code=code,
                delimiters=rust_matching_delimiters(code),
                invocations=invocations,
                aliases=[],
                module_prefixes=((),),
                ignored_test_selections=(),
            )
        )
    target_roots = frozenset(cargo_target_roots(root))
    prefixes, roots = rust_module_graph(prepared, target_roots)
    ignored_by_target = ignored_test_selections_by_target(root)
    reachable = set(prefixes)
    prepared = [source for source in prepared if source.path in reachable]
    exported = {
        source.path: rust_exported_test_aliases(source.code)
        for source in prepared
    }
    for source in prepared:
        source.module_prefixes = prefixes.get(source.path, ((),))
        visible: set[str] = set()
        for reachable_root in roots.get(source.path, {source.path}):
            visible.update(exported.get(reachable_root, frozenset()))
        source.aliases = rust_test_attribute_aliases(
            source.code, frozenset(visible)
        )
        ignored_selections = {
            selection
            for target in roots.get(source.path, {source.path})
            for selection in ignored_by_target.get(target, ())
        }
        source.ignored_test_selections = tuple(
            sorted(
                ignored_selections,
                key=lambda selection: (
                    selection.includes,
                    selection.excludes,
                    selection.skips,
                ),
            )
        )
    return prepared


def rust_invariant_test_files(
    sources: list[RustSource],
) -> dict[tuple[str, str], int]:
    """Discover every repository Rust test file carrying an INV tag."""
    found: dict[tuple[str, str], int] = {}
    for source in sources:
        tags = rust_test_invariant_tags(
            source.text,
            source.module_prefixes,
            source.aliases,
            source.ignored_test_selections,
        )
        for invariant, line in tags:
            found[(invariant, source.label)] = line
    return found


def rust_proc_macro_test_generators(source: RustSource) -> list[int]:
    """Return the offsets of procedural macros that spell a test attribute.

    A procedural macro assembles its output in ordinary Rust, so only a test
    attribute written out in the definition — in a `quote!` body or a string
    it parses — is visible here. One assembled from separate tokens is not
    decidable from source and is out of this check's reach.
    """
    offsets: list[int] = []
    for declaration in RUST_TEST_DECLARATION.finditer(source.code):
        if RUST_PROC_MACRO_ATTRIBUTE.search(declaration.group("prefix")) is None:
            continue
        opening = source.code.find("{", declaration.end("name"))
        closing = source.delimiters.get(opening) if opening >= 0 else None
        if closing is None:
            continue
        # Comments are removed but literals kept: a generator writes its
        # output into literals, and only mentions it in comments.
        body = mask_rust_non_code(
            source.text[opening:closing], preserve_literals=True
        )
        if RUST_TEST_ATTRIBUTE.search(body) is not None:
            offsets.append(declaration.start())
    return offsets


def check_rust_test_generation(sources: list[RustSource]) -> list[Violation]:
    """Reject macros whose expanded tests cannot be registered from source."""
    violations: list[Violation] = []
    for source in sources:
        for offset in rust_proc_macro_test_generators(source):
            violations.append(
                Violation(
                    source.label,
                    line_number(source.text, offset),
                    "invariant-test-generation",
                    "this procedural macro spells a test attribute in its "
                    "expansion; write explicit test declarations so invariant "
                    "registration remains mechanically visible",
                )
            )
    definition_counts: dict[str, int] = {}
    for source in sources:
        for macro in RUST_MACRO_RULES.finditer(source.code):
            name = macro.group("name")
            definition_counts[name] = definition_counts.get(name, 0) + 1
    for source in sources:
        code = source.code
        for macro in RUST_MACRO_RULES.finditer(code):
            opening = macro.start("opening")
            closing = source.delimiters.get(opening)
            if closing is None:
                continue
            aliases = rust_visible_test_aliases(source.aliases, opening)
            # A matcher may consume a test attribute the expansion never
            # emits, so only what each rule expands to is read directly; an
            # unparsed rule list falls back to the whole definition body.
            rules = rust_macro_rule_spans(
                code, opening + 1, closing, source.delimiters
            )
            body = code[opening + 1 : closing]
            emitted = (
                "\n".join(code[start:end] for _, (start, end) in rules)
                if rules
                else body
            )
            if (
                RUST_TEST_ATTRIBUTE.search(emitted) is None
                and not rust_attributes_apply_test(emitted, aliases)
                and not rust_macro_invocation_applies_test(
                    source,
                    macro.group("name"),
                    macro.start(),
                    opening,
                    # One definition of a name owns every invocation of it in
                    # the repository. Where a name is defined more than once,
                    # which definition a call site reaches is a visibility
                    # question this check does not answer, so only the
                    # definition's own file is read.
                    sources
                    if definition_counts[macro.group("name")] == 1
                    else [source],
                )
            ):
                continue
            violations.append(
                Violation(
                    source.label,
                    line_number(source.text, macro.start()),
                    "invariant-test-generation",
                    f"`macro_rules! {macro.group('name')}` emits or forwards "
                    "a test attribute; write explicit test declarations so "
                    "invariant registration remains mechanically visible",
                )
            )
    return violations


def check_invariant_citations(
    root: Path, sources: list[RustSource]
) -> tuple[list[Violation], set[tuple[int, str]]]:
    source = root / INVARIANTS
    text = mask_block_content(source.read_text(encoding="utf-8"))
    definitions = reference_definitions(text)
    violations: list[Violation] = []
    enforcement_links: set[tuple[int, str]] = set()
    catalog_pairs: set[tuple[str, str]] = set()
    target_text_cache: dict[Path, str] = {
        entry.path: entry.text for entry in sources
    }
    rust_test_files = rust_invariant_test_files(sources)
    declared_tags_by_file: dict[str, set[str]] = {}
    for invariant_and_path in rust_test_files:
        invariant, source_label = invariant_and_path
        declared_tags_by_file.setdefault(source_label, set()).add(invariant)

    for number, line in enumerate(text.splitlines(), start=1):
        if not re.match(r"^\|[ \t]*INV-[0-9]{3}[ \t]*\|", line):
            continue
        cells = split_table_row(line)
        if len(cells) not in (2, 5):
            violations.append(
                Violation(
                    INVARIANTS.as_posix(),
                    number,
                    "invariant-citation",
                    f"expected 2 generated-index cells, found {len(cells)}",
                )
            )
            continue
        invariant = cells[0]
        # Five-cell rows remain accepted by focused parser fixtures. The
        # repository file itself is exact-checked as a generated two-cell
        # index by scripts/generate_invariants.py.
        enforcement = cells[-1]
        tagged_marker = re.compile(
            rf"(?i)(?<![A-Za-z0-9]){re.escape(invariant)}-tagged(?![A-Za-z0-9])",
        )
        citation_enforcement = mask_inline_code(enforcement)
        resolved_links = sorted(
            extract_resolved_links(citation_enforcement, definitions),
            key=lambda link: link.offset,
        )
        citation_prose = mask_markdown_link_constructs(citation_enforcement)
        tagged_destinations: set[str] = set()
        tagged_active = False
        preceding_offset = 0
        for link in resolved_links:
            intervening = citation_prose[preceding_offset : link.offset]
            last_marker = max(
                (match.start() for match in tagged_marker.finditer(intervening)),
                default=-1,
            )
            last_boundary = max(
                (
                    match.start()
                    for match in re.finditer(r"[.;](?=[ \t]|$)", intervening)
                ),
                default=-1,
            )
            if last_marker >= 0 or last_boundary >= 0:
                tagged_active = last_marker > last_boundary
            # A marker inside one link label claims that link alone, so it
            # never carries into the prose or the links that follow.
            label_has_marker = tagged_marker.search(
                mask_inline_code(link.label)
            ) is not None
            if tagged_active or label_has_marker:
                tagged_destinations.add(link.destination)
            preceding_offset = link.offset + 1

        for link in resolved_links:
            resolved = resolve_relative_target(root, source, link.destination)
            if resolved is None:
                continue
            target, _ = resolved
            enforcement_links.add((number, link.destination))
            if link.definition_offset is not None:
                enforcement_links.add(
                    (
                        line_number(text, link.definition_offset),
                        link.destination,
                    )
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
            else:
                target_label = repository_path(root, target)
                catalog_pairs.add((invariant, target_label))
                declared_tags = declared_tags_by_file.get(target_label, set())
                if (
                    (len(cells) == 2 or link.destination in tagged_destinations)
                    and invariant not in declared_tags
                ):
                    violations.append(
                        Violation(
                            INVARIANTS.as_posix(),
                            number,
                            "invariant-tag",
                            f"{invariant} cites `{target_label}` as tagged "
                            f"enforcement, but the file contains no {invariant} tag "
                            "in a test name or attached doc comment",
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
            if target not in target_text_cache:
                target_text_cache[target] = target.read_text(
                    encoding="utf-8", errors="replace"
                )
            target_text = target_text_cache[target]
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

    for pair, line in rust_test_files.items():
        if pair in catalog_pairs:
            continue
        invariant, source_label = pair
        violations.append(
            Violation(
                source_label,
                line,
                "invariant-registration",
                f"{invariant}-tagged tests in `{source_label}` are not cited "
                f"by the {invariant} Enforcement cell",
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


def check_suite_manifest(root: Path) -> list[Violation]:
    """Hold the suite manifest, the Rust workflow, and the docs in agreement.

    The manifest decides which `#[ignore]`d tests count as CI-enforced, so a
    manifest that drifts from what CI actually runs corrupts `docs/invariants.md`
    silently and in the dangerous direction: it would keep claiming enforcement
    the workflow no longer performs. Three agreements are asserted.

    *Manifest and workflow.* The workflow must still derive its archive plan
    and its shard matrix from the manifest, must publish exactly one archive
    artifact per declared suite, and must run no ignored tests through a
    `cargo test` command of its own. Nothing here reconstructs a Cargo
    invocation out of YAML.

    *Manifest and documentation.* Prose that teaches a reader to run a suite
    locally names the same package and features CI archives. Drift there is
    invisible to a reader: the documented command still succeeds, and silently
    runs a different set of tests.

    *Manifest and itself.* Every declared package must exist in the workspace,
    and the schema is validated on load.

    A repository with neither manifest nor workflow is not a violation — the
    checker's own fixtures are such repositories. One without the other is.
    """
    manifest = root / postgres_integration_suites.MANIFEST
    workflow = root / postgres_integration_suites.WORKFLOW
    label = postgres_integration_suites.MANIFEST.as_posix()
    if not manifest.is_file():
        if not workflow.is_file():
            return []
        return [
            Violation(
                postgres_integration_suites.WORKFLOW.as_posix(),
                1,
                "suite-manifest",
                f"the Rust workflow exists without {label}",
            )
        ]
    # A malformed manifest raises out of here rather than reporting a
    # violation. It is a broken input, not a citation defect: every other
    # answer this checker computes about ignored tests would be derived from
    # it, so there is nothing trustworthy left to report. `main` renders it the
    # way it renders an unreadable file inventory.
    text = manifest.read_text(encoding="utf-8")
    suites = postgres_integration_suites.parse_suites(text)

    failures: list[Violation] = []
    packages = cargo_package_directories(root)
    for suite in suites:
        if suite.package not in packages:
            failures.append(
                Violation(
                    label,
                    postgres_integration_suites.manifest_line(text, suite.name),
                    "suite-manifest",
                    f"suite `{suite.name}` names package `{suite.package}`, "
                    "which is not a package in this workspace",
                )
            )
    if workflow.is_file():
        failures.extend(
            Violation(
                postgres_integration_suites.WORKFLOW.as_posix(),
                1,
                "suite-manifest",
                message,
            )
            for message in postgres_integration_suites.workflow_disagreements(
                root, suites
            )
        )
    else:
        failures.append(
            Violation(
                label,
                1,
                "suite-manifest",
                f"{label} exists without {postgres_integration_suites.WORKFLOW}",
            )
        )
    for source in markdown_sources(root):
        source_label = repository_path(root, source)
        failures.extend(
            Violation(source_label, line, "suite-manifest", message)
            for line, message in (
                postgres_integration_suites.documentation_disagreements(
                    source_label,
                    source.read_text(encoding="utf-8"),
                    suites,
                )
            )
        )
    return failures


def is_image_link(text: str, link: MarkdownLink) -> bool:
    """Report whether a parsed destination came from image syntax.

    `extract_inline_links` returns image destinations deliberately, because its
    other caller checks that every destination resolves. An image renders a
    fetch rather than a navigation, so it is not a citation. This is the test
    that function already applies to keep images out of its own link pass.
    """
    return bool(
        link.offset
        and text[link.offset - 1] == "!"
        and not is_escaped(text, link.offset - 1)
    )


def check_machine_owner_links(root: Path) -> list[Violation]:
    """Each projection owner links the machine it projects.

    The credential-availability machine is stated once and every other page
    holds a derived view of one of its columns. The failure this guards is the
    one that started that restructuring: a carve moved a paragraph to another
    branch, the anchor citing it still resolved, and only the meaning left — so
    no link checker saw anything.

    Deliberately coarse. It asks whether a page links the owner at all, not
    whether each paragraph does. A per-block rule has to read prose, and the
    checker that read prose generated more review findings than the pages it
    guarded; this costs one resolution per page and adds no Markdown surface
    beyond the link extraction this module already performs.

    Scope, decided by count rather than by argument: whether a citation's label
    *renders* anything is out of scope. Four consecutive review waves produced
    exactly four findings against this function, every one of them the same
    family — a construct that resolves but renders no navigation (an unused
    reference definition, an image destination, an empty label, a raw-HTML
    label). Three were closed by testing the label; the fourth arrived
    immediately after the third was restated positively so that "nothing is
    left for the next shape to slip through." It bought one round, which is the
    same yield the two deleted documentation lints returned before they were
    removed. The complement is unbounded at the string level — an HTML comment,
    a zero-width entity, a soft hyphen — and closing it soundly needs a
    Markdown renderer this stdlib-only module does not have and should not
    acquire for a guard this coarse. None of the four shapes occurs anywhere in
    the tracked corpus.

    So the guarded property is stated as what it always was: a derived view
    that stops citing its owner. A page either carries a link construct
    resolving to the owner or it does not. An invisible label is not that
    failure, and a page that somehow contained one would be broken in a way
    this check could not repair anyway. Images and bare reference definitions
    stay excluded, because those are destination-side facts this module already
    decides without reading a label.
    """
    owner = (root / "docs/spec/credential-availability.md").resolve()
    if not owner.exists():
        return []
    projecting_pages = (
        "docs/spec/turn-lifecycle-and-scheduling.md",
        "docs/spec/persistence-protocol.md",
        "docs/spec/sessions-and-transcript.md",
        "docs/spec/process-protocol.md",
        "docs/spec/runtime-substrate.md",
        "docs/spec/model-call-execution.md",
        "docs/spec/configuration-and-credentials.md",
    )
    violations: list[Violation] = []
    for name in projecting_pages:
        source = root / name
        if not source.exists():
            continue
        parsed = mask_inline_code(
            mask_block_content(source.read_text(encoding="utf-8"))
        )
        # A citation is a link construct whose destination resolves to the
        # owner. Label rendering is deliberately NOT tested — see the scope
        # note in this function's docstring.
        citations = [
            link
            for link in extract_inline_links(parsed)
            if not is_image_link(parsed, link)
        ]
        citations.extend(
            extract_reference_links(parsed, reference_definitions(parsed))
        )
        linked = any(
            (resolved := resolve_relative_target(root, source, link.destination))
            is not None
            and resolved[0] == owner
            for link in citations
        )
        if not linked:
            violations.append(
                Violation(
                    path=name,
                    line=1,
                    category="machine-owner-link",
                    message=(
                        "page projects a column of the credential-availability "
                        "machine but carries no resolving link to "
                        "docs/spec/credential-availability.md; a derived view "
                        "that stops citing its owner is the carve seam no link "
                        "checker can see"
                    ),
                )
            )
    return violations


def run_checks(root: Path = ROOT) -> list[Violation]:
    root = root.resolve()
    heading_anchors.cache_clear()
    sources = rust_sources(root)
    invariant_failures, enforcement_links = check_invariant_citations(
        root, sources
    )
    failures = invariant_failures
    failures.extend(check_relative_links(root, enforcement_links))
    failures.extend(check_machine_owner_links(root))
    failures.extend(check_rust_test_generation(sources))
    failures.extend(check_suite_manifest(root))
    return sorted(set(failures))


def main() -> int:
    try:
        failures = run_checks()
    except (
        TrackedFilesError,
        postgres_integration_suites.ManifestError,
    ) as error:
        print(f"docs-consistency check FAILED: {error}")
        return 1
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
