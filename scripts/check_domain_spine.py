#!/usr/bin/env python3
"""Check that docs/domain-spine.md stays in sync with the public API.

Ground truth is each crate's lib.rs: `pub use` re-exports, the domain crate's
`define_identity!` invocations, and any directly declared crate-root public
item. The spine is parsed per `## crate: module` section, taking column-0
`pub struct/enum/trait/fn` lines as its declarations. The check fails when

1. an exported name has no declaration in its owning module's section
   (a mention elsewhere in the document does not count),
2. a section declares a name its module no longer exports (stale declaration)
   or declares it twice, and duplicate Inventory rows are rejected,
3. a lib.rs exposes public API in any form this script does not parse —
   direct declarations, `pub mod`, glob/rename/path re-exports, or an
   identity invocation outside the supported doc-comment shape all fail
   loudly rather than silently thinning the ground truth, or
4. a per-module count in the Inventory table disagrees with the export
   surface, an aggregate total row disagrees with the per-module sum or
   states a different free-function split from those rows, an exporting
   module has no Inventory row, or a section declares the same name twice, or

5. a listed type's public inherent method surface disagrees with its
   section. Source truth is every item-position `impl` block in the
   crate's source tree — at column 0 or inside inline modules — except
   those whose cfg predicate removes them from library builds (trait
   impls contribute nothing: Rust rejects `pub` on their items); spine
   truth is the owning section's impl-block `pub fn` lines plus its
   `// accessors:` comment lists. A public method with no declaration
   fails, as does a declared method the source no longer defines. Types
   minted by a column-0 invocation of `define_identity!`, `goal_text!`,
   `bounded_text!`, or `positive_position!` are validated in both
   directions against the macro's expansion contract — the literal
   `pub fn` names in its macro_rules body — with the identity and
   position macros' shared shape declared once in the spine's
   `impl <Identity>`/`impl <Position>` placeholder blocks; a minting
   macro whose body spells no literal method names would fall back to a
   stale-direction exemption for its types. An impl header the scan
   cannot parse fails loudly.

Known limitation of this mechanical check: signatures, associated consts
and types, trait items, and enum variant lists inside a declaration are
not validated — method names on listed types are, but keeping the rest
faithful is a review responsibility. The scan is textual, and its item
model has a deliberate boundary: impls reached only through initializer
expressions other than a braced anonymous-const body, minting-macro
invocations off item positions, and `#[path]`-loaded files beneath a
cfg-gated module directory are outside it. None of those shapes occur in
these crates; a change that introduces one must restate itself or extend
the check, and cargo public-api is the upgrade path if these textual
tripwires prove insufficient.

The spine may say more than declarations (sealed markers, accessor notes); it
may not disagree with the export surface. Run from the repository root; exits
nonzero with a per-item report on any mismatch.
"""

from __future__ import annotations

import re
import sys
from pathlib import Path

SPINE = Path("docs/domain-spine.md")
CRATES = {
    "domain": Path("crates/domain/src/lib.rs"),
    "application": Path("crates/application/src/lib.rs"),
}
IDENTITY_SECTION = "lib.rs — identities"

