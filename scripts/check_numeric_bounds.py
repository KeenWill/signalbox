#!/usr/bin/env python3
"""Inventory numeric bound constants and enforce their declarations.

The inventory is every integer, float, ``Duration``, or ``NonZero*`` Rust
constant under ``crates/*/src`` and ``apps/*/src`` whose name contains one of
the boundary tokens in ``BOUNDARY_TOKENS``. Ordinary Rust spellings of those
types count: a qualified path such as ``std::time::Duration`` and the signed
``NonZeroI*`` family are inventoried alongside their bare and unsigned
spellings, so no accepted spelling carries an undeclared bound past the gate.

Test-only modules are inventoried but do not gate because their constants
describe fixtures, not runtime safety. A module written ``#[cfg(test)] mod
tests;`` is test-only across the separate file it names, whether that file is
found by Rust's directory rule or named outright by a ``#[path]`` attribute.

The blocking scope is deliberately smaller than the inventory: production
constants in application orchestration, provider-neutral model runtime,
process wire contracts, the terminal client review loop, and code-host tools.
``ENFORCED_ROOTS`` is the exact scope. Other workspace bounds remain visible in
the success count but are deferred rather than silently claimed as compliant.

An in-scope declaration must be immediately preceded by one of:

    // numeric-bound: ceiling - protects against unbounded retained input
    // numeric-bound: tunable - controls the default exchange wait
    // numeric-bound: not-a-bound - fixed decimal representation maximum

``docs/style.md`` defines all three kinds and owns the semantic question of
which one a given constant deserves. The kind and one-line rationale are
mechanically required here; review decides whether they are true.

A mechanically derived bound may use the narrow escape

    // numeric-bound: derived ceiling from MAX_SOURCE_CHARACTERS

only when its initializer actually references the named, same-file bound and
that name resolves to a direct declaration of the same kind. This keeps
self-evident byte/unit translations from repeating rationale while preventing
an unexplained independent cap from hiding behind the escape.

The source name resolves in the Rust scope that declares it: the innermost
inline module containing the derived constant, then outward through enclosing
modules to the file. A sibling module's declaration is never in scope, so a
derivation whose initializer really reads an imported constant cannot be
validated against an unrelated same-named constant elsewhere in the file, and a
nearer declaration shadows a farther one exactly as Rust resolves it. What this
lexical scan cannot follow, it refuses: a name reachable only through a `use`
from outside the file leaves the escape unproven and the declaration is
rejected.

Because discovery is deliberately lexical, a fixed representation fact whose
name contains a boundary token may declare ``not-a-bound`` with a one-line
explanation. That escape is for facts such as a numeric type's exact maximum or
UTF-8's continuation width, not for a runtime cap; review owns that semantic
distinction and the marker keeps each use visible in the inventory.

Run from the repository root. ``--root`` exists only for checker self-tests.
"""

from __future__ import annotations

import argparse
import os
import re
import sys
from dataclasses import dataclass
from pathlib import Path

