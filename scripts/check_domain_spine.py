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
   section. Source truth is every column-0 `impl` block in the crate's
   source tree (trait impls contribute nothing: Rust rejects `pub` on
   their items); spine truth is the owning section's impl-block `pub fn`
   lines plus its `// accessors:` comment lists. A public method with no
   declaration fails, as does a declared method the source no longer
   defines. Types whose surface comes from a column-0 macro invocation
   naming them (`define_identity!`, `goal_text!`, `bounded_text!`,
   `positive_position!`) are exempt from the stale direction only — their
   generated methods are invisible to this textual scan — and an impl
   header the scan cannot parse fails loudly.

Known limitation of this mechanical check: signatures, associated consts
and types, trait items, and enum variant lists inside a declaration are
not validated — method names on listed types are, but keeping the rest
faithful is a review responsibility (cargo public-api is the upgrade path
if these tripwires prove insufficient).

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
    rf"^pub {MODIFIERS}(?:struct|enum|trait|fn) ([A-Za-z_][A-Za-z0-9_]*)"
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
# The only macros trusted to mint method surface on the types they name; any
# other invocation naming a listed type earns no stale exemption, so a new
# method-generating macro must be added here before its types can pass.
METHOD_MINTING_MACROS = frozenset(
    {"define_identity", "goal_text", "bounded_text", "positive_position"}
)
CAPITALIZED_NAME = re.compile(r"\b([A-Z][A-Za-z0-9_]*)\b")
ANGLE_TOKEN = re.compile(r"->|<|>|\bfor\b")
# A cfg predicate that names `test` outside not(...) removes the item from
# library builds (string contents are already blanked, so a feature name
# containing "test" cannot match). `#[cfg(not(test))]` stays public surface,
# and `#[cfg_attr(...)]` never gates compilation, so neither matches here.
CFG_TEST_ATTRIBUTE = re.compile(r"^#\[cfg\(.*\btest\b")
CFG_NOT_TEST = re.compile(r"\bnot\s*\(\s*test\b")
ATTRIBUTE_LINE = re.compile(r"^#\[")


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
    in scripts/check_style_rules.py: raw string literals and the apostrophe's
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
    such as `Foo<{ 1 + 1 }>` — is part of the self type, not the body, so
    the opener is the first brace at angle depth zero (`->` skipped).
    Angle tracking suspends inside those const braces, where `<` is a
    comparison operator (`Foo<{ 1 < 2 }>`), not a delimiter.
    """
    angle_depth = 0
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
        elif char == "{":
            if angle_depth == 0:
                return index
            const_brace_depth = 1
        index += 1
    return None


def parse_source_methods(
    crate: str, path: Path
) -> tuple[dict[str, set[str]], list[str]]:
    """Map self-type name -> public method names from column-0 impl blocks.

    Only `pub fn` items at impl depth are public method surface; trait
    impls attribute to their self type but can never contribute (`pub` on
    a trait-impl item is rejected by the compiler). Impls nested inside
    inline modules are out of scope by the column-0 rule, which is what
    keeps `mod tests` fixtures out of the surface, and a column-0 impl
    under `#[cfg(test)]` is skipped for the same reason. Attribution is by
    unqualified self-type name: an inherent impl may legally live anywhere
    in its crate, so a private type sharing a listed type's name would
    misattribute — loudly, as a spurious missing declaration — and would
    need this parser extended to path-aware attribution.
    """
    lines = blank_comments_and_strings(path.read_text()).splitlines()
    methods: dict[str, set[str]] = {}
    failures: list[str] = []
    test_configured = False
    index = 0
    while index < len(lines):
        line = lines[index]
        if IMPL_HEADER.match(line):
            header_start = index
            header = line
            while body_brace_offset(header) is None and index + 1 < len(lines):
                index += 1
                header = f"{header} {lines[index]}"
            opener = body_brace_offset(header)
            was_test_configured = test_configured
            test_configured = False
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
            depth = remainder.count("{") - remainder.count("}")
            while depth > 0 and index + 1 < len(lines):
                index += 1
                body_line = lines[index]
                if depth == 1 and not was_test_configured:
                    method = IMPL_METHOD.match(body_line)
                    if method:
                        methods.setdefault(target, set()).add(method.group(1))
                depth += body_line.count("{") - body_line.count("}")
        elif CFG_TEST_ATTRIBUTE.match(line) and not CFG_NOT_TEST.search(line):
            test_configured = True
        elif not ATTRIBUTE_LINE.match(line) and line.strip():
            test_configured = False
        index += 1
    return methods, failures


def parse_macro_surface_types(text: str) -> set[str]:
    """Type names handed to the column-0 method-minting macro invocations.

    These types keep their declared spine methods without a textual source
    counterpart (the stale direction skips them). Only the macros in
    METHOD_MINTING_MACROS qualify — an assertion or derive helper naming a
    listed type mints nothing, so it earns no exemption. Comments and
    string contents are blanked before matching, so a commented-out
    invocation grants nothing and doc comments inside the invocation
    contribute no names. Only parenthesized invocations are read; a brace
    or bracket form that mints public surface would need this parser
    extended.
    """
    names: set[str] = set()
    for invocation, arguments in MACRO_INVOCATION.findall(
        blank_comments_and_strings(text)
    ):
        if invocation.split("::")[-1] not in METHOD_MINTING_MACROS:
            continue
        names.update(CAPITALIZED_NAME.findall(arguments))
    return names


def parse_spine_method_declarations(
    spine_text: str,
) -> tuple[dict[tuple[str, str], dict[str, set[str]]], list[str]]:
    """Map (crate, section label) -> type name -> declared method names.

    A method is declared by a `pub fn` line inside a section's column-0
    impl block, or by an `// accessors:` comment list — inside an impl
    block, or at column 0 attached to the nearest preceding `pub
    struct`/`pub enum` line. An accessor list continues onto following
    comment lines for as long as each line ends with a comma. Declaring
    the same method twice for one type is reported, matching how the
    name-level parser rejects duplicate type declarations.
    """
    sections: dict[tuple[str, str], dict[str, set[str]]] = {}
    duplicates: list[str] = []

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
                accessor_continues = line.rstrip().endswith(",")
                continue
            accessor_continues = False
            method = IMPL_METHOD.match(line)
            if method and impl_type:
                declare(current, impl_type, [method.group(1)])
            if line.startswith("}"):
                in_impl = False
                impl_type = None
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
    return sections, duplicates


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
    spine_methods, method_duplicates = parse_spine_method_declarations(
        spine_text
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
        macro_surface: set[str] = set()
        for path in sorted(lib_path.parent.rglob("*.rs")):
            found, unparsed = parse_source_methods(crate, path)
            for name, methods in found.items():
                source_methods.setdefault(name, set()).update(methods)
            failures.extend(unparsed)
            macro_surface.update(parse_macro_surface_types(path.read_text()))
        for name, section in sorted(home_section.items()):
            declared = spine_methods.get((crate, section), {}).get(name, set())
            found = source_methods.get(name, set())
            for method in sorted(found - declared):
                failures.append(
                    f"{crate}::{section}::{name} has public method {method}()"
                    f" with no declaration in the '{crate}: {section}' section"
                )
            for method in sorted(declared - found):
                if name in macro_surface:
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