MODIFIERS = r"(?:(?:async|unsafe|const)\s+|extern\s+\"[^\"]*\"\s+)*"
DECLARATION = re.compile(
    rf"^pub {MODIFIERS}(?:struct|enum|trait|fn|static|type|const) ([A-Za-z_][A-Za-z0-9_]*)"
)
ROOT_DECLARATION = re.compile(
    rf"^pub {MODIFIERS}(?:struct|enum|union|trait|fn|static|type|const) ([A-Za-z_][A-Za-z0-9_]*)",
    re.MULTILINE,
)
IMPL_HEADER = re.compile(r"^impl\b|^impl<")
IMPL_METHOD = re.compile(
    rf"^\s*pub\s+{MODIFIERS}fn\s+(?:r#)?([A-Za-z_][A-Za-z0-9_]*)"
)
ACCESSOR_COMMENT = re.compile(r"^\s*// accessors?:")
ACCESSOR_NAME = re.compile(r"([A-Za-z_][A-Za-z0-9_]*)\(\)")
SECTION_TYPE = re.compile(r"^pub (?:struct|enum) ([A-Za-z_][A-Za-z0-9_]*)")
MACRO_INVOCATION = re.compile(
    r"^([A-Za-z_][A-Za-z0-9_:]*)!\s*\(([^;]*?)\)\s*;", re.MULTILINE | re.DOTALL
)
# The only macros trusted to mint method surface on the types they name,
# mapped to the spine placeholder impl block that documents the shared
# shape (None when each minted type carries its own impl-block
# declarations). Generated method names are extracted from each macro's
# macro_rules body, so both comparison directions cover macro-backed types;
# a new method-generating macro must be added here before its types can
# pass.
METHOD_MINTING_MACROS: dict[str, str | None] = {
    "define_identity": "<Identity>",
    "goal_text": None,
    "bounded_text": None,
    "positive_position": "<Position>",
}
MACRO_RULES_HEADER = re.compile(r"^macro_rules!\s+([A-Za-z_][A-Za-z0-9_]*)\s*\{")
PLACEHOLDER_IMPL = re.compile(r"^impl (<[A-Za-z]+>) \{")
CAPITALIZED_NAME = re.compile(r"\b([A-Z][A-Za-z0-9_]*)\b")
ANGLE_TOKEN = re.compile(r"->|<|>|\bfor\b")
CFG_ATTRIBUTE = re.compile(r"^#\[cfg\((.*)\)\]$")
MOD_HEADER = re.compile(
    r"^\s*(?:pub(?:\([^)]*\))?\s+)?mod\s+[A-Za-z_][A-Za-z0-9_]*\s*\{"
)
# An anonymous const's initializer block is a transparent item container:
# an inherent impl inside `const _: () = { ... };` attaches its methods to
# the type crate-wide, exactly as at column 0.
TRANSPARENT_BLOCK = re.compile(
    r"^\s*(?:pub(?:\([^)]*\))?\s+)?const\s+_\s*:.*=\s*\{\s*$"
)
OUT_OF_LINE_MOD = re.compile(
    r"^(?:pub(?:\([^)]*\))?\s+)?mod\s+([A-Za-z_][A-Za-z0-9_]*)\s*;"
)
IMPL_KEYWORD = re.compile(r"^\s*impl\b")


def cfg_removes_from_library(predicate: str) -> bool:
    """True when the predicate cannot hold in any non-test build.

    That is exactly `test` itself or an `all(...)` requiring it, recursively.
    `any(test, feature = ...)` holds when the feature is enabled, and
    `not(test)` holds in every library build, so neither removes the item —
    their methods stay required in the spine like any cfg-conditional
    public API. `#[cfg_attr(...)]` never gates compilation and never
    reaches this predicate parser.
    """
    predicate = predicate.strip()
    if predicate == "test":
        return True
    conjunction = re.fullmatch(r"all\s*\((.*)\)", predicate, re.DOTALL)
    if conjunction is None:
        return False
    parts: list[str] = []
    nesting = 0
    part_start = 0
    arguments = conjunction.group(1)
    for position, char in enumerate(arguments):
        if char == "(":
            nesting += 1
        elif char == ")":
            nesting -= 1
        elif char == "," and nesting == 0:
            parts.append(arguments[part_start:position])
            part_start = position + 1
    parts.append(arguments[part_start:])
    return any(cfg_removes_from_library(part) for part in parts)


def parse_exports(lib_rs: Path) -> dict[str, set[str]]:
    """Map module name -> set of names re-exported from it at crate root."""
    text = lib_rs.read_text()
    exports: dict[str, set[str]] = {}
    for module, group in re.findall(
        r"^pub use (\w+)::\{([^}]*)\};", text, re.MULTILINE | re.DOTALL
    ):
        names = {n.strip() for n in group.split(",") if n.strip()}
        exports.setdefault(module, set()).update(names)
    for module, name in re.findall(r"^pub use (\w+)::(\w+);", text, re.MULTILINE):
        exports.setdefault(module, set()).add(name)
    return exports


def parse_identities(lib_rs: Path) -> set[str]:
    """Names declared through define_identity! invocations."""
    return set(
        re.findall(
            r"define_identity!\(\s*(?:///[^\n]*\n\s*)*([A-Za-z_][A-Za-z0-9_]*)\s*\)",
            lib_rs.read_text(),
        )
    )


def parse_root_declarations(lib_rs: Path) -> set[str]:
    """Public items declared directly at column 0 of lib.rs."""
    return set(ROOT_DECLARATION.findall(lib_rs.read_text()))


