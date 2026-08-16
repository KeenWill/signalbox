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
The whole attribute run is read, so an intervening attribute and a compound
``cfg(all(test, not(windows)))`` both still count; ``cfg(any(test, ...))`` and
``cfg(not(test))`` do not, because those modules also compile without ``test``
and their constants really are production.

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

only when its initializer references the named bound by its bare name and that
name resolves to a direct declaration of the same kind. This keeps self-evident
byte/unit translations from repeating rationale while preventing an unexplained
independent cap from hiding behind the escape. A path-qualified reference such
as ``other_crate::MAX_BASE`` does not count: it names an item this scan cannot
see, whose classification may differ from the same-named local one, so the
escape is unavailable and the constant declares its own kind.

The source name resolves in the Rust scope that declares it: the innermost
brace-delimited block containing the derived constant — a module, a function
body, an ``impl``, any block at all — then outward to the file. A declaration in
a sibling module or another function is never in scope, so a derivation whose
initializer really reads an imported constant cannot be validated against an
unrelated same-named constant elsewhere in the file, and a nearer declaration
shadows a farther one as Rust resolves it. What this lexical scan cannot follow,
it refuses: a name reachable only through a ``use`` from outside the file leaves
the escape unproven and the declaration is rejected.

Every other bound the initializer names must carry the inherited kind too. A
value assembled from a ceiling and a tunable inherits no single rationale, so
the escape is unavailable to it and it declares its own kind.

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
REFERENCED_BOUND = re.compile(r"(?<![\w:])(?P<name>[A-Z][A-Z0-9_]*)\b")
ATTRIBUTED_MODULE = re.compile(
    r"(?P<attributes>(?:#\s*\[[^\]]*\]\s*)+)"
    r"(?:pub(?:\([^)]*\))?\s+)?mod\s+(?P<name>[A-Za-z_][A-Za-z0-9_]*)\s*(?P<form>[{;])",
    re.MULTILINE,
)
CFG_ATTRIBUTE = re.compile(r"#\s*\[\s*cfg\s*\((?P<predicate>[^\]]*)\)\s*\]")
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


def split_configurations(predicate: str) -> list[str]:
    """Split one `cfg` predicate list on its top-level commas."""
    members = []
    depth = 0
    start = 0
    for position, character in enumerate(predicate):
        if character == "(":
            depth += 1
        elif character == ")":
            depth -= 1
        elif character == "," and depth == 0:
            members.append(predicate[start:position])
            start = position + 1
    members.append(predicate[start:])
    return members


def configuration_requires_test(predicate: str) -> bool:
    """Report whether a `cfg` predicate holds only when `test` is set.

    Bare `test` does, and so does an `all(...)` with a member that does, which
    covers `all(test, not(windows))`. `any(...)` does not, because the module
    also builds when another member holds, and `not(...)` never does. The
    predicate is read structurally rather than evaluated, so an unrecognised
    form counts as buildable without `test` and leaves its constants gating —
    the safe direction for a blocking check.
    """
    predicate = predicate.strip()
    if predicate == "test":
        return True
    if predicate.startswith("all(") and predicate.endswith(")"):
        return any(
            configuration_requires_test(member)
            for member in split_configurations(predicate[len("all(") : -1])
        )
    return False


def requires_test(attributes: str) -> bool:
    """Report whether an attribute run compiles only under `cfg(test)`."""
    return any(
        configuration_requires_test(match.group("predicate"))
        for match in CFG_ATTRIBUTE.finditer(attributes)
    )


def test_ranges(code: str) -> list[tuple[int, int]]:
    ranges = []
    for match in ATTRIBUTED_MODULE.finditer(code):
        if match.group("form") != "{" or not requires_test(match.group("attributes")):
            continue
        ranges.append((match.start(), matching_brace(code, match.end() - 1)))
    return ranges


def block_ranges(code: str) -> list[tuple[int, int]]:
    """Report the span of every brace-delimited block in one file.

    A Rust `const` is visible in the block that declares it and in the blocks
    nested inside it, so every brace pair is a scope boundary — a module, a
    function body, an `impl`, a bare block. Unbalanced braces cannot occur in
    code that compiles, and a stray close is dropped rather than trusted.
    """
    ranges = []
    opened: list[int] = []
    for position, character in enumerate(code):
        if character == "{":
            opened.append(position)
        elif character == "}" and opened:
            ranges.append((opened.pop(), position))
    return ranges


def innermost_scope(offset: int, ranges: list[tuple[int, int]]) -> tuple[int, int] | None:
    """Report the narrowest block containing ``offset``, or the file."""
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
    for match in ATTRIBUTED_MODULE.finditer(code):
        if match.group("form") != ";" or not requires_test(match.group("attributes")):
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
        # Scanning brace pairs costs a pass over the file, so it is paid only
        # where a boundary-named constant actually needs a scope.
        matches = [
            match
            for match in CONSTANT.finditer(code)
            if is_boundary_name(match.group("name"))
        ]
        if not matches:
            continue
        ranges = test_ranges(code)
        blocks = block_ranges(code)
        external = in_test_sources(path, test_sources)
        lines = text.splitlines()
        relative = path.relative_to(root)
        for match in matches:
            name = match.group("name")
            line = text.count("\n", 0, match.start()) + 1
            end = initializer_end(code, match.end())
            annotation = lines[line - 2] if line > 1 else ""
            bounds.append(
                Bound(
                    path=relative,
                    name=name,
                    line=line,
                    offset=match.start(),
                    scope=innermost_scope(match.start(), blocks),
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

        A file-level declaration is visible everywhere in its file, and one
        inside a block only within that block, so a sibling module or another
        function never supplies an owner. The nearest enclosing declaration
        shadows the rest; two at the same depth would not compile, and resolve
        to neither here.
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
        bare = rf"(?<![\w:]){re.escape(source)}\b"
        if source in seen or re.search(bare, bound.initializer) is None:
            return False
        owner = visible_owner(bound, source)
        if owner is None or not resolves_to_direct(owner, kind, seen | {source}):
            return False
        return all(
            resolves_to_direct(contributor, kind, seen | {source, name})
            for name, contributor in contributors(bound, source)
        )

    def contributors(bound: Bound, source: str) -> list[tuple[str, Bound]]:
        """Report the other in-scope bounds the initializer reads.

        A derivation inherits one rationale, so every bound feeding the value
        has to carry the kind being inherited. Names that resolve to no
        enforced bound contribute nothing to inherit and are left alone.
        """
        found = []
        for match in REFERENCED_BOUND.finditer(bound.initializer):
            name = match.group("name")
            if name in {source, bound.name}:
                continue
            contributor = visible_owner(bound, name)
            if contributor is not None:
                found.append((name, contributor))
        return found

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
