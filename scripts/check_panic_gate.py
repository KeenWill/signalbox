#!/usr/bin/env python3
"""Check that the workspace panic gate reaches every member crate.

The workspace denies six panicking convenience paths in
`[workspace.lints.clippy]` — `expect`, `panic`, `unwrap`, `todo`,
`unimplemented`, and `unreachable` — and `cargo clippy -D warnings` fails any
production site that reaches for one. That gate is what keeps those six out of
production code without a reviewer having to spot them.

It is not a proof that production code cannot panic, and reading it that way
would be worse than not having it. Indexing, arithmetic overflow, division by
zero, and an explicit `assert!` are all outside these six lints and compile
clean under them — confirmed against clippy with `--all-targets -D warnings`
on a member holding all four. `docs/style.md` scopes the contract to the named
convenience paths for exactly that reason, and this checker guards that
contract, not a broader one.

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
   so the workspace lint table — panic forms included — does not reach it,
2. the `[workspace.lints.clippy]` table stops denying one of the required
   panic forms, whether by deletion or by demotion to `warn`/`allow`, or
3. a `restriction` entry at a non-gating level outranks those denies.

Rule 3 is the one that does not read as a defect in review. Cargo emits lint
flags in ascending priority order and the last flag wins, so a table whose
panic lints are denied at priority -1 under a `restriction = { level =
"allow", priority = 0 }` is a table where nothing is denied at all: every
deny reads correctly, and clippy accepts a bare `panic!()`. Every required
form is a restriction lint, so that one group entry disarms all six at once.
`allow` opens the gate outright; `warn` demotes it to whatever `-D warnings`
happens to do, which is a contract that lives in a workflow file rather than
here. Equal priority is safe, the specific lint winning over the group, and a
group ranked below the denies never reaches them — both confirmed against
clippy rather than assumed.

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

`forbid` satisfies rule 2 wherever `deny` does, being strictly stronger, and
is exempt from rule 3 for the same reason: it denies the lint and every
attempt to override it, so no group entry can lower it whatever its priority.
Lint names are read with hyphens and underscores treated alike, because Cargo
accepts either spelling for the same lint and clippy honours both.

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

# Cargo passes lower-priority lint flags first, so a later entry wins. Every
# required panic form is a clippy `restriction` lint, which makes a
# `restriction` entry the one group entry able to turn all six back off. The
# other clippy groups were checked against clippy 1.97.1 with a `panic!()`
# member and a deny that outranked them — `all`, `pedantic`, `nursery`,
# `style`, `complexity`, `correctness`, `suspicious`, and `perf` all left the
# deny in force, because none of them contains a restriction lint.
OVERRIDING_GROUPS = ("restriction",)


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


def normalized_lints(clippy: dict, failures: list[str]) -> dict[str, object]:
    """Return the clippy table keyed by underscore lint names.

    Cargo accepts `unwrap-used` and `unwrap_used` as the same lint, and clippy
    honours either, so a table using the hyphen spelling is a correctly gated
    workspace and must not read as an ungated one. Two keys that differ only
    in spelling are a genuinely ambiguous table and fail rather than letting
    one silently win.
    """
    normalized: dict[str, object] = {}
    spellings: dict[str, str] = {}
    for key, configured in clippy.items():
        if not isinstance(key, str):
            continue
        name = key.replace("-", "_")
        if name in normalized:
            failures.append(
                f"[workspace.lints.clippy] configures {name} twice, as "
                f"{spellings[name]!r} and {key!r}; which one applies is ambiguous"
            )
            continue
        normalized[name] = configured
        spellings[name] = key
    return normalized


def lint_priority(configured: object) -> int | None:
    """Return the priority of one Cargo lint entry, or `None` if unreadable.

    A bare `panic = "deny"` carries Cargo's default priority of zero. `bool`
    is rejected explicitly because Python counts it as an `int`, and a
    `priority = true` should read as malformed rather than as priority one.
    """
    if isinstance(configured, str):
        return 0
    if isinstance(configured, dict):
        priority = configured.get("priority", 0)
        if isinstance(priority, bool):
            return None
        if isinstance(priority, int):
            return priority
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
    """Record a failure unless the member inherits or repeats every panic lint."""
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
    if inherits:
        return
    clippy = lints.get("clippy") if isinstance(lints, dict) else None
    if isinstance(clippy, dict):
        configured = normalized_lints(clippy, failures)
        explicitly_gated = all(
            lint in configured and lint_level(configured[lint]) in GATING_LEVELS
            for lint in REQUIRED_PANIC_LINTS
        )
        if explicitly_gated:
            return
    failures.append(
        f"{shown} neither inherits workspace lints nor explicitly denies "
        f"every required panic lint"
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
    configured = normalized_lints(clippy, failures)
    denied_at: dict[str, int] = {}
    for lint in REQUIRED_PANIC_LINTS:
        if lint not in configured:
            failures.append(
                f"[workspace.lints.clippy] no longer denies clippy::{lint}, "
                f"leaving that panic form ungated"
            )
            continue
        level = lint_level(configured[lint])
        if level not in GATING_LEVELS:
            failures.append(
                f"[workspace.lints.clippy] sets clippy::{lint} to "
                f"{level!r}, which does not fail a build (need deny or forbid)"
            )
            continue
        if level == "forbid":
            # `-F` denies the lint and every attempt to override it, so no
            # later group entry can lower it and its priority cannot matter.
            continue
        priority = lint_priority(configured[lint])
        if priority is None:
            failures.append(
                f"[workspace.lints.clippy] gives clippy::{lint} an unreadable "
                f"priority, so its position against group entries is undecidable"
            )
            continue
        denied_at[lint] = priority

    check_group_overrides(configured, denied_at, failures)


def check_group_overrides(
    clippy: dict, denied_at: dict[str, int], failures: list[str]
) -> None:
    """Record a failure for each group entry that outranks a panic deny.

    Cargo emits lint flags in ascending priority order and the last flag wins,
    so a group holding these lints at a non-gating level beats every deny it
    outranks. Equal priority is safe — the specific lint wins over the group —
    and a group ranked below the denies never reaches them.

    Only `deny` reaches this comparison. `forbid` denies the lint and every
    attempt to override it, so a forbidden form survives an outranking group
    and never belongs in `denied_at`; treating it like a deny would fail a
    table strictly stronger than the one this checker demands.
    """
    for group in OVERRIDING_GROUPS:
        if group not in clippy:
            continue
        level = lint_level(clippy[group])
        if level in GATING_LEVELS:
            continue
        priority = lint_priority(clippy[group])
        if priority is None:
            failures.append(
                f"[workspace.lints.clippy] gives clippy::{group} an unreadable "
                f"priority, so it cannot be shown to leave the panic denies standing"
            )
            continue
        outranked = sorted(
            lint for lint, at in denied_at.items() if priority > at
        )
        if outranked:
            listing = ", ".join(f"clippy::{lint}" for lint in outranked)
            failures.append(
                f"[workspace.lints.clippy] sets clippy::{group} to {level!r} at "
                f"priority {priority}, which Cargo emits after — and so overrides "
                f"— the denies on {listing}; those panic forms are not gated "
                f"despite reading as denied. Rank the group below them."
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
        f"{len(manifests)} members covered)"
    )
    return 0


if __name__ == "__main__":
    sys.exit(main())
