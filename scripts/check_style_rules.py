#!/usr/bin/env python3
"""Report the mechanically checkable half of the house style guide.

Ground truth is the tracked working tree plus `crates/persistence/migrations`,
never a recorded inventory: a rule that counts its own violations from a
checked-in baseline stops seeing the next one. Every rule below restates a
convention `docs/style.md` states normatively, and each finding names the file,
the line, and the fact observed there.

This checker is deliberately report-only while the tree still violates the
conventions it just adopted. Promotion to a blocking gate is a separate,
deliberate step: once a rule's count reaches zero it gains a workflow step that
runs `--rule` on it alone and tolerates no failure, while the report-only
invocation goes on running every rule. Promotion is never a side effect of a
change that merely reduces a count.

Reliability over coverage. A rule appears here only when a text scan decides it
with few enough false positives that a reader can trust a count without opening
the files. Rules whose decision needs type resolution, cross-crate body
comparison, or a judgment about which enum a scrutinee belongs to are left to
review and to Clippy, which sees the compiler's own answers; `docs/style.md`
records which of those are deferred and why.

Rules, each identified by the `SR-` tag its report line carries:

- SR-1  every Rust module and Swift source file opens with a file-level doc
        comment
- SR-2  no comment cites a process artifact as its authority
- SR-3  a type is spelled one way per file: imported, or crate-qualified
- SR-4  a public `*Error`/`*Failure`/`*Defect`/`*Exceeded` type implements
        `Display` and `Error`
- SR-5  no `bool` in a public struct field or public parameter of the domain
        and application crates
- SR-6  no SQL row decoded as an anonymous tuple
- SR-7  a stored storage version is never compared against the module's
        current `STORAGE_VERSION`
- SR-8  no code under `apps/` writes SQL naming a persistence-owned table
- SR-9  a migration superseding a constraint names the migration it supersedes,
        and migration filenames sort identically lexically and numerically
- SR-10 no two adjacent parameters of a public function share a type
- SR-11 no function body exceeds 400 lines
- SR-12 every clap argument and `ValueEnum` variant carries a doc comment
- SR-13 no proc-macro diagnostic is spanned on `Span::call_site()`

Run from anywhere; `--root` selects the tree. Exits nonzero when any rule
reports, after printing a per-rule count table so a caller that only wants the
baseline can read it without the findings.
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
# modules; a rule that decides how a test body reads would be restating
# `docs/agents/testing-style.md`, which owns that.
RUST_SOURCE_GLOBS = ("crates/*/src/*.rs", "apps/*/src/*.rs")
# Scanned by the rules the guide states without qualification, and only those.
# A rule reads test targets when its own text does: SR-1 says "every Rust
# module" and SR-3 says "a name has one spelling per file", and the guide opens
# by binding its two disciplines to "production and test code alike". SR-6 and
# SR-8 say "production", and the description holds their test-target scope for
# an owner ruling, so they stay on `src`. SR-4, SR-5, and SR-10 decide public
# API, which a test target does not have, and a rule about how long a test body
# runs would be `docs/agents/testing-style.md` restated.
RUST_TEST_TARGET_GLOBS = ("crates/*/tests/*.rs", "apps/*/tests/*.rs")
RUST_ALL_GLOBS = ("crates/*.rs", "apps/*.rs")
SWIFT_SOURCE_GLOBS = ("clients/native/Sources/*.swift",)
# Production code under `apps/` only. Whether the rule also binds the daemon's
# integration tests, which assert durable state through raw SQL today, is an
# open scope question; scanning them would report a change nobody has decided
# to make.
APPS_GLOBS = ("apps/*/src/*.rs",)
MIGRATION_GLOB = "crates/*/migrations/*.sql"

# The one binary the SQL-ownership rule excepts: a diagnostic tool whose whole
# purpose is to read durable rows the repository types do not yet project.
SQL_RULE_EXCEPTION = "apps/signalboxd/src/bin/signalbox-debug.rs"

MAX_FUNCTION_BODY_LINES = 400


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
    maps to the original line. `comments` carries the inverse view: the comment
    text alone, with the line it starts on.
    """

    path: str
    text: str

    def __post_init__(self) -> None:
        self.lines = self.text.splitlines()
        self.code, comment_spans = _strip_rust(self.text)
        self.comments = comment_spans

    def line_of(self, offset: int) -> int:
        return self.code.count("\n", 0, offset) + 1