BOUNDARY_TOKENS = frozenset(
    {
        "BASELINE",
        "BOUND",
        "BUDGET",
        "CAP",
        "CAPACITY",
        "CEILING",
        "DEADLINE",
        "LIMIT",
        "MAX",
        "MAXIMUM",
        "MIN",
        "MINIMUM",
        "THRESHOLD",
        "TIMEOUT",
        "TTL",
    }
)
ENFORCED_ROOTS = (
    "apps/client/src",
    "crates/application/src",
    "crates/model-provider-runtime/src",
    "crates/model-runtime/src",
    "crates/process-protocol/src",
    "crates/tools-code-host/src",
)
# The optional leading qualifier makes `std::time::Duration` and bare
# `Duration` one declaration to this scan, and the signed `NonZeroI*` family is
# listed beside the unsigned one: a spelling this pattern misses is an
# undeclared bound the gate silently accepts.
NUMERIC_TYPE = (
    r"(?:[A-Za-z_][A-Za-z0-9_]*\s*::\s*)*"
    r"(?:[ui](?:8|16|32|64|128|size)|f(?:32|64)|Duration|"
    r"NonZero(?:U8|U16|U32|U64|U128|Usize|I8|I16|I32|I64|I128|Isize))"
)
CONSTANT = re.compile(
    rf"(?m)^(?P<indent>[ \t]*)(?:pub(?:\([^)]*\))?\s+)?const\s+"
    rf"(?P<name>[A-Z][A-Z0-9_]*)\s*:\s*(?P<type>{NUMERIC_TYPE})\s*=\s*"
)
DIRECT_DECLARATION = re.compile(
    r"^\s*// numeric-bound: (?P<kind>ceiling|tunable|not-a-bound) - "
    r"(?P<rationale>\S.*)$"
)
DERIVED_DECLARATION = re.compile(
    r"^\s*// numeric-bound: derived (?P<kind>ceiling|tunable) from "
    r"(?P<source>[A-Z][A-Z0-9_]*)\s*$"
)
TEST_MODULE = re.compile(
    r"#\s*\[\s*cfg\s*\(\s*test\s*\)\s*\]\s*"
    r"(?:pub(?:\([^)]*\))?\s+)?mod\s+[A-Za-z_][A-Za-z0-9_]*\s*\{",
    re.MULTILINE,
)
MODULE_BLOCK = re.compile(r"\bmod\s+[A-Za-z_][A-Za-z0-9_]*\s*\{")
EXTERNAL_MODULE = re.compile(
    r"(?P<attributes>(?:#\s*\[[^\]]*\]\s*)+)"
    r"(?:pub(?:\([^)]*\))?\s+)?mod\s+(?P<name>[A-Za-z_][A-Za-z0-9_]*)\s*;",
    re.MULTILINE,
)
CFG_TEST_ATTRIBUTE = re.compile(r"#\s*\[\s*cfg\s*\(\s*test\s*\)\s*\]")
PATH_ATTRIBUTE = re.compile(r"#\s*\[\s*path\s*=\s*\"(?P<path>[^\"]+)\"\s*\]")
MODULE_ROOTS = frozenset({"lib.rs", "main.rs", "mod.rs"})


@dataclass(frozen=True)
class Bound:
    path: Path
    name: str
    line: int
    offset: int
    scope: tuple[int, int] | None
    initializer: str
    annotation: str
    test_only: bool


def blank_non_code(text: str) -> str:
    """Blank Rust comments and strings while preserving offsets and newlines."""
    code = list(text)
    index = 0
    length = len(text)
    while index < length:
        if text[index : index + 2] == "//":
            end = text.find("\n", index)
            end = length if end == -1 else end
            for position in range(index, end):
                code[position] = " "
            index = end
            continue
        if text[index : index + 2] == "/*":
            depth = 1
            cursor = index + 2
            while cursor < length and depth:
                if text[cursor : cursor + 2] == "/*":
                    depth += 1
                    cursor += 2
                elif text[cursor : cursor + 2] == "*/":
                    depth -= 1
                    cursor += 2
                else:
                    cursor += 1
            for position in range(index, cursor):
                if code[position] != "\n":
                    code[position] = " "
            index = cursor
            continue
        if text[index] == "r" and text[index + 1 : index + 2] in ('"', "#"):
            cursor = index + 1
            hashes = 0
            while cursor < length and text[cursor] == "#":
                hashes += 1
                cursor += 1
            if cursor < length and text[cursor] == '"':
                terminator = '"' + "#" * hashes
                end = text.find(terminator, cursor + 1)
                end = length if end == -1 else end + len(terminator)
                for position in range(index, end):
                    if code[position] != "\n":
                        code[position] = " "
                index = end
                continue
        if text[index] == '"':
            cursor = index + 1
            while cursor < length:
                if text[cursor] == "\\":
                    cursor += 2
                    continue
                if text[cursor] == '"':
                    cursor += 1
                    break
                cursor += 1
            for position in range(index, cursor):
                if code[position] != "\n":
                    code[position] = " "
            index = cursor
            continue
        if text[index] == "'" and text[index + 2 : index + 3] == "'":
            code[index + 1] = " "
            index += 3
            continue
        index += 1
    return "".join(code)