def blank_comments_and_strings(text: str) -> str:
    """Blank comment and string-literal contents, preserving offsets.

    An explicit scanner rather than a regex, for the same reason as the one
    in source scans: raw string literals and the apostrophe's
    double duty as lifetime and character literal are not a regular language,
    and getting either wrong moves impl blocks into or out of the scan.
    Rustdoc examples are comments, so the `impl` blocks they quote vanish
    here instead of counting as public surface.
    """
    code = list(text)
    index = 0
    length = len(text)
    while index < length:
        char = text[index]
        if char == "/" and text[index + 1 : index + 2] == "/":
            end = text.find("\n", index)
            end = length if end == -1 else end
            for position in range(index, end):
                code[position] = " "
            index = end
            continue
        if char == "/" and text[index + 1 : index + 2] == "*":
            # Rust block comments nest; scan for the delimiter that closes
            # the outermost one rather than the first `*/`.
            depth = 1
            cursor = index + 2
            while cursor < length and depth > 0:
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
        if char == "r" and text[index + 1 : index + 2] in ('"', "#"):
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
            if text[index + 1 : index + 2] == "\\":
                end = text.find("'", index + 2)
                end = length if end == -1 else end + 1
                for position in range(index + 1, min(end - 1, length)):
                    code[position] = " "
                index = end
                continue
            if text[index + 2 : index + 3] == "'":
                code[index + 1] = " "
                index += 3
                continue
        index += 1
    return "".join(code)


def impl_self_type(header: str) -> str | None:
    """Self-type name of one impl header, None when there is none to take.

    Handles generic parameter lists after `impl`, `Trait for Type` at angle
    depth zero, trailing where-clauses, and path-qualified self types. The
    spine's `impl <Identity>`-style placeholders have no self type and map
    to None; a source header that maps to None is reported by the caller.
    """
    flat = " ".join(header.split())
    rest = flat[len("impl") :].lstrip()
    if rest.startswith("<"):
        depth = 0
        for token in ANGLE_TOKEN.finditer(rest):
            if token.group() == "<":
                depth += 1
            elif token.group() == ">":
                depth -= 1
                if depth == 0:
                    rest = rest[token.end() :].lstrip()
                    break
        else:
            return None
    rest = re.split(r"\bwhere\b", rest)[0].split("{")[0].strip()
    if not rest:
        return None
    depth = 0
    for token in ANGLE_TOKEN.finditer(rest):
        if token.group() == "<":
            depth += 1
        elif token.group() == ">":
            depth -= 1
        elif token.group() == "for" and depth == 0:
            rest = rest[token.end() :].lstrip()
            break
    segment = re.match(r"(?:[A-Za-z_][A-Za-z0-9_]*::)*([A-Za-z_][A-Za-z0-9_]*)", rest)
    return segment.group(1) if segment else None


def body_brace_offset(header: str) -> int | None:
    """Offset of the impl body's opening brace, None while none is present.

    A brace inside the header's angle brackets — a const-generic argument
    such as `Foo<{ 1 + 1 }>` — or inside square brackets — a where-bound
    array type such as `[(); { const N: usize = 2; N }]` — is part of the
    header, not the body, so the opener is the first brace at angle and
    bracket depth zero (`->` skipped). Depth tracking suspends inside
    those const braces, where `<` is a comparison operator
    (`Foo<{ 1 < 2 }>`), not a delimiter.
    """
    angle_depth = 0
    bracket_depth = 0
    const_brace_depth = 0
    index = 0
    length = len(header)
    while index < length:
        char = header[index]
        if const_brace_depth:
            if char == "{":
                const_brace_depth += 1
            elif char == "}":
                const_brace_depth -= 1
            index += 1
            continue
        if header[index : index + 2] == "->":
            index += 2
            continue
        if char == "<":
            angle_depth += 1
        elif char == ">":
            angle_depth -= 1
        elif char == "[":
            bracket_depth += 1
        elif char == "]":
            bracket_depth -= 1
        elif char == "{":
            if angle_depth == 0 and bracket_depth == 0:
                return index
            const_brace_depth = 1
        index += 1
    return None