def _strip_rust(text: str) -> tuple[str, list[tuple[int, str]]]:
    """Blank comments and string contents; return the code view and comments.

    Written as an explicit scanner rather than a regex because Rust's raw string
    literals (`r#"..."#`) and its reuse of the apostrophe for both lifetimes and
    character literals are not a regular language, and a regex that gets either
    wrong silently moves findings into or out of string bodies.
    """
    code = list(text)
    comments: list[tuple[int, str]] = []
    index = 0
    length = len(text)
    line = 1
    while index < length:
        char = text[index]
        if char == "\n":
            line += 1
            index += 1
            continue
        if char == "/" and index + 1 < length and text[index + 1] == "/":
            end = text.find("\n", index)
            end = length if end == -1 else end
            comments.append((line, text[index:end]))
            for position in range(index, end):
                code[position] = " "
            index = end
            continue
        if char == "/" and index + 1 < length and text[index + 1] == "*":
            end = text.find("*/", index + 2)
            end = length if end == -1 else end + 2
            comments.append((line, text[index:end]))
            for position in range(index, end):
                if code[position] != "\n":
                    code[position] = " "
            line += text.count("\n", index, end)
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
                line += text.count("\n", index, end)
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
            line += text.count("\n", index, min(cursor + 1, length))
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
    return "".join(code), comments


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


# --- SR-1: file-level doc comments -----------------------------------------

def check_file_doc_comments(repository: Repository) -> Iterator[Finding]:
    """A file states what it owns before it states anything else."""
    for source in repository.sources((*RUST_SOURCE_GLOBS, *RUST_TEST_TARGET_GLOBS)):
        if not _opens_with(source.lines, "//!", skippable=("#![", "//")):
            yield Finding(
                "SR-1", source.path, 1, "Rust module has no `//!` file doc comment"
            )
    for source in repository.sources(SWIFT_SOURCE_GLOBS):
        if not _opens_with(source.lines, "///", skippable=("//",)):
            yield Finding(
                "SR-1", source.path, 1, "Swift file has no `///` file doc comment"
            )


def _opens_with(lines: list[str], marker: str, skippable: tuple[str, ...]) -> bool:
    for line in lines:
        stripped = line.strip()
        if not stripped:
            continue
        if stripped.startswith(marker):
            return True
        if any(stripped.startswith(prefix) for prefix in skippable):
            continue
        return False
    return False


# --- SR-2: comments cite constraints, not process artifacts -----------------

# Each pattern names a citation form the guide forbids outright. Deliberately
# absent: any pattern over pull requests, which this repository's own domain
# vocabulary and live fixtures name as subjects rather than as authorities, and
# the word "audit", which names durable tables. Matching either would drown the
# real citations in vocabulary. A date inside backticks is a value the code
# carries — an API version spelling, say — not a citation, so it is skipped.
PROCESS_CITATIONS = (
    ("a process document under docs/agents/", re.compile(r"docs/agents/")),
    ("the retired decision ledger", re.compile(r"docs/decisions\.md")),
    ("a numbered rule", re.compile(r"\brules?\s+\d+", re.IGNORECASE)),
    ("a review wave", re.compile(r"\bwave\s+\d+", re.IGNORECASE)),
    (
        "an owner process event",
        re.compile(r"\bowner\s+(?:ruling|decision|approval)\b", re.IGNORECASE),
    ),
    ("a bare date", re.compile(r"(?<!`)\b20\d{2}-\d{2}-\d{2}\b(?!`)")),
)


SQL_COMMENT = re.compile(r"--[^\n]*")
SQL_STRING = re.compile(r"'(?:[^']|'')*'")


def _sql_comments(text: str) -> Iterator[tuple[int, str]]:
    """Yield each SQL line comment with the line it starts on.

    String bodies are blanked first, preserving their newlines so every offset
    still maps to its original line: a `--` inside a literal is data a migration
    writes, not a comment a reader is being addressed by.
    """
    scrubbed = SQL_STRING.sub(
        lambda match: re.sub(r"[^\n]", " ", match.group()), text
    )
    for match in SQL_COMMENT.finditer(scrubbed):
        yield text.count("\n", 0, match.start()) + 1, text[match.start() : match.end()]