def matching_brace(code: str, opening: int) -> int:
    depth = 0
    for position in range(opening, len(code)):
        if code[position] == "{":
            depth += 1
        elif code[position] == "}":
            depth -= 1
            if depth == 0:
                return position
    return len(code)


def test_ranges(code: str) -> list[tuple[int, int]]:
    ranges = []
    for match in TEST_MODULE.finditer(code):
        opening = code.find("{", match.start(), match.end())
        ranges.append((match.start(), matching_brace(code, opening)))
    return ranges


def module_ranges(code: str) -> list[tuple[int, int]]:
    """Report the span of every inline `mod name { ... }` block in one file."""
    ranges = []
    for match in MODULE_BLOCK.finditer(code):
        ranges.append((match.start(), matching_brace(code, match.end() - 1)))
    return ranges


def innermost_scope(offset: int, ranges: list[tuple[int, int]]) -> tuple[int, int] | None:
    """Report the narrowest module block containing ``offset``, or the file."""
    enclosing = [span for span in ranges if span[0] <= offset <= span[1]]
    return max(enclosing, default=None)


def external_test_sources(path: Path, text: str, code: str) -> set[Path]:
    """Report the sources a `#[cfg(test)] mod name;` in ``path`` owns.

    Rust reaches such a module either through an explicit ``#[path]``, which is
    relative to the declaring file's directory, or through the directory that
    file owns; both spellings are followed, and a returned directory owns every
    source beneath it. Attributes are matched against ``code`` so a
    commented-out declaration cannot claim a file, then re-read from ``text``
    because blanking has emptied the ``#[path]`` string literal.
    """
    directory = path.parent if path.name in MODULE_ROOTS else path.with_suffix("")
    owned: set[Path] = set()
    for match in EXTERNAL_MODULE.finditer(code):
        if CFG_TEST_ATTRIBUTE.search(match.group("attributes")) is None:
            continue
        attributes = text[match.start("attributes") : match.end("attributes")]
        explicit = PATH_ATTRIBUTE.search(attributes)
        if explicit is not None:
            owned.add(Path(os.path.normpath(path.parent / explicit.group("path"))))
            continue
        name = match.group("name")
        owned.add(directory / f"{name}.rs")
        owned.add(directory / name)
    return owned


def in_test_sources(path: Path, test_sources: set[Path]) -> bool:
    return path in test_sources or any(parent in test_sources for parent in path.parents)


def in_ranges(position: int, ranges: list[tuple[int, int]]) -> bool:
    return any(start <= position <= end for start, end in ranges)


def initializer_end(code: str, start: int) -> int:
    delimiter_depth = 0
    for position in range(start, len(code)):
        char = code[position]
        if char in "([{":
            delimiter_depth += 1
        elif char in ")]}":
            delimiter_depth -= 1
        elif char == ";" and delimiter_depth == 0:
            return position
    return len(code)


def is_boundary_name(name: str) -> bool:
    return bool(BOUNDARY_TOKENS.intersection(name.split("_")))


def source_files(root: Path) -> list[Path]:
    files = []
    for top_level in (root / "crates", root / "apps"):
        if top_level.exists():
            files.extend(top_level.glob("*/src/**/*.rs"))
    return sorted(path for path in files if path.is_file())


def inventory(root: Path) -> list[Bound]:
    sources = {path: path.read_text(encoding="utf-8") for path in source_files(root)}
    blanked = {path: blank_non_code(text) for path, text in sources.items()}
    test_sources: set[Path] = set()
    for path, text in sources.items():
        test_sources |= external_test_sources(path, text, blanked[path])
    bounds = []
    for path, text in sources.items():
        code = blanked[path]
        ranges = test_ranges(code)
        modules = module_ranges(code)
        external = in_test_sources(path, test_sources)
        lines = text.splitlines()
        relative = path.relative_to(root)
        for match in CONSTANT.finditer(code):
            name = match.group("name")
            if not is_boundary_name(name):
                continue
            line = text.count("\n", 0, match.start()) + 1
            end = initializer_end(code, match.end())
            annotation = lines[line - 2] if line > 1 else ""
            bounds.append(
                Bound(
                    path=relative,
                    name=name,
                    line=line,
                    offset=match.start(),
                    scope=innermost_scope(match.start(), modules),
                    initializer=code[match.end() : end],
                    annotation=annotation,
                    test_only=external or in_ranges(match.start(), ranges),
                )
            )
    return bounds