def parse_source_methods(
    crate: str, path: Path
) -> tuple[dict[str, set[str]], list[str]]:
    """Map self-type name -> public method names from item-position impls.

    An impl counts wherever items appear: at column 0, and inside inline
    modules — a production `mod nested { impl ... }` is public surface too.
    Exclusion is by configuration, not indentation: an impl or module whose
    cfg predicate removes it from library builds contributes nothing, which
    is what keeps `#[cfg(test)] mod tests` fixtures out. Only `pub fn`
    items at impl depth are public method surface; trait impls attribute to
    their self type but can never contribute (`pub` on a trait-impl item is
    rejected by the compiler). Attribution is by unqualified self-type
    name: an inherent impl may legally live anywhere in its crate, so a
    private type sharing a listed type's name would misattribute — loudly,
    as a spurious missing declaration — and would need this parser extended
    to path-aware attribution.
    """
    lines = blank_comments_and_strings(path.read_text()).splitlines()
    methods: dict[str, set[str]] = {}
    failures: list[str] = []
    depth = 0
    module_entry_depths: list[int] = []
    module_test_gated: list[bool] = []
    tracker = TestConfigurationTracker()
    index = 0
    while index < len(lines):
        line = lines[index]
        at_item_position = depth == len(module_entry_depths)
        if at_item_position and IMPL_KEYWORD.match(line):
            header_start = index
            header = line
            while body_brace_offset(header) is None and index + 1 < len(lines):
                index += 1
                header = f"{header} {lines[index]}"
            opener = body_brace_offset(header)
            excluded = tracker.take() or any(module_test_gated)
            if opener is None:
                index += 1
                continue
            target = impl_self_type(header[:opener])
            if target is None:
                failures.append(
                    f"{crate} {path.as_posix()}:{header_start + 1} has an impl"
                    " header this check cannot parse — restate or extend the"
                    " check"
                )
                index += 1
                continue
            remainder = header[opener:]
            block_depth = remainder.count("{") - remainder.count("}")
            body_tracker = TestConfigurationTracker()
            while block_depth > 0 and index + 1 < len(lines):
                index += 1
                body_line = lines[index]
                if block_depth == 1 and not body_tracker.is_attribute(
                    body_line
                ):
                    method = IMPL_METHOD.match(body_line)
                    if method and not excluded and not body_tracker.pending:
                        methods.setdefault(target, set()).add(method.group(1))
                    if body_line.strip():
                        body_tracker.pending = False
                block_depth += body_line.count("{") - body_line.count("}")
            index += 1
            continue
        if at_item_position and (
            MOD_HEADER.match(line) or TRANSPARENT_BLOCK.match(line)
        ):
            gated = tracker.take() or any(module_test_gated)
            entry_depth = depth
            depth += line.count("{") - line.count("}")
            if depth > entry_depth:
                module_entry_depths.append(entry_depth)
                module_test_gated.append(gated)
            index += 1
            continue
        tracker.observe(line)
        depth += line.count("{") - line.count("}")
        while module_entry_depths and depth <= module_entry_depths[-1]:
            module_entry_depths.pop()
            module_test_gated.pop()
        index += 1
    return methods, failures


class TestConfigurationTracker:
    """Whether the item being approached is removed from library builds.

    Absorbs attribute lines — including rustfmt's multi-line cfg
    predicates, accumulated across continuation lines by bracket balance —
    and clears the pending state on other substantive lines. Item
    consumers read and reset `pending` through `take()`.
    """

    def __init__(self) -> None:
        self.pending = False
        self._continuation = 0
        self._parts: list[str] = []

    def take(self) -> bool:
        pending = self.pending
        self.pending = False
        return pending

    def is_attribute(self, line: str) -> bool:
        """Absorb attribute lines; True when the line was one."""
        stripped = line.strip()
        if self._continuation > 0:
            self._parts.append(stripped)
            self._continuation += line.count("[") - line.count("]")
            if self._continuation <= 0:
                joined = " ".join(self._parts)
                self._parts = []
                cfg = CFG_ATTRIBUTE.match(joined)
                if cfg and cfg_removes_from_library(cfg.group(1)):
                    self.pending = True
            return True
        if not stripped.startswith("#["):
            return False
        cfg = CFG_ATTRIBUTE.match(stripped)
        if cfg:
            if cfg_removes_from_library(cfg.group(1)):
                self.pending = True
            return True
        balance = line.count("[") - line.count("]")
        if balance > 0:
            self._continuation = balance
            self._parts = [stripped]
        return True

    def observe(self, line: str) -> None:
        """Absorb one line outside item context, clearing on substance."""
        if not self.is_attribute(line) and line.strip():
            self.pending = False


def collect_test_gated_module_files(source_root: Path) -> set[Path]:
    """Paths loaded by out-of-line `mod` declarations under a test cfg.

    A `#[cfg(test)] mod helpers;` loads a separate file whose contents are
    absent from library builds; the per-file scan cannot see the declaring
    module's gate, so those files (and their subdirectories) are excluded
    up front — unless another declaration of the same module is reachable
    in library builds (`#[cfg(not(test))] mod helpers;` alongside the
    gated one), in which case the file stays scanned. A `#[path = ...]`
    override is invisible here — its string is blanked — which can only
    fail loud: a file is never excluded on the strength of an attribute
    this scan cannot read.
    """
    gated: set[Path] = set()
    production: set[Path] = set()
    for path in sorted(source_root.rglob("*.rs")):
        tracker = TestConfigurationTracker()
        for line in blank_comments_and_strings(path.read_text()).splitlines():
            declared = OUT_OF_LINE_MOD.match(line)
            if declared:
                if path.name in ("lib.rs", "mod.rs", "main.rs"):
                    base = path.parent
                else:
                    base = path.parent / path.stem
                targets = (base / f"{declared.group(1)}.rs", base / declared.group(1))
                if tracker.take():
                    gated.update(targets)
                else:
                    production.update(targets)
                continue
            tracker.observe(line)
    return gated - production


