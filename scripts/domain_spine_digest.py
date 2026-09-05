#!/usr/bin/env python3
"""Summarize committed cargo-public-api snapshots and their working-tree diff."""

import os
import re
import subprocess
from collections import Counter, defaultdict
from pathlib import Path
from typing import NamedTuple

SNAPSHOTS = {
    "signalbox-domain": Path("docs/api/signalbox-domain.txt"),
    "signalbox-application": Path("docs/api/signalbox-application.txt"),
}
DECLARATION = re.compile(
    r'^pub (?:(?:async|const|unsafe)\s+|extern\s+"[^"]*"\s+)*'
    r"(?P<kind>struct|enum|union|type|trait|fn|const|static|macro)\s+"
)
MODULE = re.compile(r"^pub mod (?P<name>[A-Za-z_][A-Za-z0-9_#:]*)(?:\b|$)")
AUTO_TRAIT = re.compile(
    r"^core::(?:marker::(?:Freeze|Send|StructuralPartialEq|"
    r"Sync|Unpin|UnsafeUnpin)|"
    r"panic::unwind_safe::(?:RefUnwindSafe|UnwindSafe))$"
)


Item = NamedTuple(
    "Item", [("module", str), ("category", str), ("name", str), ("declaration", str)]
)


def git_text(revision: str, path: Path) -> str:
    result = subprocess.run(
        ["git", "show", f"{revision}:{path.as_posix()}"],
        check=False, capture_output=True, text=True,
    )
    return result.stdout if result.returncode == 0 else ""


def previous_text(path: Path, current: str) -> str:
    if base := os.environ.get("DOMAIN_SPINE_BASE"):
        return git_text(base, path) or current
    dirty = subprocess.run(
        ["git", "diff", "--quiet", "HEAD", "--", path.as_posix()], check=False)
    revision = "HEAD" if dirty.returncode == 1 else "HEAD^"
    return git_text(revision, path) or current


def without_generics(text: str) -> str:
    result: list[str] = []
    depth = 0
    for char in text:
        if char == "<":
            depth += 1
        elif char == ">":
            depth -= 1
        elif depth == 0:
            result.append(char)
    return "".join(result)


def item_name(line: str, start: int) -> str:
    return re.split(r"[\s(={]", without_generics(line[start:]), 1)[0].rstrip(":")


def parse(text: str) -> tuple[str, list[Item]]:
    lines = text.splitlines()
    module_lines = [
        (match.group("name"), line)
        for line in lines if (match := MODULE.match(line))
    ]
    modules = {name for name, _ in module_lines}
    root = min(modules, key=lambda name: name.count("::"))
    items = [
        Item(name.rpartition("::")[0] or root, "module", name, line)
        for name, line in module_lines
    ]
    include_nested_functions = include_associated_types = False
    for line in lines:
        if line.startswith(("impl ", "impl<")):
            include_associated_types = False
            target = line.split(" where ", 1)[0]
            include_nested_functions = " for " not in target and any(
                f"{module}::" in target for module in modules)
            normalized = without_generics(line[4:]).split(" where ", 1)[0].strip()
            if " for " not in normalized:
                owners = [module for module in modules if normalized.startswith(module)]
                if owners:
                    module = max(owners, key=len)
                    items.append(Item(module, "implementation", f"{normalized} (inherent)", line))
            else:
                trait, subject = normalized.split(" for ", 1)
                raw_subject = line.split(" for ", 1)[1].split(" where ", 1)[0]
                owners = [module for module in modules if subject.startswith(module)]
                parameters = re.findall(r"(?:^|,\s*)(?:const\s+)?('?\w+)", line[5:line.find(">")])
                source_owned = not line.startswith("impl<") or any(
                    re.search(rf"(?<!\w){re.escape(name)}(?!\w)", raw_subject)
                    for name in parameters
                )
                if owners and not AUTO_TRAIT.match(trait) and (
                    source_owned or any(trait.startswith(module) for module in modules)
                ):
                    module = max(owners, key=len)
                    name = f"{subject} as {trait}"
                    items.append(Item(module, "implementation", name, line))
                    include_associated_types = True
        match = DECLARATION.match(line)
        if match is None:
            if line.startswith(f"pub {root}::"):
                name = item_name(line, 4)
                owners = [module for module in modules if name.startswith(f"{module}::")]
                module = max(owners, key=len, default=root)
                items.append(Item(module, "member", name, line))
            continue
        name = item_name(line, match.end())
        owners = [module for module in modules if name.startswith(f"{module}::")]
        module = max(owners, key=len, default=root)
        kind = match.group("kind")
        direct_child = "::" not in name.removeprefix(f"{module}::")
        if kind == "trait":
            include_nested_functions = direct_child
            include_associated_types = direct_child
        if kind in {"struct", "enum", "union", "type", "trait"} and not direct_child:
            if kind == "type" and include_associated_types:
                items.append(Item(module, "associated type", name, line))
            continue
        if kind == "fn" and not (direct_child or include_nested_functions):
            continue
        category = "type" if kind in {"struct", "enum", "union"} else {
            "fn": "function", "const": "constant"
        }.get(kind, kind)
        items.append(Item(module, category, name, line))
    return root, items


def render(crate: str, path: Path) -> None:
    current_text = path.read_text()
    root, current = parse(current_text)
    _, previous = parse(previous_text(path, current_text))
    added = Counter(current) - Counter(previous)
    removed = Counter(previous) - Counter(current)
    by_module: dict[str, list[Item]] = defaultdict(list)
    for item in current:
        by_module[item.module].append(item)
    changed_modules = {item.module for item in added + removed}

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
                    f"{item.category} {item.name.removeprefix(f'{item.module}::')}"
                    for item, count in changes.items()
                    if item.module == module and count
                }
            )
            print(f"    {heading}: {', '.join(names) if names else 'none'}")
if __name__ == "__main__":
    for crate, path in SNAPSHOTS.items():
        render(crate, path)
