#!/usr/bin/env python3
"""Summarize committed cargo-public-api snapshots and their working-tree diff."""

import os
import re
import subprocess
from collections import Counter, defaultdict, namedtuple
from pathlib import Path

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
BLANKET_TRAIT = re.compile(
    r"^(?:alloc::(?:borrow::ToOwned|string::ToString)|"
    r"core::(?:any::Any|borrow::Borrow(?:Mut)?|"
    r"clone::CloneToUninit|convert::(?:From|Into|TryFrom|TryInto))|"
    r"equivalent::(?:Comparable|Equivalent)|hashbrown::Equivalent|"
    r"tracing::instrument::(?:Instrument|WithSubscriber)|typenum::type_operators::Same)$"
)
Item = namedtuple("Item", "module category name declaration")


def git_text(revision: str, path: Path) -> str:
    result = subprocess.run(
        ["git", "show", f"{revision}:{path.as_posix()}"], check=False,
        capture_output=True, text=True)
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
    for index, char in enumerate(text):
        if char == "<":
            depth += 1
        elif char == ">" and text[index - 1:index] != "-":
            depth -= 1
        elif depth == 0:
            result.append(char)
    return "".join(result)


def item_name(line: str, start: int) -> str:
    return re.split(r"[\s(={]", without_generics(line[start:]), 1)[0].rstrip(":")


def implementation_parts(line: str) -> tuple[str, str]:
    body = line[4:].lstrip()
    parameters = ""
    if body.startswith("<"):
        depth = 0
        for index, char in enumerate(body):
            depth += char == "<"
            depth -= char == ">" and body[index - 1:index] != "-"
            if depth == 0:
                parameters = body[1:index]
                body = body[index + 1:].lstrip()
                break
    return parameters, body.split(" where ", 1)[0]


def owning_modules(text: str, modules: set[str]) -> list[str]:
    return [module for module in modules if re.search(
        rf"(?<![\w:]){re.escape(module)}(?=::|\b)", text)]


def parameter_names(text: str) -> set[str]:
    return set(re.findall(r"(?:^|,\s*)(?:const\s+)?('?\w+)", text))


def parse(text: str) -> tuple[str, list[Item]]:
    lines = text.splitlines()
    declared_parameters: dict[str, set[str]] = {}
    for line in lines:
        match = DECLARATION.match(line)
        if match is None or match.group("kind") not in {"struct", "enum", "union"}:
            continue
        body = line[match.end():]
        generic_start = body.find("<")
        name_end = re.search(r"[\s<(={]", body)
        if generic_start >= 0 and name_end and generic_start == name_end.start():
            parameters, _ = implementation_parts(f"impl{body[generic_start:]}")
            declared_parameters[body[:generic_start]] = parameter_names(parameters)
    module_lines = [(match.group("name"), line) for line in lines
                    if (match := MODULE.match(line))]
    modules = {name for name, _ in module_lines}
    root = min(modules, key=lambda name: name.count("::"))
    items = [Item(name.rpartition("::")[0] or root, "module", name, line)
             for name, line in module_lines]
    include_nested_functions = include_associated_types = False
    for line in lines:
        impl_line = line.removeprefix("unsafe ")
        if impl_line.startswith(("impl ", "impl<")):
            include_associated_types = False
            target = impl_line.split(" where ", 1)[0]
            include_nested_functions = " for " not in target and any(
                f"{module}::" in target for module in modules)
            parameter_text, signature = implementation_parts(impl_line)
            normalized = without_generics(signature)
            if " for " not in normalized:
                owners = owning_modules(signature, modules)
                if owners:
                    module = max(owners, key=len)
                    items.append(Item(module, "implementation", f"{signature} (inherent)", line))
            else:
                trait, subject = normalized.split(" for ", 1)
                raw_trait, raw_subject = signature.split(" for ", 1)
                subject_owners = owning_modules(raw_subject, modules)
                trait_owners = owning_modules(raw_trait, modules)
                owners = subject_owners or trait_owners
                parameters = parameter_names(parameter_text)
                own_parameters = declared_parameters.get(
                    without_generics(raw_subject), set())
                uses_subject_parameter = any(
                    name not in own_parameters
                    and re.search(rf"(?<!\w){re.escape(name)}(?!\w)", raw_subject)
                    for name in parameters
                )
                blanket = (
                    impl_line.startswith("impl<") and not uses_subject_parameter
                    and BLANKET_TRAIT.match(without_generics(trait))
                )
                if owners and not AUTO_TRAIT.match(trait.removeprefix("!")) and (
                    not blanket or trait_owners
                ):
                    module = max(owners, key=len)
                    name = f"{raw_subject} as {raw_trait}"
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
        kind = match.group("kind")
        start = match.end() + (4 if kind == "static" and line[match.end():].startswith("mut ") else 0)
        name = item_name(line, start)
        owners = [module for module in modules if name.startswith(f"{module}::")]
        module = max(owners, key=len, default=root)
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
    current = list(dict.fromkeys(current))
    previous = list(dict.fromkeys(previous))
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
