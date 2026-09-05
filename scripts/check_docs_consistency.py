#!/usr/bin/env python3
"""Check repository-relative links across the living documentation surface."""

from __future__ import annotations

import re
import subprocess
import sys
import tomllib
from dataclasses import dataclass
from functools import lru_cache
from html.parser import HTMLParser
from pathlib import Path
from urllib.parse import unquote, urlsplit

from markdown_it import MarkdownIt

import postgres_integration_suites

ROOT = Path(__file__).resolve().parent.parent
MARKDOWN = MarkdownIt("commonmark", {"html": True})


class TrackedFilesError(RuntimeError):
    """Raised when Git cannot provide the authoritative input set."""


@dataclass(frozen=True, order=True)
class Violation:
    path: str
    line: int
    category: str
    message: str

    def render(self) -> str:
        return f"{self.path}:{self.line}: [{self.category}] {self.message}"


@dataclass(frozen=True)
class Link:
    destination: str
    line: int
    is_image: bool = False


class AnchorParser(HTMLParser):
    def __init__(self) -> None:
        super().__init__(convert_charrefs=True)
        self.anchors: set[str] = set()

    def handle_starttag(
        self, tag: str, attrs: list[tuple[str, str | None]]
    ) -> None:
        if tag.casefold() == "a":
            values = dict(attrs)
            anchor = values.get("id") or values.get("name")
            if anchor:
                self.anchors.add(anchor)


def tracked_files(root: Path) -> list[Path]:
    result = subprocess.run(
        ["git", "ls-files", "-z", "--cached"],
        cwd=root,
        capture_output=True,
        check=False,
    )
    if result.returncode != 0:
        detail = result.stderr.decode(errors="replace").strip()
        raise TrackedFilesError(detail or "git ls-files failed")
    return [
        (root / entry.decode(errors="surrogateescape")).resolve()
        for entry in result.stdout.split(b"\0")
        if entry
    ]


def markdown_sources(root: Path) -> list[Path]:
    docs = (root / "docs").resolve()
    guidance = (root / "AGENTS.md").resolve()
    return [
        path
        for path in tracked_files(root)
        if path.exists()
        and path.suffix.casefold() == ".md"
        and (path == guidance or docs in path.parents)
    ]


def repository_path(root: Path, path: Path) -> str:
    return path.relative_to(root).as_posix()


def visible_inline_text(token) -> str:
    parts: list[str] = []
    for child in token.children or []:
        if child.type in {"text", "code_inline"}:
            parts.append(child.content)
        elif child.type == "image":
            parts.append(child.content)
    return "".join(parts)


def github_slug(text: str) -> str:
    text = text.strip().casefold()
    text = re.sub(r"[^\w\- ]", "", text, flags=re.UNICODE)
    return re.sub(r"[ \t\n]", "-", text)


def parsed_document(text: str) -> tuple[list[Link], set[str]]:
    tokens = MARKDOWN.parse(text)
    links: list[Link] = []
    anchors: set[str] = set()
    used_slugs: set[str] = set()
    for index, token in enumerate(tokens):
        line = (token.map[0] + 1) if token.map else 1
        if token.type == "inline":
            for child in token.children or []:
                if child.type == "link_open":
                    destination = child.attrGet("href")
                    if destination is not None:
                        links.append(Link(destination, line))
                elif child.type == "image":
                    destination = child.attrGet("src")
                    if destination is not None:
                        links.append(Link(destination, line, is_image=True))
                elif child.type == "html_inline":
                    parser = AnchorParser()
                    parser.feed(child.content)
                    anchors.update(parser.anchors)
        elif token.type == "html_block":
            parser = AnchorParser()
            parser.feed(token.content)
            anchors.update(parser.anchors)
        elif token.type == "heading_open" and index + 1 < len(tokens):
            inline = tokens[index + 1]
            if inline.type == "inline":
                base = github_slug(visible_inline_text(inline))
                slug = base
                suffix = 0
                while slug in used_slugs:
                    suffix += 1
                    slug = f"{base}-{suffix}"
                used_slugs.add(slug)
                anchors.add(slug)
    return links, anchors


def split_destination(destination: str) -> tuple[str, str] | None:
    try:
        parsed = urlsplit(destination)
    except ValueError:
        return None
    if parsed.scheme or parsed.netloc or destination.startswith(("/", "//")):
        return None
    return unquote(parsed.path), unquote(parsed.fragment)


def resolve_relative_target(
    source: Path, destination: str
) -> tuple[Path, str] | None:
    parts = split_destination(destination)
    if parts is None:
        return None
    path_text, fragment = parts
    target = source if not path_text else source.parent / path_text
    return target.resolve(), fragment


def is_inside(root: Path, path: Path) -> bool:
    try:
        path.relative_to(root)
    except ValueError:
        return False
    return True


@lru_cache(maxsize=None)
def heading_anchors(path: Path) -> set[str]:
    text = path.read_text(encoding="utf-8", errors="replace")
    return parsed_document(text)[1]


