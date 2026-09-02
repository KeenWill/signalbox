#!/usr/bin/env python3
"""Enforce the three house-style conventions a text scan decides reliably.

Ground truth is the tracked working tree plus `crates/persistence/migrations`,
never a recorded inventory: a rule that counts its own violations from a
checked-in baseline stops seeing the next one. Every rule below restates a
convention `docs/style.md` states normatively, and each finding names the file,
the line, and the fact observed there.

This checker gates. The tree satisfies all three rules, so the step can only
fail on a regression, and any finding is a failure rather than a count in a
burndown.

Reliability over coverage. A rule appears here only when a text scan decides it
with few enough false positives that a reader can trust it without opening the
files, and only when the tree it scans is already at zero. Rules whose decision
needs type resolution, cross-crate body comparison, or a judgment about which
enum a scrutinee belongs to are left to review and to Clippy, which sees the
compiler's own answers; `docs/style.md` records which of those are deferred and
why. A rule whose tree is not at zero belongs in neither place: a step that
always reports gates nothing, and findings nobody can act on go unread.

Coverage is best-effort by design, and this is a property of the tool rather
than a gap in it. Each rule detects the common shapes of its convention in the
spellings this repository actually writes; completeness is explicitly not a
goal, and it is not reachable — a text scan is not a compiler, so for any rule
here a determined author can write a conforming-looking spelling it does not
parse. A finding that reports only "the scanner would not see X" is therefore
describing the design, not a defect, and is declined on that basis; a finding
that shows a rule reporting something it should not, or missing a shape this
tree currently writes, is a defect and is fixed.

Rules, each identified by the `SR-` tag its report line carries:

- SR-8  no code under `apps/` writes SQL naming a persistence-owned table
- SR-12 every clap argument and `ValueEnum` variant carries a doc comment
- SR-13 no proc-macro diagnostic is spanned on `Span::call_site()`

Run from anywhere; `--root` selects the tree. Exits nonzero when any rule
reports, after printing a per-rule count table.
"""

from __future__ import annotations

import argparse
import re
import subprocess
import sys
from collections import Counter
from collections.abc import Callable, Iterator, Sequence
from dataclasses import dataclass
from pathlib import Path

# Trees the rules below scan. `src` alone means production and its inline test
# modules; a rule that decides how a test body reads would be restating the
# testing style guide, which owns that.
RUST_SOURCE_GLOBS = ("crates/*/src/*.rs", "apps/*/src/*.rs")
# Production code under `apps/` only. Whether the rule also binds the daemon's
# integration tests, which assert durable state through raw SQL today, is an
# open scope question; scanning them would report a change nobody has decided
# to make.
APPS_GLOBS = ("apps/*/src/*.rs",)
MIGRATION_GLOB = "crates/*/migrations/*.sql"

# The one binary the SQL-ownership rule excepts: a diagnostic tool whose whole
# purpose is to read durable rows the repository types do not yet project.
SQL_RULE_EXCEPTION = "apps/signalboxd/src/bin/signalbox-debug.rs"


class InventoryError(RuntimeError):
    """Git could not provide a trustworthy tracked-file inventory."""


@dataclass(frozen=True)
class Finding:
    """One observed violation, addressed well enough to open the file at it."""

    rule: str
    path: str
    line: int
    detail: str

    def render(self) -> str:
        return f"{self.rule} {self.path}:{self.line}: {self.detail}"


@dataclass
class Source:
    """One text file with the derived views the rules read.

    `code` blanks comment bodies and string contents in place, so a pattern
    meant for code never matches prose or a SQL literal and every offset still
    maps to the original line.
    """

    path: str
    text: str

    def __post_init__(self) -> None:
        self.lines = self.text.splitlines()
        self.code = _strip_rust(self.text)

    def line_of(self, offset: int) -> int:
        return self.code.count("\n", 0, offset) + 1