def check_comment_provenance(repository: Repository) -> Iterator[Finding]:
    """A comment states a constraint; provenance belongs to git history."""
    patterns = (*RUST_ALL_GLOBS, *SWIFT_SOURCE_GLOBS)
    commented: list[tuple[str, Iterator[tuple[int, str]]]] = [
        (source.path, iter(source.comments)) for source in repository.sources(patterns)
    ]
    # A migration is code, and its comments address the same reader. They are
    # where a durable bound's provenance is most tempting to write down and
    # where a citation outlives the document it names longest.
    for path in repository.files((MIGRATION_GLOB,)):
        label = path.relative_to(repository.root).as_posix()
        commented.append((label, _sql_comments(path.read_text(encoding="utf-8"))))
    for path, comments in commented:
        for line, comment in comments:
            for label, pattern in PROCESS_CITATIONS:
                match = pattern.search(comment)
                if match is None:
                    continue
                yield Finding(
                    "SR-2",
                    path,
                    line + comment.count("\n", 0, match.start()),
                    f"comment cites {label}: {match.group().strip()}",
                )
                break


# --- SR-3: one spelling per type per file -----------------------------------

USE_STATEMENT = re.compile(r"^\s*(?:pub\s+)?use\s+([^;]+);", re.MULTILINE)
WORKSPACE_PATH = re.compile(r"\bsignalbox_[a-z0-9_]+::([A-Z][A-Za-z0-9_]*)")
TYPE_NAME = re.compile(r"\b([A-Z][A-Za-z0-9_]*)\b")


def check_single_type_spelling(repository: Repository) -> Iterator[Finding]:
    """A type imported into a file is never also spelled crate-qualified.

    The crate each name was imported from is recorded alongside the name, so a
    file that deliberately disambiguates two same-named types from different
    crates — the one case where both spellings are load-bearing — does not
    report.
    """
    for source in repository.sources((*RUST_SOURCE_GLOBS, *RUST_TEST_TARGET_GLOBS)):
        imported: dict[str, set[str]] = {}
        import_spans: list[tuple[int, int]] = []
        for statement in USE_STATEMENT.finditer(source.code):
            import_spans.append(statement.span())
            body = statement.group(1)
            origin = body.strip().split("::", 1)[0].strip()
            for segment in body.replace("{", " ").replace("}", " ").split(","):
                trailing = segment.strip().rsplit("::", 1)[-1].strip()
                if TYPE_NAME.fullmatch(trailing):
                    imported.setdefault(trailing, set()).add(origin)
        if not imported:
            continue
        for match in WORKSPACE_PATH.finditer(source.code):
            crate = match.group().split("::", 1)[0]
            if crate not in imported.get(match.group(1), frozenset()):
                continue
            if any(start <= match.start() < end for start, end in import_spans):
                continue
            yield Finding(
                "SR-3",
                source.path,
                source.line_of(match.start()),
                f"`{match.group()}` is also imported by name in this file",
            )


# --- SR-4: public failure types are renderable ------------------------------

FAILURE_DECLARATION = re.compile(
    r"^\s*pub\s+(?:enum|struct)\s+"
    r"([A-Za-z0-9_]*(?:Error|Failure|Defect|Exceeded))\b",
    re.MULTILINE,
)
DISPLAY_IMPL = re.compile(
    r"\bimpl(?:\s*<[^>]*>)?\s+(?:[A-Za-z0-9_:]*::)?Display\s+for\s+([A-Za-z0-9_]+)"
)
ERROR_IMPL = re.compile(
    r"\bimpl(?:\s*<[^>]*>)?\s+(?:[A-Za-z0-9_:]*::)?Error\s+for\s+([A-Za-z0-9_]+)"
)


def check_failure_type_rendering(repository: Repository) -> Iterator[Finding]:
    """A public failure type renders itself and forwards its source."""
    by_crate: dict[str, list[Source]] = {}
    for source in repository.sources(RUST_SOURCE_GLOBS):
        crate = "/".join(source.path.split("/")[:2])
        by_crate.setdefault(crate, []).append(source)
    for sources in by_crate.values():
        displays: set[str] = set()
        errors: set[str] = set()
        for source in sources:
            displays.update(
                match.group(1) for match in DISPLAY_IMPL.finditer(source.code)
            )
            errors.update(
                match.group(1) for match in ERROR_IMPL.finditer(source.code)
            )
        for source in sources:
            for match in FAILURE_DECLARATION.finditer(source.code):
                name = match.group(1)
                missing = [
                    label
                    for label, known in (("Display", displays), ("Error", errors))
                    if name not in known
                ]
                if missing:
                    yield Finding(
                        "SR-4",
                        source.path,
                        source.line_of(match.start()),
                        f"`{name}` implements neither "
                        f"{' nor '.join(missing)}"
                        if len(missing) == 2
                        else f"`{name}` does not implement {missing[0]}",
                    )


