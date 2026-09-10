#!/usr/bin/env python3
"""Select PostgreSQL suites using Bazel's current source dependency graph."""

from __future__ import annotations

import argparse
import json
from pathlib import Path
import subprocess
import sys

from postgres_integration_suites import ROOT, Suite, load_suites, run_matrix


def source_path(label: str) -> str | None:
    """Convert a main-repository source label to a repository-relative path."""
    if label.startswith("@@//"):
        label = label[2:]
    elif label.startswith("@//"):
        label = label[1:]
    if not label.startswith("//"):
        return None
    package, name = label[2:].split(":", 1)
    return f"{package}/{name}" if package else name


def query_sources(suite: Suite, root: Path) -> set[str]:
    """Ask Bazel for source inputs across all configurations of one suite."""
    target = "//:postgres_" + suite.name.replace("-", "_")
    result = subprocess.run(
        ["bazel", "query", f'kind("source file", deps(tests({target})))',
         "--output=label", "--noshow_progress"],
        cwd=root, check=True, stdout=subprocess.PIPE, text=True,
    )
    return {path for label in result.stdout.splitlines()
            if (path := source_path(label)) is not None}


def select_suites(
    suites: tuple[Suite, ...], paths: list[str], root: Path
) -> tuple[Suite, ...]:
    """Narrow source-only edits; uncertainty and shared inputs run every suite."""
    # Build definitions, manifests, fixtures, renames/deletions and unknown inputs
    # retain the full bar. Querying the head alone cannot see a removed input.
    if not paths or any(
        not path.endswith(".rs")
        or not path.startswith(("apps/", "crates/"))
        or not (root / path).is_file()
        for path in paths
    ):
        return suites
    dependencies = {suite.name: query_sources(suite, root) for suite in suites}
    known_sources = set().union(*dependencies.values())
    if not set(paths) <= known_sources:
        return suites
    return tuple(suite for suite in suites if set(paths) & dependencies[suite.name])


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--changed-paths", default="[]", help="JSON array of changed paths")
    arguments = parser.parse_args()
    paths = json.loads(arguments.changed_paths)
    if not isinstance(paths, list) or not all(isinstance(path, str) for path in paths):
        parser.error("--changed-paths must be a JSON array of strings")
    suites = select_suites(load_suites(ROOT), paths, ROOT)
    print(json.dumps(run_matrix(suites), separators=(",", ":"), sort_keys=True))
    return 0


if __name__ == "__main__":
    sys.exit(main())