def parse_macro_minted_types(text: str) -> dict[str, str]:
    """Map minted type name -> macro, from column-0 minting invocations.

    Only the macros in METHOD_MINTING_MACROS qualify — an assertion or
    derive helper naming a listed type mints nothing. Each invocation
    mints exactly one type: the first capitalized name in its arguments.
    Comments and string contents are blanked before matching, so a
    commented-out invocation grants nothing and doc comments inside the
    invocation contribute no names. Only parenthesized invocations are
    read; a brace or bracket form that mints public surface would need
    this parser extended.
    """
    minted: dict[str, str] = {}
    for invocation, arguments in MACRO_INVOCATION.findall(
        blank_comments_and_strings(text)
    ):
        macro = invocation.split("::")[-1]
        if macro not in METHOD_MINTING_MACROS:
            continue
        first = CAPITALIZED_NAME.search(arguments)
        if first:
            minted[first.group(1)] = macro
    return minted


def parse_macro_generated_methods(text: str) -> dict[str, set[str]]:
    """Public method names each column-0 macro_rules body generates.

    Textual like the rest of the scan: the literal `pub fn` lines in a
    macro body are its expansion contract, letting minted types face both
    comparison directions. A minting macro that derived method names from
    metavariables would extract nothing here, and its types fall back to
    the stale-direction exemption rather than failing on phantoms.
    """
    generated: dict[str, set[str]] = {}
    lines = blank_comments_and_strings(text).splitlines()
    index = 0
    while index < len(lines):
        header = MACRO_RULES_HEADER.match(lines[index])
        if header:
            names = generated.setdefault(header.group(1), set())
            block_depth = lines[index].count("{") - lines[index].count("}")
            while block_depth > 0 and index + 1 < len(lines):
                index += 1
                body_line = lines[index]
                method = IMPL_METHOD.match(body_line)
                if method:
                    names.add(method.group(1))
                block_depth += body_line.count("{") - body_line.count("}")
        index += 1
    return generated


def parse_spine_method_declarations(
    spine_text: str,
) -> tuple[
    dict[tuple[str, str], dict[str, set[str]]],
    list[str],
    dict[str, set[str]],
]:
    """Map (crate, section label) -> type name -> declared method names.

    A method is declared by a `pub fn` line inside a section's column-0
    impl block, or by an `// accessors:` comment list — inside an impl
    block, or at column 0 attached to the nearest preceding `pub
    struct`/`pub enum` line. An accessor list continues onto following
    comment lines for as long as each line ends with a comma. Declaring
    the same method twice for one type is reported, matching how the
    name-level parser rejects duplicate type declarations. Placeholder
    impl blocks (`impl <Identity>`, `impl <Position>`) declare the shared
    surface of macro-minted types and are returned separately.
    """
    sections: dict[tuple[str, str], dict[str, set[str]]] = {}
    duplicates: list[str] = []
    placeholders: dict[str, set[str]] = {}

    def declare(
        section: tuple[str, str], target: str, names: list[str]
    ) -> None:
        declared = sections[section].setdefault(target, set())
        for name in names:
            if name in declared:
                duplicates.append(
                    f"'{section[0]}: {section[1]}' section declares"
                    f" {target}::{name}() more than once"
                )
            declared.add(name)

    current: tuple[str, str] | None = None
    current_type: str | None = None
    in_impl = False
    impl_type: str | None = None
    placeholder_target: str | None = None
    header_accum: list[str] | None = None
    accessor_continues = False
    for line in spine_text.splitlines():
        if line.startswith("## "):
            header = re.match(r"^## (domain|application): (.+)$", line)
            current = (header.group(1), header.group(2).strip()) if header else None
            if current:
                sections.setdefault(current, {})
            current_type = None
            in_impl = False
            impl_type = None
            placeholder_target = None
            header_accum = None
            accessor_continues = False
            continue
        if current is None:
            continue
        if header_accum is not None:
            header_accum.append(line)
            if "{" in line:
                impl_type = impl_self_type(" ".join(header_accum))
                in_impl = "}" not in line.split("{", 1)[1]
                header_accum = None
            continue
        if not in_impl and IMPL_HEADER.match(line):
            accessor_continues = False
            placeholder = PLACEHOLDER_IMPL.match(line)
            if placeholder:
                placeholder_target = placeholder.group(1)
                placeholders.setdefault(placeholder_target, set())
                impl_type = None
                in_impl = True
                continue
            placeholder_target = None
            if "{" in line:
                impl_type = impl_self_type(line)
                in_impl = "}" not in line.split("{", 1)[1]
            else:
                header_accum = [line]
            continue
        if in_impl:
            if ACCESSOR_COMMENT.match(line) or (
                accessor_continues and line.lstrip().startswith("//")
            ):
                if impl_type:
                    declare(current, impl_type, ACCESSOR_NAME.findall(line))
                elif placeholder_target:
                    placeholders[placeholder_target].update(
                        ACCESSOR_NAME.findall(line)
                    )
                accessor_continues = line.rstrip().endswith(",")
                continue
            accessor_continues = False
            method = IMPL_METHOD.match(line)
            if method and impl_type:
                declare(current, impl_type, [method.group(1)])
            elif method and placeholder_target:
                placeholders[placeholder_target].add(method.group(1))
            if line.startswith("}"):
                in_impl = False
                impl_type = None
                placeholder_target = None
            continue
        if ACCESSOR_COMMENT.match(line) or (
            accessor_continues and line.startswith("//")
        ):
            if current_type:
                declare(current, current_type, ACCESSOR_NAME.findall(line))
            accessor_continues = line.rstrip().endswith(",")
            continue
        accessor_continues = False
        declared_type = SECTION_TYPE.match(line)
        if declared_type:
            current_type = declared_type.group(1)
    return sections, duplicates, placeholders


