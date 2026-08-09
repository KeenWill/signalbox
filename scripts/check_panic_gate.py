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

1. a workspace member has no `[lints] workspace = true` in its own manifest,
   so the workspace lint table — panic forms included — does not reach it, or
2. the `[workspace.lints.clippy]` table stops denying one of the required
   panic forms, whether by deletion or by demotion to `warn`/`allow`.

Membership is whatever `cargo metadata` resolves, never a reading of the
`members` list and never a directory glob. Those two disagree with Cargo in
opposite directions, and only the resolved list is right about both. A crate
under the workspace root that a member depends on by path is a member even
though nothing lists it, which is precisely the silently-exempt crate this
checker exists to catch; a crate matched by a `members` glob but named in
`exclude` is not a member, and flagging it would fail CI over a contract that
does not apply to it. Asking Cargo costs one `--no-deps` invocation, reads no
dependency graph, and cannot drift from what `cargo clippy --workspace`
actually lints.

`forbid` satisfies rule 2 wherever `deny` does, being strictly stronger.

Run from the repository root; exits nonzero with a per-failure report naming
every manifest involved. A workspace whose membership cannot be resolved is a
failure, never a pass: a gate that cannot see the crates it covers reports
nothing useful.
"""

from __future__ import annotations

import json
import subprocess
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


class MembershipError(Exception):
    """Raised when Cargo cannot tell us which packages are workspace members."""


def member_manifests() -> list[Path]:
    """Return the manifest of every package Cargo resolves as a member.

    `--no-deps` keeps this to the workspace itself: Cargo expands `members`
    globs, applies `exclude`, and adds the path dependencies it treats as
    implicit members, without resolving or downloading a dependency graph.
    """
    try:
        completed = subprocess.run(
            ["cargo", "metadata", "--no-deps", "--format-version", "1"],
            capture_output=True,
            text=True,
            check=False,
        )
    except OSError as error:
        raise MembershipError(f"could not run cargo metadata: {error}") from error
    if completed.returncode != 0:
        # Every diagnostic line, not just the last: Cargo reports the cause
        # first and the workspace that pulled the manifest in last, so a tail
        # would name the workspace and drop the reason.
        detail = " / ".join(
            line.strip() for line in completed.stderr.splitlines() if line.strip()
        )
        raise MembershipError(
            f"cargo metadata failed: {detail or 'no diagnostic'}"
        )
    try:
        metadata = json.loads(completed.stdout)
    except json.JSONDecodeError as error:
        raise MembershipError(
            f"cargo metadata emitted unreadable JSON: {error}"
        ) from error
    manifests_by_id = {
        package["id"]: package["manifest_path"]
        for package in metadata.get("packages", [])
        if "id" in package and "manifest_path" in package
    }
    members = metadata.get("workspace_members", [])
    missing = [identifier for identifier in members if identifier not in manifests_by_id]
    if missing:
        raise MembershipError(
            f"cargo metadata named {len(missing)} member(s) it did not describe"
        )
    return sorted(Path(manifests_by_id[identifier]) for identifier in members)


def display_path(manifest_path: Path) -> str:
    """Return `manifest_path` relative to the working directory when possible."""
    try:
        return str(manifest_path.resolve().relative_to(Path.cwd().resolve()))
    except ValueError:
        return str(manifest_path)


def check_inheritance(manifest_path: Path, failures: list[str]) -> None:
    """Record a failure unless the member's manifest inherits workspace lints."""
    shown = display_path(manifest_path)
    try:
        manifest = tomllib.loads(manifest_path.read_text(encoding="utf-8"))
    except OSError:
        failures.append(f"workspace member has no readable manifest: {shown}")
        return
    except tomllib.TOMLDecodeError as error:
        failures.append(f"unparseable manifest {shown}: {error}")
        return
    lints = manifest.get("lints")
    inherits = isinstance(lints, dict) and lints.get("workspace") is True
    if not inherits:
        failures.append(
            f"{shown} does not inherit workspace lints "
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

    try:
        manifests = member_manifests()
    except MembershipError as error:
        print("panic-gate check FAILED:")
        for failure in failures:
            print(f"  - {failure}")
        print(f"  - {error}")
        return 1

    if not manifests:
        failures.append("cargo metadata resolved no workspace members to check")
    for manifest_path in manifests:
        check_inheritance(manifest_path, failures)

    if failures:
        print("panic-gate check FAILED:")
        for failure in failures:
            print(f"  - {failure}")
        return 1
    print(
        f"panic-gate check passed "
        f"({len(REQUIRED_PANIC_LINTS)} panic lints denied, "
        f"{len(manifests)} members inherit them)"
    )
    return 0


if __name__ == "__main__":
    sys.exit(main())
