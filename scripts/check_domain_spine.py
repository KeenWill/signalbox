#!/usr/bin/env python3
"""Check that docs/domain-spine.md stays in sync with the public API.

Ground truth is each crate's lib.rs: `pub use` re-exports, the domain crate's
`define_identity!` invocations, and any directly declared crate-root public
item. The spine is parsed per `## crate: module` section, taking column-0
`pub struct/enum/trait/fn` lines as its declarations. The check fails when

1. an exported name has no declaration in its owning module's section
   (a mention elsewhere in the document does not count),
2. a section declares a name its module no longer exports (stale declaration),
3. a lib.rs declares a public item outside `pub use`/`define_identity!`
   that this mapping does not cover, or
4. a per-module count in the Inventory table disagrees with the export
   surface, an aggregate total row disagrees with the per-module sum, an
   exporting module has no Inventory row, or a section declares the same
   name twice.

Known limitation, accepted in the decision log: signatures, associated
items, and enum variant lists inside a declaration are not validated —
keeping those faithful is a review responsibility (cargo public-api is the
upgrade path if name/count tripwires prove insufficient).

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
    rf"^pub {MODIFIERS}(?:struct|enum|trait|fn|static|type|const) ([A-Za-z_][A-Za-z0-9_]*)",
    re.MULTILINE,
)


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


def parse_spine_sections(
    spine_text: str,
) -> tuple[dict[tuple[str, str], set[str]], list[str]]:
    """Map (crate, section label) -> declared names; also report duplicates."""
    sections: dict[tuple[str, str], set[str]] = {}
    duplicates: list[str] = []
    current: tuple[str, str] | None = None
    for line in spine_text.splitlines():
        header = re.match(r"^## (domain|application): (.+)$", line)
        if header:
            current = (header.group(1), header.group(2).strip())
            sections.setdefault(current, set())
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
    """
    expected: dict[tuple[str, str], int] = {}
    for crate, label, count, extra in re.findall(
        r"^\| (domain|application): ([^|]+?) \| (\d+)(?: \(\+(\d+) free fn\))?[^|]*\|",
        spine_text,
        re.MULTILINE,
    ):
        expected[(crate, label.strip())] = int(count) + int(extra or 0)
    return expected


def main() -> int:
    spine_text = SPINE.read_text()
    failures: list[str] = []

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

    expected = parse_inventory(spine_text)
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

    totals = {
        crate: int(count) + int(extra or 0)
        for crate, count, extra in re.findall(
            r"^\| \*\*signalbox-(domain|application) total\*\* \|"
            r" \*\*(\d+)(?: \(\+(\d+) free fn\))?\*\* \|",
            spine_text,
            re.MULTILINE,
        )
    }
    for crate in CRATES:
        claimed = totals.get(crate)
        actual = sum(count for (c, _), count in expected.items() if c == crate)
        if claimed is None:
            failures.append(f"no aggregate total row found for signalbox-{crate}")
        elif claimed != actual:
            failures.append(
                f"signalbox-{crate} total row says {claimed} but per-module rows sum to {actual}"
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
