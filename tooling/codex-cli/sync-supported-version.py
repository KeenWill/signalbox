#!/usr/bin/env python3
"""Move the Codex adapter coverage marker to the exact npm pin."""

from __future__ import annotations

import json
import re
import sys
from pathlib import Path

ROOT = Path(__file__).resolve().parents[2]
MANIFEST = ROOT / "tooling/codex-cli/package.json"
RUNTIME = ROOT / "crates/model-runtime-codex-cli/src/runtime.rs"
PACKAGE = "@openai/codex"
EXACT_VERSION = re.compile(r"[0-9]+\.[0-9]+\.[0-9]+")
COVERAGE_MARKER = re.compile(
    r'(?P<prefix>pub const SUPPORTED_CODEX_CLI_VERSION: &str = ")'
    r'(?P<version>[^"]+)'
    r'(?P<suffix>";)'
)


def main() -> int:
    """Replace the unique adapter marker with the manifest's exact pin."""
    manifest = json.loads(MANIFEST.read_text(encoding="utf-8"))
    try:
        pinned = manifest["dependencies"][PACKAGE]
    except (KeyError, TypeError):
        print(f"{MANIFEST.relative_to(ROOT)} does not pin {PACKAGE}", file=sys.stderr)
        return 1
    if not isinstance(pinned, str) or EXACT_VERSION.fullmatch(pinned) is None:
        print(f"{PACKAGE} is not pinned to an exact major.minor.patch", file=sys.stderr)
        return 1

    runtime = RUNTIME.read_text(encoding="utf-8")
    markers = list(COVERAGE_MARKER.finditer(runtime))
    if len(markers) != 1:
        print(
            f"{RUNTIME.relative_to(ROOT)} must declare exactly one coverage marker",
            file=sys.stderr,
        )
        return 1
    current = markers[0].group("version")
    if current == pinned:
        print(f"Codex CLI coverage marker already matches {pinned}")
        return 0

    updated, replacements = COVERAGE_MARKER.subn(
        rf"\g<prefix>{pinned}\g<suffix>", runtime
    )
    if replacements != 1:
        print("failed to move the Codex CLI coverage marker", file=sys.stderr)
        return 1
    RUNTIME.write_text(updated, encoding="utf-8")
    print(
        f"moved Codex CLI coverage marker from {current} to {pinned}; "
        "review the fixture corpus before pushing"
    )
    return 0


if __name__ == "__main__":
    sys.exit(main())