def _strip_rust(text: str) -> str:
    """Blank comments and string contents, returning the code view.

    Written as an explicit scanner rather than a regex because Rust's raw string
    literals (`r#"..."#`) and its reuse of the apostrophe for both lifetimes and
    character literals are not a regular language, and a regex that gets either
    wrong silently moves findings into or out of string bodies.
    """
    code = list(text)
    index = 0
    length = len(text)
    while index < length:
        char = text[index]
        if char == "\n":
            index += 1
            continue
        if char == "/" and index + 1 < length and text[index + 1] == "/":
            end = text.find("\n", index)
            end = length if end == -1 else end
            for position in range(index, end):
                code[position] = " "
            index = end
            continue
        if char == "/" and index + 1 < length and text[index + 1] == "*":
            end = text.find("*/", index + 2)
            end = length if end == -1 else end + 2
            for position in range(index, end):
                if code[position] != "\n":
                    code[position] = " "
            index = end
            continue
        if char == "r" and index + 1 < length and text[index + 1] in '"#':
            hashes = 0
            cursor = index + 1
            while cursor < length and text[cursor] == "#":
                hashes += 1
                cursor += 1
            if cursor < length and text[cursor] == '"':
                terminator = '"' + "#" * hashes
                end = text.find(terminator, cursor + 1)
                end = length if end == -1 else end + len(terminator)
                for position in range(cursor + 1, min(end - len(terminator), length)):
                    if code[position] != "\n":
                        code[position] = " "
                index = end
                continue
        if char == '"':
            cursor = index + 1
            while cursor < length:
                if text[cursor] == "\\":
                    cursor += 2
                    continue
                if text[cursor] == '"':
                    break
                cursor += 1
            for position in range(index + 1, min(cursor, length)):
                if code[position] != "\n":
                    code[position] = " "
            index = min(cursor + 1, length)
            continue
        if char == "'":
            # A lifetime, not a character literal, unless the quote closes
            # within three characters (`'a'`, `'\n'`, `'\u{1f}'` excepted by
            # the escape branch below).
            if index + 1 < length and text[index + 1] == "\\":
                end = text.find("'", index + 2)
                end = length if end == -1 else end + 1
                for position in range(index + 1, min(end - 1, length)):
                    code[position] = " "
                index = end
                continue
            if index + 2 < length and text[index + 2] == "'":
                code[index + 1] = " "
                index += 3
                continue
        index += 1
    return "".join(code)


def tracked_files(root: Path, patterns: Sequence[str]) -> list[Path]:
    """List tracked files matching the patterns.

    An empty result is admissible for one pattern — a rule may scan two trees
    and a caller may hold only one of them — but never for a whole run, which
    `audit` refuses below. Git itself failing is always loud: a silent empty
    inventory is how a checker reports success over a tree it never read.
    """
    result = subprocess.run(
        ["git", "ls-files", "-z", "--", *patterns],
        cwd=root,
        check=False,
        capture_output=True,
        text=True,
    )
    if result.returncode != 0:
        raise InventoryError(result.stderr.strip() or "git ls-files failed")
    return [root / label for label in result.stdout.split("\0") if label]


@dataclass
class Repository:
    """The tree under audit, with each file read at most once."""

    root: Path

    def __post_init__(self) -> None:
        self._sources: dict[str, Source] = {}
        self.inspected = 0

    def files(self, patterns: Sequence[str]) -> list[Path]:
        matched = tracked_files(self.root, patterns)
        self.inspected += len(matched)
        return matched

    def source(self, path: Path) -> Source | None:
        label = path.relative_to(self.root).as_posix()
        if label not in self._sources:
            try:
                text = path.read_text(encoding="utf-8")
            except (UnicodeDecodeError, OSError):
                return None
            self._sources[label] = Source(label, text)
        return self._sources[label]

    def sources(self, patterns: Sequence[str]) -> Iterator[Source]:
        for path in self.files(patterns):
            source = self.source(path)
            if source is not None:
                yield source


# --- SR-8: table access belongs to the persistence crate --------------------

