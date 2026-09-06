#!/usr/bin/env python3
"""Reject module dependency edges and SQL reach-arounds across the ownership seam."""

from __future__ import annotations

import re
import sys
import tomllib
from pathlib import Path

ROOT = Path.cwd().resolve()
MODULE_ROOT = ROOT / "crates" / "modules"
SEAM_ROOT = (ROOT / "crates" / "ownership-seam").resolve()
FORBIDDEN_IMPORTS = (
    "signalbox_application",
    "signalbox_domain",
    "signalbox_persistence",
)
PUBLIC_RELATION = re.compile(r"\bpublic\s*\.", re.IGNORECASE)
MODULE_RELATION = re.compile(r"\bmod_[a-z][a-z0-9_]*\s*\.", re.IGNORECASE)


def dependency_tables(manifest: dict[str, object]) -> list[dict[str, object]]:
    tables = []
    for name in ("dependencies", "dev-dependencies", "build-dependencies"):
        table = manifest.get(name)
        if isinstance(table, dict):
            tables.append(table)
    target = manifest.get("target")
    if isinstance(target, dict):
        for target_table in target.values():
            if isinstance(target_table, dict):
                tables.extend(dependency_tables(target_table))
    return tables


def workspace_dependencies() -> dict[str, object]:
    path = ROOT / "Cargo.toml"
    if not path.is_file():
        return {}
    manifest = tomllib.loads(path.read_text(encoding="utf-8"))
    workspace = manifest.get("workspace")
    if not isinstance(workspace, dict):
        return {}
    dependencies = workspace.get("dependencies")
    return dependencies if isinstance(dependencies, dict) else {}


def check_manifest(path: Path, inherited: dict[str, object]) -> list[str]:
    manifest = tomllib.loads(path.read_text(encoding="utf-8"))
    failures = []
    for table in dependency_tables(manifest):
        for name, configured in table.items():
            is_inherited = isinstance(configured, dict) and configured.get("workspace") is True
            effective = inherited.get(name, configured) if is_inherited else configured
            normalized = name.replace("_", "-")
            package = effective.get("package") if isinstance(effective, dict) else None
            normalized_package = package.replace("_", "-") if isinstance(package, str) else ""
            dependency_names = (normalized, normalized_package)
            if any(
                candidate.startswith("signalbox-")
                and candidate != "signalbox-ownership-seam"
                for candidate in dependency_names
            ):
                failures.append(f"{path}: forbidden Signalbox dependency {name}")
            if isinstance(effective, dict) and isinstance(effective.get("path"), str):
                base = ROOT if is_inherited else path.parent
                target = (base / effective["path"]).resolve()
                if target != SEAM_ROOT and ROOT in target.parents:
                    failures.append(f"{path}: forbidden workspace path dependency {name}")
    return failures


def check_module_source(path: Path) -> list[str]:
    source = path.read_text(encoding="utf-8")
    failures = [
        f"{path}: forbidden import {name}"
        for name in FORBIDDEN_IMPORTS
        if name in source
    ]
    if PUBLIC_RELATION.search(source):
        failures.append(f"{path}: module SQL names a public relation")
    return failures


def check_core_source(path: Path) -> list[str]:
    source = path.read_text(encoding="utf-8")
    if MODULE_RELATION.search(source):
        return [f"{path}: core SQL names a module relation"]
    return []


def main() -> int:
    failures: list[str] = []
    if MODULE_ROOT.is_dir():
        inherited = workspace_dependencies()
        for manifest in sorted(MODULE_ROOT.glob("*/Cargo.toml")):
            failures.extend(check_manifest(manifest, inherited))
        for source in sorted(MODULE_ROOT.glob("*/src/**/*")):
            if source.suffix in {".rs", ".sql"}:
                failures.extend(check_module_source(source))

    for root in (ROOT / "crates", ROOT / "apps"):
        if not root.is_dir():
            continue
        for source in sorted(root.glob("**/*")):
            if MODULE_ROOT in source.parents:
                continue
            if source.suffix in {".rs", ".sql"}:
                failures.extend(check_core_source(source))

    if failures:
        print("ownership-seam check failed:", file=sys.stderr)
        for failure in failures:
            print(f"  {failure}", file=sys.stderr)
        return 1
    print("ownership-seam check passed")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