def validate_lib_forms(crate: str, lib_rs: Path) -> list[str]:
    """Closed-world guard: any public form this script cannot parse fails.

    The check's ground truth is only trustworthy if every way of exposing
    public API through lib.rs is either parsed or rejected here.
    """
    text = lib_rs.read_text()
    failures = [
        f"{crate} lib.rs declares `pub mod {name};`; the check supports only"
        " private modules with pub use re-exports — restate or extend the check"
        for name in re.findall(r"^pub mod (\w+)", text, re.MULTILINE)
    ]
    for statement in re.findall(r"^pub use [^;]+;", text, re.MULTILINE):
        flat = " ".join(statement.split())
        group = re.fullmatch(r"pub use (\w+)::\{(.*)\};", flat)
        if group:
            for name in group.group(2).split(","):
                if name.strip() and not re.fullmatch(r"[A-Za-z_][A-Za-z0-9_]*", name.strip()):
                    failures.append(
                        f"{crate} lib.rs re-export `{name.strip()}` is not a bare"
                        " name (glob/rename/path forms are unsupported) —"
                        " restate or extend the check"
                    )
        elif not re.fullmatch(r"pub use \w+::\w+;", flat):
            failures.append(
                f"{crate} lib.rs has an unsupported re-export form: `{flat}`"
                " — restate or extend the check"
            )
    for name in re.findall(r"^pub extern crate (\w+)", text, re.MULTILINE):
        failures.append(
            f"{crate} lib.rs re-exports crate `{name}` via pub extern crate;"
            " this form is unsupported — restate or extend the check"
        )
    for line in text.splitlines():
        macro = re.match(r"([A-Za-z_][A-Za-z0-9_:]*)!\s*[\(\[{]", line)
        if macro and macro.group(1) not in ("define_identity", "macro_rules"):
            failures.append(
                f"{crate} lib.rs invokes item macro `{macro.group(1)}!` at the"
                " crate root; its expansion is invisible to this check —"
                " restate or extend the check"
            )
    if crate == "domain":
        invocations = text.count("define_identity!(")
        parsed = len(
            re.findall(
                r"define_identity!\(\s*(?:///[^\n]*\n\s*)*[A-Za-z_][A-Za-z0-9_]*\s*\)",
                text,
            )
        )
        if invocations != parsed:
            failures.append(
                f"domain lib.rs has {invocations} define_identity! invocations"
                f" but only {parsed} parse (only /// doc lines before the name"
                " are supported) — restate or extend the check"
            )
    return failures


