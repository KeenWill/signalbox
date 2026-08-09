#!/usr/bin/env python3
"""Check that the workspace panic gate reaches every member crate.

The workspace denies the panic-producing macros and the panicking `Option`
and `Result` accessors in `[workspace.lints.clippy]`, and `cargo clippy -D
warnings` fails any production site that uses one. That gate is what lets the
repository treat a panic in production code as a compile-time impossibility
rather than a review obligation.

Clippy cannot check the gate's own reach. A workspace lint table applies to a
member only where that member's manifest opts in with `[lints] workspace =
true`; a member that omits the stanza inherits nothing and is silently exempt
from every deny above. Nothing fails, no warning is printed, and `cargo clippy
--workspace` stays green while the new crate accumulates `unwrap()` and
`unreachable!()`. The same silence covers the other direction: deleting a line
from the workspace table disarms that lint everywhere at once, and the only
evidence is that some existing violation stopped being reported.

Both holes are one-line edits in a manifest, which is exactly the kind of
change that reads as innocuous in review. This has already happened once: the
`unreachable!` form sat outside the table while a dozen production sites
accumulated under it, and none of them cost anything to write. The lints are
all present now, and this checker is what keeps them reaching every crate.

This reads manifests, never Rust source. A source scan for panic macros would
duplicate what target-aware Clippy already does better, since it cannot tell
production code from an inline `#[cfg(test)]` module; the gap worth closing is
the one Clippy structurally cannot see, which is a lint that was never
configured for a crate at all.

The check fails when

1. a `[workspace] members` entry has no `[lints] workspace = true` in its own
   manifest, so the workspace lint table — panic forms included — does not
   reach it, or
2. the `[workspace.lints.clippy]` table stops denying one of the required
   panic forms, whether by deletion or by demotion to `warn`/`allow`.

Ground truth for rule 1 is the `members` list in the root manifest, not a
directory glob, so a crate that exists on disk but is not a member is out of
scope exactly as it is for Cargo. `forbid` satisfies rule 2 wherever `deny`
does, being strictly stronger.

Run from the repository root; exits nonzero with a per-failure report naming
every manifest involved.
"""

from __future__ import annotations

import sys
import tomllib
from pathlib import Path

# The clippy lints that make a panic in production code a build failure. Each
# names a distinct way to panic, so a missing entry is a reachable panic form,
# not a redundancy. `disallowed_methods` is deliberately absent: it enforces a
# different discipline and its content is not a panic form.
REQUIRED_PANIC_LINTS = (
    "expect_used",
    "panic",
    "todo",
    "unimplemented",
    "unreachable",
    "unwrap_used",
)

GATING_LEVELS = frozenset({"deny", "forbid"})


def lint_level(configured: object) -> str | None:
    """Return the level of one Cargo lint entry, or `None` if unreadable.

    Cargo accepts both `panic = "deny"` and the table form `panic = { level =
    "deny", priority = -1 }`, and a manifest may use either without changing
    behavior, so both are read the same way.
    """
    if isinstance(configured, str):
        return configured
    if isinstance(configured, dict):
        level = configured.get("level")
        if isinstance(level, str):
            return level
    return None


def member_directories(root_manifest: dict) -> list[str]:
    """Return every `[workspace] members` entry, expanding glob entries."""
    workspace = root_manifest.get("workspace")
    members = workspace.get("members") if isinstance(workspace, dict) else None
    if not isinstance(members, list):
        return []
    directories: list[str] = []
    for entry in members:
        if not isinstance(entry, str):
            continue
        if "*" in entry:
            directories.extend(
                sorted(str(path) for path in Path().glob(entry) if path.is_dir())
            )
        else:
            directories.append(entry)
    return directories


def check_inheritance(directory: str, failures: list[str]) -> None:
    """Record a failure unless `directory`'s manifest inherits workspace lints."""
    manifest_path = Path(directory) / "Cargo.toml"
    try:
        manifest = tomllib.loads(manifest_path.read_text(encoding="utf-8"))
    except OSError:
        failures.append(f"workspace member has no readable manifest: {manifest_path}")
        return
    except tomllib.TOMLDecodeError as error:
        failures.append(f"unparseable manifest {manifest_path}: {error}")
        return
    lints = manifest.get("lints")
    inherits = isinstance(lints, dict) and lints.get("workspace") is True
    if not inherits:
        failures.append(
            f"{manifest_path} does not inherit workspace lints "
            f"(needs a [lints] section with workspace = true), so the panic "
            f"denies do not apply to it"
        )


def check_panic_lints(root_manifest: dict, failures: list[str]) -> None:
    """Record a failure for each required panic lint not denied workspace-wide."""
    workspace = root_manifest.get("workspace")
    lints = workspace.get("lints") if isinstance(workspace, dict) else None
    clippy = lints.get("clippy") if isinstance(lints, dict) else None
    if not isinstance(clippy, dict):
        failures.append(
            "Cargo.toml has no [workspace.lints.clippy] table, so no panic form is gated"
        )
        return
    for lint in REQUIRED_PANIC_LINTS:
        if lint not in clippy:
            failures.append(
                f"[workspace.lints.clippy] no longer denies clippy::{lint}, "
                f"leaving that panic form ungated"
            )
            continue
        level = lint_level(clippy[lint])
        if level not in GATING_LEVELS:
            failures.append(
                f"[workspace.lints.clippy] sets clippy::{lint} to "
                f"{level!r}, which does not fail a build (need deny or forbid)"
            )


def main() -> int:
    failures: list[str] = []
    root_path = Path("Cargo.toml")
    try:
        root_manifest = tomllib.loads(root_path.read_text(encoding="utf-8"))
    except OSError:
        print("panic-gate check FAILED:")
        print(f"  - no readable workspace manifest at {root_path}")
        return 1
    except tomllib.TOMLDecodeError as error:
        print("panic-gate check FAILED:")
        print(f"  - unparseable workspace manifest {root_path}: {error}")
        return 1

    check_panic_lints(root_manifest, failures)

    directories = member_directories(root_manifest)
    if not directories:
        failures.append("Cargo.toml lists no [workspace] members to check")
    for directory in directories:
        check_inheritance(directory, failures)

    if failures:
        print("panic-gate check FAILED:")
        for failure in failures:
            print(f"  - {failure}")
        return 1
    print(
        f"panic-gate check passed "
        f"({len(REQUIRED_PANIC_LINTS)} panic lints denied, "
        f"{len(directories)} members inherit them)"
    )
    return 0


if __name__ == "__main__":
    sys.exit(main())