def check_relative_links(root: Path) -> list[Violation]:
    violations: list[Violation] = []
    for source in markdown_sources(root):
        text = source.read_text(encoding="utf-8", errors="replace")
        links, _ = parsed_document(text)
        source_label = repository_path(root, source)
        for link in links:
            resolved = resolve_relative_target(source, link.destination)
            if resolved is None:
                continue
            target, fragment = resolved
            if not is_inside(root, target):
                violations.append(Violation(source_label, link.line, "relative-link", f"target escapes the repository: `{link.destination}`"))
                continue
            if not target.exists():
                violations.append(Violation(source_label, link.line, "relative-link", f"target does not exist: `{link.destination}`"))
                continue
            if not fragment:
                continue
            anchor_target = target
            if target.is_dir() and (target / "README.md").is_file():
                anchor_target = target / "README.md"
            if anchor_target.suffix.casefold() in {".md", ".markdown"}:
                if fragment not in heading_anchors(anchor_target):
                    violations.append(Violation(source_label, link.line, "relative-link", f"anchor `#{fragment}` does not exist in `{repository_path(root, anchor_target)}`"))
                continue
            line_anchor = re.fullmatch(r"L([1-9][0-9]*)(?:-L([1-9][0-9]*))?", fragment)
            if target.is_file() and line_anchor:
                first = int(line_anchor.group(1))
                last = int(line_anchor.group(2) or first)
                available = len(target.read_text(encoding="utf-8", errors="replace").splitlines())
                if first <= last <= available:
                    continue
            violations.append(Violation(source_label, link.line, "relative-link", f"anchor `#{fragment}` does not resolve in `{repository_path(root, target)}`"))
    return violations


def check_machine_owner_links(root: Path) -> list[Violation]:
    owner = (root / "docs/spec/credential-availability.md").resolve()
    if not owner.exists():
        return []
    projecting_pages = (
        "docs/spec/turn-lifecycle-and-scheduling.md",
        "docs/spec/persistence-protocol.md",
        "docs/spec/sessions-and-transcript.md",
        "docs/spec/process-protocol.md",
        "docs/spec/runtime-substrate.md",
        "docs/spec/model-call-execution.md",
        "docs/spec/configuration-and-credentials.md",
    )
    violations: list[Violation] = []
    for name in projecting_pages:
        source = root / name
        if not source.exists():
            continue
        links, _ = parsed_document(source.read_text(encoding="utf-8"))
        if any(
            not link.is_image
            and (
                resolved := resolve_relative_target(source, link.destination)
            )
            is not None
            and resolved[0] == owner
            for link in links
        ):
            continue
        violations.append(Violation(name, 1, "machine-owner-link", "page projects the credential-availability machine but carries no resolving link to its owning specification"))
    return violations


def cargo_package_names(root: Path) -> set[str]:
    """Return package names declared by tracked Cargo manifests."""
    names: set[str] = set()
    for manifest in (path for path in tracked_files(root) if path.name == "Cargo.toml"):
        try:
            declared = tomllib.loads(manifest.read_text(encoding="utf-8"))
        except (OSError, tomllib.TOMLDecodeError):
            continue
        package = declared.get("package")
        name = package.get("name") if isinstance(package, dict) else None
        if isinstance(name, str):
            names.add(name)
    return names


def check_suite_manifest(root: Path) -> list[Violation]:
    """Hold the PostgreSQL suite manifest, workflow, and docs in agreement."""
    manifest = root / postgres_integration_suites.MANIFEST
    workflow = root / postgres_integration_suites.WORKFLOW
    label = postgres_integration_suites.MANIFEST.as_posix()
    if not manifest.is_file():
        if not workflow.is_file():
            return []
        return [
            Violation(
                postgres_integration_suites.WORKFLOW.as_posix(),
                1,
                "suite-manifest",
                f"the Rust workflow exists without {label}",
            )
        ]

    text = manifest.read_text(encoding="utf-8")
    suites = postgres_integration_suites.parse_suites(text)
    failures: list[Violation] = []
    packages = cargo_package_names(root)
    for suite in suites:
        if suite.package not in packages:
            failures.append(
                Violation(
                    label,
                    postgres_integration_suites.manifest_line(text, suite.name),
                    "suite-manifest",
                    f"suite `{suite.name}` names package `{suite.package}`, "
                    "which is not a package in this workspace",
                )
            )
    if workflow.is_file():
        failures.extend(
            Violation(
                postgres_integration_suites.WORKFLOW.as_posix(),
                1,
                "suite-manifest",
                message,
            )
            for message in postgres_integration_suites.workflow_disagreements(
                root, suites
            )
        )
    else:
        failures.append(
            Violation(
                label,
                1,
                "suite-manifest",
                f"{label} exists without {postgres_integration_suites.WORKFLOW}",
            )
        )
    for source in markdown_sources(root):
        source_label = repository_path(root, source)
        failures.extend(
            Violation(source_label, line, "suite-manifest", message)
            for line, message in postgres_integration_suites.documentation_disagreements(
                source_label,
                source.read_text(encoding="utf-8"),
                suites,
            )
        )
    return failures


def run_checks(root: Path = ROOT) -> list[Violation]:
    root = root.resolve()
    heading_anchors.cache_clear()
    failures = check_relative_links(root)
    failures.extend(check_machine_owner_links(root))
    failures.extend(check_suite_manifest(root))
    return sorted(set(failures))


def main() -> int:
    try:
        failures = run_checks()
    except (TrackedFilesError, postgres_integration_suites.ManifestError) as error:
        print(f"docs-consistency check FAILED: {error}")
        return 1
    if failures:
        print("docs-consistency check FAILED:")
        for failure in failures:
            print(f"  - {failure.render()}")
        return 1
    print("documentation links and ownership references are consistent")
    return 0


if __name__ == "__main__":
    sys.exit(main())
