#!/usr/bin/env python3
"""Check that docs/domain-spine.md stays in sync with the public API.

Ground truth is each crate's lib.rs export surface: `pub use` re-exports plus
the `define_identity!` invocations in the domain crate. The check fails when

1. an exported public name does not appear in the spine, or
2. a per-module count in the spine's Inventory table disagrees with the
   number of names lib.rs exports from that module.

The spine may say more than the export surface (sealed markers, semantics
notes); it may not say less. Run from the repository root: exits nonzero with
a per-item report on any mismatch.
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
    text = lib_rs.read_text()
    return set(
        re.findall(
            r"define_identity!\(\s*(?:///[^\n]*\n\s*)*([A-Za-z_][A-Za-z0-9_]*)\s*\)",
            text,
        )
    )


def parse_inventory(spine_text: str) -> dict[tuple[str, str], int]:
    """Map (crate, module-label) -> expected export count from the table.

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
    all_exports: dict[str, dict[str, set[str]]] = {
        crate: parse_exports(path) for crate, path in CRATES.items()
    }

    for name in sorted(identities):
        if not re.search(rf"\b{name}\b", spine_text):
            failures.append(f"identity {name} is missing from the spine")
    for crate, exports in all_exports.items():
        for module, names in exports.items():
            for name in sorted(names):
                if not re.search(rf"\b{name}\b", spine_text):
                    failures.append(
                        f"{crate}::{module}::{name} is exported but missing from the spine"
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