CREATE_TABLE = re.compile(
    r"\bcreate\s+table\s+(?:if\s+not\s+exists\s+)?([a-z_][a-z0-9_]*)", re.IGNORECASE
)
# Every SQL form that names a table, not only the ones a query uses: the rule
# forbids naming a table at all, so a DDL statement admitted under `apps/`
# would otherwise never be reported. `LOCK TABLE` is among them because a
# connection-level statement that names a table is the same duplicated schema
# knowledge. The two-word forms precede the bare `truncate` so the alternation
# cannot stop at the verb.
SQL_TABLE_REFERENCE = re.compile(
    r"\b(?:from|join|into|update"
    r"|(?:alter|drop|create|truncate|lock)\s+table"
    r"|truncate)\s+"
    r"(?:if\s+not\s+exists\s+|if\s+exists\s+|only\s+)?"
    r"\"?([a-z_][a-z0-9_]*)\"?",
    re.IGNORECASE,
)
CFG_TEST_MODULE = re.compile(r"#\[cfg\(test\)\]\s*(?:pub\s+)?mod\s+[A-Za-z0-9_]+\s*\{")


def check_app_sql_table_access(repository: Repository) -> Iterator[Finding]:
    """A table name under `apps/` duplicates schema knowledge across a crate."""
    tables: set[str] = set()
    for path in repository.files((MIGRATION_GLOB,)):
        tables.update(
            match.group(1).lower()
            for match in CREATE_TABLE.finditer(path.read_text(encoding="utf-8"))
        )
    if not tables:
        raise InventoryError("no CREATE TABLE statements found in migrations")
    for source in repository.sources(APPS_GLOBS):
        if source.path == SQL_RULE_EXCEPTION:
            continue
        # The rule says production, and an inline `#[cfg(test)]` module is not
        # that: a fixture asserting durable state through raw SQL is a test
        # reading its own setup.
        test_lines = {
            (source.code.count("\n", 0, start) + 1, source.code.count("\n", 0, end) + 1)
            for start, end in _inline_test_spans(source.code)
        }
        for line, literal in _string_literals(source):
            if any(first <= line <= last for first, last in test_lines):
                continue
            for match in SQL_TABLE_REFERENCE.finditer(literal):
                if match.group(1).lower() not in tables:
                    continue
                yield Finding(
                    "SR-8",
                    source.path,
                    line,
                    f"SQL under apps/ names the table `{match.group(1)}`",
                )


def _string_literals(source: Source) -> Iterator[tuple[int, str]]:
    """Yield each string literal in code, with the line it starts on.

    Quotes inside a comment are prose, not a literal: a comment saying which
    query the projection replaced is not a query. The code view blanks a
    comment whole while keeping a real literal's opening delimiter in place, so
    a literal whose first character survived the blanking is one the compiler
    also sees.
    """
    text = source.text
    for match in re.finditer(r'r#*"(?:[^"]|"(?!#))*"#*|"(?:\\.|[^"\\])*"', text, re.S):
        if source.code[match.start()] != text[match.start()]:
            continue
        yield text.count("\n", 0, match.start()) + 1, match.group()


def _inline_test_spans(code: str) -> list[tuple[int, int]]:
    """The spans of the `#[cfg(test)]` modules in a file's code view."""
    spans: list[tuple[int, int]] = []
    for match in CFG_TEST_MODULE.finditer(code):
        opening = code.rindex("{", match.start(), match.end())
        closing = _matching_brace(code, opening)
        if closing is not None:
            spans.append((match.start(), closing))
    return spans


def _matching_brace(text: str, opening: int) -> int | None:
    depth = 0
    for index in range(opening, len(text)):
        if text[index] == "{":
            depth += 1
        elif text[index] == "}":
            depth -= 1
            if depth == 0:
                return index
    return None


# --- SR-12: every configuration surface documents itself --------------------

