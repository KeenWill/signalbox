#!/usr/bin/env python3
"""Summarize committed cargo-public-api snapshots and their working-tree diff."""

import os
import re
import subprocess
from collections import Counter, defaultdict
from dataclasses import dataclass
from pathlib import Path

SNAPSHOTS = {
    "signalbox-domain": Path("docs/api/signalbox-domain.txt"),
    "signalbox-application": Path("docs/api/signalbox-application.txt"),
}
DECLARATION = re.compile(
    r'^pub (?:(?:async|const|unsafe)\s+|extern\s+"[^"]*"\s+)*'
    r"(?P<kind>struct|enum|union|type|trait|fn|const)\s+"
)
MODULE = re.compile(r"^pub mod (?P<name>[A-Za-z_][A-Za-z0-9_#:]*)(?:\b|$)")


@dataclass(frozen=True)
class Item:
    module: str
    category: str
    name: str
    declaration: str


def git_text(revision: str, path: Path) -> str:
    result = subprocess.run(
        ["git", "show", f"{revision}:{path.as_posix()}"],
        check=False,
        capture_output=True,
        text=True,
    )
    return result.stdout if result.returncode == 0 else ""


def previous_text(path: Path, current: str) -> str:
    if base := os.environ.get("DOMAIN_SPINE_BASE"):
        return git_text(base, path) or current
    dirty = subprocess.run(
        ["git", "diff", "--quiet", "HEAD", "--", path.as_posix()], check=False
    )
    revision = "HEAD" if dirty.returncode == 1 else "HEAD^"
    return git_text(revision, path) or current


def item_name(line: str, start: int) -> str:
    name: list[str] = []
    generic_depth = 0
    for char in line[start:]:
        if char == "<":
            generic_depth += 1
        elif char == ">":
            generic_depth -= 1
        elif generic_depth == 0 and (char.isspace() or char in "(={"):
            break
        elif generic_depth == 0:
            name.append(char)
    return "".join(name)


def parse(text: str) -> tuple[str, list[Item]]:
    lines = text.splitlines()
    modules = {match.group("name") for line in lines if (match := MODULE.match(line))}
    root = min(modules, key=lambda name: name.count("::"))
    items: list[Item] = []
    include_nested_functions = False
    for line in lines:
        if line.startswith(("impl ", "impl<")):
            target = line.split(" where ", 1)[0]
            include_nested_functions = " for " not in target and any(
                f"{module}::" in target for module in modules
            )
        match = DECLARATION.match(line)
        if match is None:
            continue
        name = item_name(line, match.end())
        owners = [module for module in modules if name.startswith(f"{module}::")]
        module = max(owners, key=len, default=root)
        kind = match.group("kind")
        direct_child = "::" not in name.removeprefix(f"{module}::")
        if kind == "trait":
            include_nested_functions = direct_child
        if kind in {"struct", "enum", "union", "type", "trait"} and not direct_child:
            continue
        if kind == "fn" and not (direct_child or include_nested_functions):
            continue
        category = (
            "type"
            if kind in {"struct", "enum", "union", "type"}
            else "function"
            if kind == "fn"
            else "constant"
            if kind == "const"
            else kind
        )
        items.append(Item(module, category, name, line))
    return root, items


def identity(item: Item) -> tuple[str, str, str, str]:
    return item.module, item.category, item.name, item.declaration


def display_name(item: Item) -> str:
    return item.name.removeprefix(f"{item.module}::")


def render(crate: str, path: Path) -> None:
    current_text = path.read_text()
    root, current = parse(current_text)
    baseline = previous_text(path, current_text)
    _, previous = parse(baseline) if baseline else (root, [])
    current_counts = Counter(identity(item) for item in current)
    previous_counts = Counter(identity(item) for item in previous)
    added = current_counts - previous_counts
    removed = previous_counts - current_counts
    by_module: dict[str, list[Item]] = defaultdict(list)
    for item in current:
        by_module[item.module].append(item)
    changed_modules = {key[0] for key in added + removed}

    print(crate)
    for module in sorted(set(by_module) | changed_modules):
        counts = Counter(item.category for item in by_module[module])
        label = module.removeprefix(root).removeprefix("::") or "(root)"
        print(
            f"  {label}: types={counts['type']} traits={counts['trait']} "
            f"functions={counts['function']}"
        )
        for heading, changes in (("added", added), ("removed", removed)):
            names = sorted(
                {
                    f"{category} {display_name(Item(owner, category, name, line))}"
                    for (owner, category, name, line), count in changes.items()
                    if owner == module and count
                }
            )
            print(f"    {heading}: {', '.join(names) if names else 'none'}")


def main() -> None:
    for crate, path in SNAPSHOTS.items():
        render(crate, path)


if __name__ == "__main__":
    main()