# --- SR-5: no public boolean axes in the domain and application crates ------

BOOLEAN_TREES = ("crates/domain/src/*.rs", "crates/application/src/*.rs")
PUBLIC_BOOL_FIELD = re.compile(
    r"^\s*pub\s+([a-z_][a-z0-9_]*)\s*:\s*bool\s*,", re.MULTILINE
)


def check_public_boolean_axes(repository: Repository) -> Iterator[Finding]:
    """A public boolean is a question the call site cannot see."""
    for source in repository.sources(BOOLEAN_TREES):
        for match in PUBLIC_BOOL_FIELD.finditer(source.code):
            yield Finding(
                "SR-5",
                source.path,
                source.line_of(match.start()),
                f"public field `{match.group(1)}: bool` erases its question",
            )
        for signature in public_signatures(source):
            for name, spelling in signature.parameters:
                if spelling == "bool":
                    yield Finding(
                        "SR-5",
                        source.path,
                        signature.line,
                        f"public parameter `{name}: bool` in "
                        f"`{signature.name}` erases its question",
                    )


# --- SR-6: rows carry labels ------------------------------------------------

TUPLE_ALIAS = re.compile(
    r"^[ \t]*(?:pub(?:\s*\([^)]*\))?\s+)?type\s+([A-Za-z0-9_]+)\s*=\s*\(",
    re.MULTILINE,
)
# Bounded to the line: the row type in a turbofish is written on one, and an
# unbounded scan would run to the next `>` in the file and read an unrelated
# name as this projection's.
QUERY_AS_TURBOFISH = re.compile(r"query_as::<\s*_\s*,\s*([^>;\n]+)")
QUERY_AS_BINDING = re.compile(
    r"\blet\s+[A-Za-z0-9_]+\s*:\s*([^=;]+?)\s*=\s*sqlx::query_as"
)
IDENTIFIER = re.compile(r"\b[A-Za-z_][A-Za-z0-9_]*\b")


def check_anonymous_row_decoding(repository: Repository) -> Iterator[Finding]:
    """A projected row is a record with names, not a run of positions."""
    for source in repository.sources(RUST_SOURCE_GLOBS):
        # A tuple hidden behind `type Row = (..)` is the same run of positions
        # with a name on the outside, and the guide forbids it in the same
        # sentence. Aliases are collected per file, which is where the alias and
        # its `query_as` sit in every case this reports; an alias imported from
        # another module needs a name resolver, not a scan.
        aliases = {match.group(1) for match in TUPLE_ALIAS.finditer(source.code)}
        reported: dict[int, str] = {}
        for pattern in (QUERY_AS_TURBOFISH, QUERY_AS_BINDING):
            for match in pattern.finditer(source.code):
                detail = _anonymous_row_shape(match.group(1), aliases)
                if detail is None:
                    continue
                # One projection can match both spellings — an annotated
                # binding whose call also carries a turbofish — and it is one
                # finding.
                reported.setdefault(source.line_of(match.start()), detail)
        for line, detail in sorted(reported.items()):
            yield Finding("SR-6", source.path, line, detail)


def _anonymous_row_shape(annotation: str, aliases: set[str]) -> str | None:
    """Name the anonymous shape a projection decodes into, if it is one."""
    if "(" in annotation and "," in annotation:
        return "SQL row decoded as an anonymous tuple"
    for name in IDENTIFIER.findall(annotation):
        if name in aliases:
            return f"SQL row decoded as an anonymous tuple aliased `{name}`"
    return None


# --- SR-7: schema thresholds are their own constants ------------------------