CLAP_ARGUMENT = re.compile(r"^\s*#\[arg\(")
VALUE_ENUM_DERIVE = re.compile(r"^\s*#\[derive\([^)]*\bValueEnum\b")
ENUM_VARIANT = re.compile(r"^\s{4}([A-Z][A-Za-z0-9_]*)\s*(?:[,{(]|$)")
# The three outer-doc spellings clap reads as help text. `#[doc = "..."]` is
# what `///` desugars to and `/** ... */` is its block form; all three reach
# `--help`, so a rule that saw only `///` would report documented arguments.
# `#[doc(hidden)]` and the other `#[doc(...)]` forms carry no text and are not
# among them.
DOC_ATTRIBUTE = re.compile(r"^#\[\s*doc\s*=")
BLOCK_DOC_OPENER = re.compile(r"^/\*\*(?![*/])")


def check_documented_configuration(repository: Repository) -> Iterator[Finding]:
    """A flag with no help text ships a blank line in `--help`."""
    for source in repository.sources(RUST_SOURCE_GLOBS):
        if "clap::" not in source.text and "use clap" not in source.text:
            continue
        for index, line in enumerate(source.lines):
            if CLAP_ARGUMENT.match(line) and not _documented_above(source.lines, index):
                yield Finding(
                    "SR-12",
                    source.path,
                    index + 1,
                    "clap argument carries no doc comment",
                )
            if not VALUE_ENUM_DERIVE.match(line):
                continue
            for offset, variant in _enum_variants(source.lines, index):
                if not _documented_above(source.lines, offset):
                    yield Finding(
                        "SR-12",
                        source.path,
                        offset + 1,
                        f"`ValueEnum` variant `{variant}` carries no doc comment",
                    )


def _documented_above(lines: list[str], index: int) -> bool:
    """Whether an outer doc comment, in any of its spellings, precedes a line."""
    for candidate in range(index - 1, -1, -1):
        stripped = lines[candidate].strip()
        if stripped.startswith("///"):
            return True
        if DOC_ATTRIBUTE.match(stripped):
            return True
        if stripped.endswith("*/"):
            return _block_doc_opens_above(lines, candidate)
        if stripped.startswith("#["):
            continue
        return False
    return False


def _block_doc_opens_above(lines: list[str], closing: int) -> bool:
    """Whether the block comment closing on a line opened as an outer doc."""
    for candidate in range(closing, -1, -1):
        stripped = lines[candidate].lstrip()
        if BLOCK_DOC_OPENER.match(stripped):
            return True
        if stripped.startswith("/*"):
            return False
    return False


def _enum_variants(lines: list[str], declaration: int) -> Iterator[tuple[int, str]]:
    opened = False
    for index in range(declaration, len(lines)):
        line = lines[index]
        if not opened:
            if line.rstrip().endswith("{"):
                opened = True
            continue
        if line.startswith("}"):
            return
        match = ENUM_VARIANT.match(line)
        if match is not None:
            yield index, match.group(1)


# --- SR-13: proc-macro diagnostics point at the user's tokens ---------------

CALL_SITE = re.compile(r"\bSpan::call_site\(\)")
PROC_MACRO_FLAG = re.compile(r"^\s*proc-macro\s*=\s*true\s*$", re.MULTILINE)
# The rule forbids the call site as a *diagnostic's* span, not the span itself:
# a generated token has no user tokens to point at, so `Ident::new("helper",
# Span::call_site())` is the correct spelling and reporting it would block
# conforming code. A finding therefore needs the call site to be an argument of
# an error or diagnostic construction, which is where the span becomes what a
# compiler shows the caller.
DIAGNOSTIC_CONSTRUCTOR = re.compile(
    r"(?:\b(?:Error|Diagnostic)\s*::\s*(?:new|new_spanned|spanned)"
    r"|\b(?:abort|abort_call_site|emit_error|emit_warning|emit_call_site_error)\s*!)"
    r"\s*$"
)
# How far back the callee is looked for. A path spelled out in full
# (`proc_macro_error::abort!`) fits; a line break between the callee and its
# `(` is not a spelling this repository writes.
CONSTRUCTOR_WINDOW = 96