def parse_spine_sections(
    spine_text: str,
) -> tuple[dict[tuple[str, str], set[str]], list[str]]:
    """Map (crate, section label) -> declared names; also report duplicates."""
    sections: dict[tuple[str, str], set[str]] = {}
    duplicates: list[str] = []
    current: tuple[str, str] | None = None
    for line in spine_text.splitlines():
        if line.startswith("## "):
            header = re.match(r"^## (domain|application): (.+)$", line)
            if header:
                current = (header.group(1), header.group(2).strip())
                sections.setdefault(current, set())
            else:
                current = None
            continue
        if current:
            declared = DECLARATION.match(line)
            if declared:
                name = declared.group(1)
                if name in sections[current] and name != "<Identity>":
                    duplicates.append(
                        f"'{current[0]}: {current[1]}' declares {name} more than once"
                    )
                sections[current].add(name)
    return sections, duplicates


def parse_inventory(spine_text: str) -> dict[tuple[str, str], int]:
    """Map (crate, module label) -> expected export count from the table.

    A cell like `5 (+1 free fn)` expects 5 types plus 1 function = 6 exports;
    `8 (incl. 2 traits)` expects 8 (traits are already types).

    The free-function component is returned separately as well as summed. An
    aggregate row that credits types to the free-function column, or the
    reverse, leaves the sum unchanged, so a check that compares only totals
    cannot see that error — `761 (+14 free fn)` and `763 (+12 free fn)` both
    total 775, and the wrong one stood for two commits.
    """
    expected: dict[tuple[str, str], int] = {}
    free_functions: dict[tuple[str, str], int] = {}
    duplicate_rows: list[str] = []
    for crate, label, count, extra in re.findall(
        r"^\| (domain|application): ([^|]+?) \| (\d+)(?: \(\+(\d+) free fn\))?[^|]*\|",
        spine_text,
        re.MULTILINE,
    ):
        key = (crate, label.strip())
        if key in expected:
            duplicate_rows.append(
                f"Inventory table has more than one row for '{key[0]}: {key[1]}'"
            )
        expected[key] = int(count) + int(extra or 0)
        free_functions[key] = int(extra or 0)
    return expected, free_functions, duplicate_rows