# Equality as well as ordering: the rule is that a stored version is compared
# only against a named feature threshold, and `stored != STORAGE_VERSION` is the
# comparison whose harm the rule states most exactly — advancing the writer
# version turns every row written under the old one into corruption.
STORAGE_VERSION_COMPARISON = re.compile(
    r"(?:(?:[<>!=]=|[<>])\s*STORAGE_VERSION"
    r"|STORAGE_VERSION\s*(?:[<>!=]=|[<>][^=])"
    r"|\.\.=?\s*STORAGE_VERSION)"
)
STORAGE_VERSION_TREES = ("crates/persistence/src/*.rs",)


def check_storage_version_thresholds(repository: Repository) -> Iterator[Finding]:
    """A feature threshold is not the version this module currently writes."""
    for source in repository.sources(STORAGE_VERSION_TREES):
        for match in STORAGE_VERSION_COMPARISON.finditer(source.code):
            yield Finding(
                "SR-7",
                source.path,
                source.line_of(match.start()),
                "stored version compared against the module's own STORAGE_VERSION",
            )


# --- SR-8: table access belongs to the persistence crate --------------------

CREATE_TABLE = re.compile(
    r"\bcreate\s+table\s+(?:if\s+not\s+exists\s+)?([a-z_][a-z0-9_]*)", re.IGNORECASE
)
# Every SQL form that names a table, not only the ones a query uses: the rule
# forbids naming a table at all, so a DDL statement admitted under `apps/` after
# the count reached zero would otherwise never be reported. The two-word forms
# precede the bare `truncate` so the alternation cannot stop at the verb.
SQL_TABLE_REFERENCE = re.compile(
    r"\b(?:from|join|into|update"
    r"|(?:alter|drop|create|truncate)\s+table"
    r"|truncate)\s+"
    r"(?:if\s+not\s+exists\s+|if\s+exists\s+|only\s+)?"
    r"\"?([a-z_][a-z0-9_]*)\"?",
    re.IGNORECASE,
)


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
        for line, literal in _string_literals(source.text):
            for match in SQL_TABLE_REFERENCE.finditer(literal):
                if match.group(1).lower() not in tables:
                    continue
                yield Finding(
                    "SR-8",
                    source.path,
                    line,
                    f"SQL under apps/ names the table `{match.group(1)}`",
                )


def _string_literals(text: str) -> Iterator[tuple[int, str]]:
    """Yield each string literal body with the line it starts on."""
    for match in re.finditer(r'r#*"(?:[^"]|"(?!#))*"#*|"(?:\\.|[^"\\])*"', text, re.S):
        yield text.count("\n", 0, match.start()) + 1, match.group()


# --- SR-9: migrations name what they supersede and sort one way -------------

MIGRATION_NAME = re.compile(r"^(\d+)_")
DROP_CONSTRAINT = re.compile(r"\bDROP\s+CONSTRAINT\b", re.IGNORECASE)
SUPERSEDED_MIGRATION = re.compile(r"\b\d{8,}_[a-z0-9_]+")


def _attributing_comments(lines: list[str], index: int) -> list[str]:
    """The comments that can attribute the `DROP CONSTRAINT` on `index`.

    The statement is the unit, not a fixed distance. One `ALTER TABLE` carries
    as many clauses as it needs — six drops in a single statement exist in this
    repository's own migrations — and a comment above the statement attributes
    every one of them; a window measured in lines would instead demand the same
    sentence copied beside each clause, which the guide does not ask for and
    which no reader would want. The statement begins after the last line that
    closed one, and a blank line bounds the attribution above it so a comment
    belonging to something else is not read as this statement's.
    """
    start = index
    while start > 0:
        previous = lines[start - 1].strip()
        if not previous or previous.endswith(";"):
            break
        start -= 1
    return [
        candidate.strip()
        for candidate in lines[start:index]
        if candidate.strip().startswith("--")
    ]