def check_proc_macro_spans(repository: Repository) -> Iterator[Finding]:
    """A diagnostic at the call site cannot show the offending token."""
    # Matched as a key and a value rather than as one spelling: `proc-macro=true`
    # is the same manifest, and an exact-substring test would drop the whole
    # crate from the inventory rather than report a rule it could not decide.
    crates = [
        path.parent
        for path in repository.files(("crates/*/Cargo.toml",))
        if PROC_MACRO_FLAG.search(path.read_text(encoding="utf-8"))
    ]
    for crate in crates:
        label = crate.relative_to(repository.root).as_posix()
        for source in repository.sources((f"{label}/src/*.rs",)):
            for match in CALL_SITE.finditer(source.code):
                if not _spans_a_diagnostic(source.code, match.start()):
                    continue
                yield Finding(
                    "SR-13",
                    source.path,
                    source.line_of(match.start()),
                    "diagnostic spanned on the macro call site, not the tokens",
                )


def _spans_a_diagnostic(code: str, offset: int) -> bool:
    """Whether the call enclosing an offset constructs a diagnostic.

    Only the immediately enclosing call is read. A span bound to a local first
    and passed in later reads as conforming here, which is the same
    best-effort coverage every rule in this file offers: the checker declines
    to report what it cannot see rather than reporting a shape that is correct.
    """
    opener = _enclosing_call(code, offset)
    if opener is None:
        return False
    window = code[max(0, opener - CONSTRUCTOR_WINDOW) : opener]
    return DIAGNOSTIC_CONSTRUCTOR.search(window) is not None


def _enclosing_call(code: str, offset: int) -> int | None:
    """The offset of the `(` opening the argument list an offset sits in."""
    depth = 0
    for index in range(offset - 1, -1, -1):
        character = code[index]
        if character in ")]}":
            depth += 1
        elif character in "([{":
            if depth:
                depth -= 1
            elif character == "(":
                return index
            else:
                return None
    return None


RULES: tuple[tuple[str, str, Callable[[Repository], Iterator[Finding]]], ...] = (
    (
        "SR-8",
        "table access belongs to the persistence crate",
        check_app_sql_table_access,
    ),
    (
        "SR-12",
        "configuration surfaces document themselves",
        check_documented_configuration,
    ),
    (
        "SR-13",
        "proc-macro diagnostics span the user's tokens",
        check_proc_macro_spans,
    ),
)


def audit(root: Path, selected: frozenset[str] | None = None) -> list[Finding]:
    repository = Repository(root)
    findings: list[Finding] = []
    for identifier, _, check in RULES:
        if selected is not None and identifier not in selected:
            continue
        findings.extend(check(repository))
    if repository.inspected == 0:
        raise InventoryError("git ls-files matched no files under any scanned tree")
    return sorted(
        findings, key=lambda finding: (finding.rule, finding.path, finding.line)
    )


def main() -> int:
    repository_root = Path(__file__).resolve().parents[1]
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--root", type=Path, default=repository_root)
    parser.add_argument(
        "--rule",
        action="append",
        help="run only the named rule; repeatable",
    )
    parser.add_argument(
        "--counts-only",
        action="store_true",
        help="print the per-rule table without the individual findings",
    )
    arguments = parser.parse_args()
    selected = frozenset(arguments.rule) if arguments.rule else None
    if selected is not None:
        known = {identifier for identifier, _, _ in RULES}
        unknown = sorted(selected - known)
        if unknown:
            print(f"unknown rule(s): {', '.join(unknown)}", file=sys.stderr)
            return 2
    try:
        findings = audit(arguments.root.resolve(), selected)
    except (InventoryError, OSError) as error:
        print(f"style-rule check failed: {error}", file=sys.stderr)
        return 2
    counts = Counter(finding.rule for finding in findings)
    print("style-rule counts:")
    for identifier, summary, _ in RULES:
        if selected is not None and identifier not in selected:
            continue
        print(f"  {identifier:>5}  {counts[identifier]:>6}  {summary}")
    print(f"  total  {len(findings):>6}")
    if not findings:
        print("style-rule check passed")
        return 0
    if not arguments.counts_only:
        print("style-rule findings:")
        for finding in findings:
            print(f"  - {finding.render()}")
    return 1


if __name__ == "__main__":
    raise SystemExit(main())