def is_enforced(bound: Bound) -> bool:
    path = bound.path.as_posix()
    return not bound.test_only and any(
        path == root or path.startswith(f"{root}/") for root in ENFORCED_ROOTS
    )


def declaration(bound: Bound) -> tuple[str, str, str | None] | None:
    direct = DIRECT_DECLARATION.fullmatch(bound.annotation)
    if direct is not None:
        return direct.group("kind"), direct.group("rationale"), None
    derived = DERIVED_DECLARATION.fullmatch(bound.annotation)
    if derived is not None:
        return derived.group("kind"), "", derived.group("source")
    return None


def validate(bounds: list[Bound]) -> list[str]:
    failures = []
    enforced = [bound for bound in bounds if is_enforced(bound)]
    declarations: dict[tuple[Path, str], list[Bound]] = {}
    for bound in enforced:
        declarations.setdefault((bound.path, bound.name), []).append(bound)

    def depth(candidate: Bound) -> int:
        return -1 if candidate.scope is None else candidate.scope[0]

    def visible_owner(bound: Bound, source: str) -> Bound | None:
        """Resolve ``source`` as the Rust scope around ``bound`` would.

        A file-level declaration is visible everywhere in its file and an inline
        module's declaration only within that module, so a sibling module never
        supplies an owner. The nearest enclosing declaration shadows the rest;
        two at the same depth would not compile, and resolve to neither here.
        """
        visible = [
            candidate
            for candidate in declarations.get((bound.path, source), ())
            if candidate.scope is None
            or candidate.scope[0] <= bound.offset <= candidate.scope[1]
        ]
        if not visible:
            return None
        nearest = [
            candidate for candidate in visible if depth(candidate) == max(map(depth, visible))
        ]
        return nearest[0] if len(nearest) == 1 else None

    def resolves_to_direct(bound: Bound, kind: str, seen: set[str]) -> bool:
        parsed = declaration(bound)
        if parsed is None or parsed[0] != kind or kind == "not-a-bound":
            return False
        source = parsed[2]
        if source is None:
            return bool(parsed[1].strip())
        if source in seen or re.search(rf"\b{re.escape(source)}\b", bound.initializer) is None:
            return False
        owner = visible_owner(bound, source)
        return owner is not None and resolves_to_direct(owner, kind, seen | {source})

    for bound in enforced:
        parsed = declaration(bound)
        location = f"{bound.path}:{bound.line}: {bound.name}"
        if parsed is None:
            failures.append(f"{location} has no numeric-bound declaration")
            continue
        kind, rationale, source = parsed
        if source is None and not rationale.strip():
            failures.append(f"{location} has an empty rationale")
        elif source is not None and not resolves_to_direct(bound, kind, {bound.name}):
            failures.append(
                f"{location} has an invalid derived declaration from {source}"
            )
    return failures


def main(argv: list[str] | None = None) -> int:
    parser = argparse.ArgumentParser()
    parser.add_argument("--root", type=Path, default=Path("."))
    arguments = parser.parse_args(argv)
    bounds = inventory(arguments.root.resolve())
    failures = validate(bounds)
    if failures:
        print("numeric-bound check FAILED:")
        for failure in failures:
            print(f"  - {failure}")
        return 1
    enforced = sum(is_enforced(bound) for bound in bounds)
    test_only = sum(bound.test_only for bound in bounds)
    outside = len(bounds) - enforced - test_only
    print(
        "numeric-bound check passed: "
        f"{enforced} enforced, {outside} outside blocking scope, "
        f"{test_only} test-only"
    )
    return 0


if __name__ == "__main__":
    sys.exit(main())