def check_migration_supersession(repository: Repository) -> Iterator[Finding]:
    """A re-issued constraint names the definition it replaces, and files sort."""
    paths = repository.files((MIGRATION_GLOB,))
    for path in paths:
        label = path.relative_to(repository.root).as_posix()
        lines = path.read_text(encoding="utf-8").splitlines()
        for index, line in enumerate(lines):
            if DROP_CONSTRAINT.search(line) is None:
                continue
            if any(
                SUPERSEDED_MIGRATION.search(comment)
                for comment in _attributing_comments(lines, index)
            ):
                continue
            yield Finding(
                "SR-9",
                label,
                index + 1,
                "DROP CONSTRAINT does not name the migration it supersedes",
            )
    by_directory: dict[str, list[Path]] = {}
    for path in paths:
        by_directory.setdefault(path.parent.as_posix(), []).append(path)
    for directory, entries in sorted(by_directory.items()):
        named = sorted(entries, key=lambda entry: entry.name)
        numbered = sorted(
            entries,
            key=lambda entry: (
                int(match.group(1))
                if (match := MIGRATION_NAME.match(entry.name))
                else -1
            ),
        )
        for lexical, numeric in zip(named, numbered):
            if lexical.name != numeric.name:
                yield Finding(
                    "SR-9",
                    Path(directory).relative_to(repository.root).as_posix()
                    if Path(directory).is_absolute()
                    else directory,
                    1,
                    f"lexical and numeric ordering disagree at `{lexical.name}` "
                    f"versus `{numeric.name}`",
                )
                break


# --- SR-10: adjacent parameters differ in type ------------------------------

@dataclass(frozen=True)
class Signature:
    """One parsed function signature: its name, line, and typed parameters."""

    name: str
    line: int
    parameters: tuple[tuple[str, str], ...]


PUBLIC_FUNCTION = re.compile(
    r"\bpub\s+(?:const\s+)?(?:async\s+)?(?:unsafe\s+)?(?:extern\s+\"[^\"]*\"\s+)?"
    r"fn\s+([A-Za-z0-9_]+)\s*(?:<[^>({]*>)?\s*\("
)


# A trait method carries no `pub`: the trait's visibility is the method's. A
# public trait's methods are public API, and a transposition at a call site
# compiles there exactly as it would for a free function, so the rule reaches
# them and the pattern above cannot.
PUBLIC_TRAIT = re.compile(r"\bpub\s+(?:unsafe\s+)?trait\s+[A-Za-z0-9_]+")
TRAIT_METHOD = re.compile(
    r"\b(?:const\s+)?(?:async\s+)?(?:unsafe\s+)?"
    r"fn\s+([A-Za-z0-9_]+)\s*(?:<[^>({]*>)?\s*\("
)


def public_signatures(source: Source) -> Iterator[Signature]:
    """Parse every publicly reachable function signature in a file."""
    yield from _signatures(source, PUBLIC_FUNCTION)
    for trait in PUBLIC_TRAIT.finditer(source.code):
        opening = source.code.find("{", trait.end())
        if opening == -1:
            continue
        closing = _matching_brace(source.code, opening)
        if closing is None:
            continue
        yield from _signatures(source, TRAIT_METHOD, opening, closing)


def _signatures(
    source: Source,
    pattern: re.Pattern[str],
    start: int = 0,
    end: int | None = None,
) -> Iterator[Signature]:
    for match in pattern.finditer(
        source.code, start, len(source.code) if end is None else end
    ):
        opening = source.code.index("(", match.end() - 1)
        closing = _matching_paren(source.code, opening)
        if closing is None:
            continue
        body = source.code[opening + 1 : closing]
        parameters: list[tuple[str, str]] = []
        for fragment in _split_top_level(body):
            fragment = fragment.strip()
            if not fragment or fragment.endswith("self") or fragment == "self":
                continue
            if ":" not in fragment:
                continue
            name, _, spelling = fragment.partition(":")
            parameters.append((name.strip(), " ".join(spelling.split())))
        yield Signature(
            match.group(1), source.line_of(match.start()), tuple(parameters)
        )


def _matching_paren(text: str, opening: int) -> int | None:
    depth = 0
    for index in range(opening, len(text)):
        if text[index] == "(":
            depth += 1
        elif text[index] == ")":
            depth -= 1
            if depth == 0:
                return index
    return None


def _split_top_level(body: str) -> list[str]:
    """Split a parameter list on the commas that are not inside a nesting.

    Angle brackets nest here too, so `HashMap<K, V>` stays one parameter; a `>`
    that closes an arrow (`->`, `=>`) is not a nesting close and is skipped,
    which is the only place the two spellings collide in a signature.
    """
    fragments: list[str] = []
    depth = 0
    start = 0
    for index, char in enumerate(body):
        if char in "([{<":
            if char == "<" and index and body[index - 1] in "-=":
                continue
            depth += 1
        elif char in ")]}>":
            if char == ">" and index and body[index - 1] in "-=":
                continue
            depth -= 1
        elif char == "," and depth == 0:
            fragments.append(body[start:index])
            start = index + 1
    fragments.append(body[start:])
    return fragments