def main() -> int:
    spine_text = SPINE.read_text()
    failures: list[str] = []

    for crate, path in CRATES.items():
        failures.extend(validate_lib_forms(crate, path))

    identities = parse_identities(CRATES["domain"])
    all_exports = {crate: parse_exports(path) for crate, path in CRATES.items()}
    sections, duplicates = parse_spine_sections(spine_text)
    failures.extend(duplicates)

    # Root-declared items must be the identity macros; anything else needs
    # this mapping extended before it can pass.
    for crate, path in CRATES.items():
        allowed = identities if crate == "domain" else set()
        for name in sorted(parse_root_declarations(path) - allowed):
            failures.append(
                f"{crate} lib.rs declares public item {name} directly; add it to"
                " the spine and extend scripts/check_domain_spine.py to cover it"
            )

    # Declaration-level comparison per module section, both directions.
    identity_declared = sections.get(("domain", IDENTITY_SECTION), set())
    for name in sorted(identities - identity_declared):
        failures.append(f"identity {name} has no declaration in the identities section")
    for name in sorted(identity_declared - identities):
        failures.append(
            f"identities section declares {name}, which lib.rs does not define"
        )

    for crate, exports in all_exports.items():
        for module, names in exports.items():
            declared = sections.get((crate, module))
            if declared is None:
                failures.append(f"{crate}: {module} has exports but no spine section")
                continue
            for name in sorted(names - declared):
                failures.append(
                    f"{crate}::{module}::{name} is exported but not declared in"
                    f" the '{crate}: {module}' section"
                )
            for name in sorted(declared - names):
                failures.append(
                    f"'{crate}: {module}' section declares {name}, which the"
                    " module no longer exports"
                )
    for crate, label in sections:
        if label == IDENTITY_SECTION:
            continue
        if label not in all_exports[crate] and sections[(crate, label)]:
            failures.append(
                f"spine section '{crate}: {label}' matches no exporting module"
            )

    # Method-surface comparison per listed type, both directions.
    spine_methods, method_duplicates, placeholder_methods = (
        parse_spine_method_declarations(spine_text)
    )
    failures.extend(method_duplicates)
    for crate, lib_path in CRATES.items():
        home_section: dict[str, str] = {}
        for module, names in all_exports[crate].items():
            for name in names:
                if name in home_section:
                    failures.append(
                        f"{crate} exports {name} from both"
                        f" {home_section[name]} and {module}; method"
                        " attribution needs one home — extend the check"
                    )
                home_section[name] = module
        if crate == "domain":
            for name in identities:
                home_section.setdefault(name, IDENTITY_SECTION)
        source_methods: dict[str, set[str]] = {}
        minted_types: dict[str, str] = {}
        generated_methods: dict[str, set[str]] = {}
        test_gated_files = collect_test_gated_module_files(lib_path.parent)
        for path in sorted(lib_path.parent.rglob("*.rs")):
            if any(
                path == gated or gated in path.parents
                for gated in test_gated_files
            ):
                continue
            found, unparsed = parse_source_methods(crate, path)
            for name, methods in found.items():
                source_methods.setdefault(name, set()).update(methods)
            failures.extend(unparsed)
            text = path.read_text()
            minted_types.update(parse_macro_minted_types(text))
            for macro, names in parse_macro_generated_methods(text).items():
                generated_methods.setdefault(macro, set()).update(names)
        for name, section in sorted(home_section.items()):
            declared = spine_methods.get((crate, section), {}).get(name, set())
            found = source_methods.get(name, set())
            stale_exempt = False
            minting_macro = minted_types.get(name)
            if minting_macro is not None:
                extracted = generated_methods.get(minting_macro, set())
                if extracted:
                    found = found | extracted
                else:
                    stale_exempt = True
                placeholder = METHOD_MINTING_MACROS[minting_macro]
                if placeholder is not None:
                    declared = declared | placeholder_methods.get(
                        placeholder, set()
                    )
            for method in sorted(found - declared):
                failures.append(
                    f"{crate}::{section}::{name} has public method {method}()"
                    f" with no declaration in the '{crate}: {section}' section"
                )
            for method in sorted(declared - found):
                if stale_exempt:
                    continue
                failures.append(
                    f"'{crate}: {section}' section declares {name}::{method}(),"
                    " which source no longer defines as a public method"
                )
        for (section_crate, label), by_type in spine_methods.items():
            if section_crate != crate:
                continue
            for name in sorted(by_type):
                if home_section.get(name) != label:
                    failures.append(
                        f"'{crate}: {label}' section declares methods for"
                        f" {name}, which is not part of that module's export"
                        " surface"
                    )

    expected, free_functions, duplicate_rows = parse_inventory(spine_text)
    failures.extend(duplicate_rows)
    if not expected:
        failures.append("could not parse any Inventory table rows")
    for (crate, label), count in expected.items():
        if label == "lib.rs identities":
            actual = len(identities)
        else:
            actual = len(all_exports[crate].get(label, set()))
        if actual != count:
            failures.append(
                f"inventory row '{crate}: {label}' says {count} but lib.rs exports {actual}"
            )
    for crate, exports in all_exports.items():
        for module in exports:
            if (crate, module) not in expected:
                failures.append(
                    f"{crate}: {module} has exports but no Inventory table row"
                )

    totals: dict[str, int] = {}
    total_free_functions: dict[str, int] = {}
    for crate, count, extra in re.findall(
        r"^\| \*\*signalbox-(domain|application) total\*\*\s*\|"
        r" \*\*(\d+)(?: \(\+(\d+) free fn\))?\*\*\s*\|",
        spine_text,
        re.MULTILINE,
    ):
        if crate in totals:
            failures.append(
                f"Inventory table has more than one signalbox-{crate} total row"
            )
        totals[crate] = int(count) + int(extra or 0)
        total_free_functions[crate] = int(extra or 0)
    if ("domain", "lib.rs identities") not in expected:
        failures.append(
            "Inventory table is missing the 'domain: lib.rs identities' row"
        )
    for crate in CRATES:
        claimed = totals.get(crate)
        actual = sum(count for (c, _), count in expected.items() if c == crate)
        if claimed is None:
            failures.append(f"no aggregate total row found for signalbox-{crate}")
        elif claimed != actual:
            failures.append(
                f"signalbox-{crate} total row says {claimed} but per-module rows sum to {actual}"
            )
        claimed_free = total_free_functions.get(crate)
        actual_free = sum(
            count for (row_crate, _), count in free_functions.items() if row_crate == crate
        )
        if claimed_free is not None and claimed_free != actual_free:
            failures.append(
                f"signalbox-{crate} total row says {claimed_free} free functions "
                f"but per-module rows sum to {actual_free}"
            )

    if failures:
        print("domain-spine check FAILED — docs/domain-spine.md is out of sync:")
        for failure in failures:
            print(f"  - {failure}")
        print("Update docs/domain-spine.md in the same change as the public API.")
        return 1
    print("domain-spine check passed")
    return 0


if __name__ == "__main__":
    sys.exit(main())