def check_adjacent_parameter_types(repository: Repository) -> Iterator[Finding]:
    """Two adjacent parameters of one type let a transposition compile."""
    for source in repository.sources(RUST_SOURCE_GLOBS):
        for signature in public_signatures(source):
            for (first, spelling), (second, following) in zip(
                signature.parameters, signature.parameters[1:]
            ):
                if spelling and spelling == following:
                    yield Finding(
                        "SR-10",
                        source.path,
                        signature.line,
                        f"`{signature.name}` takes `{first}` and `{second}` "
                        f"both as `{spelling}`",
                    )


# --- SR-11: function bodies stay readable -----------------------------------

FUNCTION_DECLARATION = re.compile(r"\bfn\s+([A-Za-z0-9_]+)\s*[<(]")


def check_function_body_length(repository: Repository) -> Iterator[Finding]:
    """A body past the ceiling has shared mutable state no name describes."""
    for source in repository.sources(RUST_SOURCE_GLOBS):
        for match in FUNCTION_DECLARATION.finditer(source.code):
            opening = _body_start(source.code, match.end())
            if opening is None:
                continue
            closing = _matching_brace(source.code, opening)
            if closing is None:
                continue
            span = source.code.count("\n", opening, closing) + 1
            if span > MAX_FUNCTION_BODY_LINES:
                yield Finding(
                    "SR-11",
                    source.path,
                    source.line_of(match.start()),
                    f"`{match.group(1)}` body is {span} lines "
                    f"(ceiling {MAX_FUNCTION_BODY_LINES})",
                )


def _body_start(text: str, after: int) -> int | None:
    """Find the brace opening a function body, or None for a declaration."""
    depth = 0
    for index in range(after, len(text)):
        char = text[index]
        if char in "([<":
            depth += 1
        elif char in ")]>":
            depth -= 1
        elif char == ";" and depth <= 0:
            return None
        elif char == "{" and depth <= 0:
            return index
    return None


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
    for candidate in range(index - 1, -1, -1):
        stripped = lines[candidate].strip()
        if stripped.startswith("///"):
            return True
        if stripped.startswith("#["):
            continue
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


def check_proc_macro_spans(repository: Repository) -> Iterator[Finding]:
    """A diagnostic at the call site cannot show the offending token."""
    crates = [
        path.parent
        for path in repository.files(("crates/*/Cargo.toml",))
        if "proc-macro = true" in path.read_text(encoding="utf-8")
    ]
    for crate in crates:
        label = crate.relative_to(repository.root).as_posix()
        for source in repository.sources((f"{label}/src/*.rs",)):
            for match in CALL_SITE.finditer(source.code):
                yield Finding(
                    "SR-13",
                    source.path,
                    source.line_of(match.start()),
                    "diagnostic spanned on the macro call site, not the tokens",
                )


RULES: tuple[tuple[str, str, Callable[[Repository], Iterator[Finding]]], ...] = (
    ("SR-1", "file-level doc comments", check_file_doc_comments),
    (
        "SR-2",
        "comments cite constraints, not process artifacts",
        check_comment_provenance,
    ),
    ("SR-3", "one spelling per name per file", check_single_type_spelling),
    (
        "SR-4",
        "public failure types render and forward",
        check_failure_type_rendering,
    ),
    (
        "SR-5",
        "no public boolean axes in domain and application",
        check_public_boolean_axes,
    ),
    ("SR-6", "rows decode into named records", check_anonymous_row_decoding),
    (
        "SR-7",
        "schema thresholds are their own constants",
        check_storage_version_thresholds,
    ),
    (
        "SR-8",
        "table access belongs to the persistence crate",
        check_app_sql_table_access,
    ),
    (
        "SR-9",
        "migrations name supersession and sort one way",
        check_migration_supersession,
    ),
    ("SR-10", "adjacent parameters differ in type", check_adjacent_parameter_types),
    (
        "SR-11",
        f"function bodies stay under {MAX_FUNCTION_BODY_LINES} lines",
        check_function_body_length,
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
    print("style-rule counts (report only):")
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
