#!/usr/bin/env python3
"""Prove check_panic_gate.py actually fails on an escaped crate or a lost lint.

A checker that cannot be shown to fail is the false-confidence trap this
repository documents, and this checker is especially exposed to it: the tree it
guards is clean, so a version that returned zero unconditionally would look
identical in CI to one that works. Each rule therefore gets a positive and a
negative case.

The membership cases are the ones worth reading closely, because the two ways
of guessing at membership fail in opposite directions and a fixture proves
which side the checker lands on. A crate reachable only as a path dependency
is a member Cargo lints, so missing it would be the silently-exempt crate this
gate exists to catch; a crate matched by a `members` glob but named in
`exclude` is not a member, so flagging it would fail CI over a contract that
does not apply. Both are asserted against a real `cargo metadata` resolution
rather than a reimplementation of it. A workspace Cargo cannot resolve is
asserted to fail the gate and to carry Cargo's own reason, since a membership
check that cannot see the crates it covers must never report success.

Each case runs the checker as a subprocess against a synthetic workspace in a
temporary working directory, so its root-relative discovery sees only the
fixture. Every fixture package carries a `src/lib.rs`, because Cargo refuses
to describe a package with no target.
"""

from __future__ import annotations

import subprocess
import sys
import tempfile
import unittest
from pathlib import Path

CHECKER = Path(__file__).resolve().parent / "check_panic_gate.py"

REQUIRED_PANIC_LINTS = (
    "expect_used",
    "panic",
    "todo",
    "unimplemented",
    "unreachable",
    "unwrap_used",
)

INHERITING_MANIFEST = """[package]
name = "{name}"
version = "0.0.0"
edition = "2021"

[lints]
workspace = true
"""

NON_INHERITING_MANIFEST = """[package]
name = "{name}"
version = "0.0.0"
edition = "2021"
"""

# Cargo rejects this manifest outright — `workspace` cannot be false — so it
# stands in for any member manifest Cargo refuses to load, not for a crate
# opting out. Opting out is spelled by omitting the stanza entirely.
DECLINING_MANIFEST = """[package]
name = "{name}"
version = "0.0.0"
edition = "2021"

[lints]
workspace = false
"""

DEPENDING_MANIFEST = """[package]
name = "{name}"
version = "0.0.0"
edition = "2021"

[lints]
workspace = true

[dependencies]
implicit = {{ path = "../implicit" }}
"""


def deny_all_lints() -> str:
    """Return a clippy lint table denying every required panic form."""
    return "\n".join(f'{lint} = "deny"' for lint in REQUIRED_PANIC_LINTS)


def lint_table_without(omitted: str) -> str:
    """Return a clippy lint table denying every required form but `omitted`."""
    return "\n".join(
        f'{lint} = "deny"' for lint in REQUIRED_PANIC_LINTS if lint != omitted
    )


def lint_table_in_table_form() -> str:
    """Return the required denies spelled as Cargo's `{ level = ... }` tables."""
    return "\n".join(
        f'{lint} = {{ level = "deny", priority = -1 }}'
        for lint in REQUIRED_PANIC_LINTS
    )


def lint_table_forbidding_all() -> str:
    """Return the required panic forms at `forbid`, stricter than `deny`."""
    return "\n".join(f'{lint} = "forbid"' for lint in REQUIRED_PANIC_LINTS)


def lint_table_with_panic_warned() -> str:
    """Return the required denies with `panic` demoted to a non-gating level."""
    return deny_all_lints().replace('panic = "deny"', 'panic = "warn"')


def lint_table_with_group(
    *,
    group: str,
    lint_level: str,
    lint_priority: int,
    group_level: str,
    group_priority: str,
) -> str:
    """Return the required panic lints plus one group entry.

    Every knob is keyword-only because all five vary across the group-override
    cases and each one decides whether a case passes. `group_priority` is
    spelled as raw TOML so a case can hand the checker an unreadable value
    without the helper having to model what is readable.
    """
    lints = "\n".join(
        f'{lint} = {{ level = "{lint_level}", priority = {lint_priority} }}'
        for lint in REQUIRED_PANIC_LINTS
    )
    entry = f'{group} = {{ level = "{group_level}", priority = {group_priority} }}'
    return f"{lints}\n{entry}"


def lint_table_spelled_with_hyphens() -> str:
    """Return the required denies using Cargo's hyphenated lint spelling."""
    return "\n".join(
        f'{lint.replace("_", "-")} = "deny"' for lint in REQUIRED_PANIC_LINTS
    )


def lint_table_spelling_one_lint_twice(lint: str) -> str:
    """Return the required denies plus `lint` repeated in hyphen spelling."""
    return f'{deny_all_lints()}\n{lint.replace("_", "-")} = "deny"'


def lint_table_with_unreadable_lint_priority() -> str:
    """Return a table whose `panic` deny carries a non-integer priority."""
    return deny_all_lints().replace(
        'panic = "deny"', 'panic = { level = "deny", priority = true }'
    )


def workspace_manifest(
    members: tuple[str, ...],
    exclude: tuple[str, ...] = (),
    clippy_table: str | None = None,
) -> str:
    """Return a virtual workspace manifest listing `members` and `exclude`."""
    if clippy_table is None:
        clippy_table = deny_all_lints()
    member_lines = "".join(f'    "{entry}",\n' for entry in members)
    exclude_lines = "".join(f'    "{entry}",\n' for entry in exclude)
    excluded = f"exclude = [\n{exclude_lines}]\n" if exclude else ""
    return (
        f"[workspace]\nmembers = [\n{member_lines}]\n{excluded}"
        f'resolver = "2"\n\n[workspace.lints.clippy]\n{clippy_table}\n'
    )


def run_checker(root: Path) -> subprocess.CompletedProcess:
    """Run the checker with `root` as its working directory."""
    return subprocess.run(
        [sys.executable, str(CHECKER)],
        cwd=root,
        capture_output=True,
        text=True,
    )


def check_workspace(
    packages: dict[str, str] | None = None,
    members: tuple[str, ...] = ("crates/synthetic",),
    exclude: tuple[str, ...] = (),
    clippy_table: str | None = None,
) -> subprocess.CompletedProcess:
    """Run the checker over a synthetic Cargo workspace, outside test bodies.

    Writes the root manifest and every package in `packages` — each mapping a
    directory to a manifest template taking its package name — into a
    temporary working directory, then runs the checker there.
    """
    if packages is None:
        packages = {"crates/synthetic": INHERITING_MANIFEST}
    with tempfile.TemporaryDirectory() as tmp:
        root = Path(tmp)
        (root / "Cargo.toml").write_text(
            workspace_manifest(members, exclude, clippy_table), encoding="utf-8"
        )
        for directory, template in packages.items():
            package = root / directory
            (package / "src").mkdir(parents=True)
            name = Path(directory).name
            (package / "Cargo.toml").write_text(
                template.format(name=name), encoding="utf-8"
            )
            (package / "src" / "lib.rs").write_text(
                "pub fn f() {}\n", encoding="utf-8"
            )
        return run_checker(root)


class PanicGateMembershipTests(unittest.TestCase):
    def test_inheriting_member_passes(self) -> None:
        result = check_workspace()

        self.assertEqual(result.returncode, 0, result.stdout)

    def test_member_without_lints_stanza_fails_naming_its_manifest(self) -> None:
        result = check_workspace(
            packages={
                "crates/inheriting": INHERITING_MANIFEST,
                "crates/escaped": NON_INHERITING_MANIFEST,
            },
            members=("crates/inheriting", "crates/escaped"),
        )

        self.assertEqual(result.returncode, 1, result.stdout)
        self.assertIn("crates/escaped/Cargo.toml", result.stdout)
        self.assertNotIn("crates/inheriting/Cargo.toml", result.stdout)

    def test_manifest_cargo_refuses_fails_the_gate_with_cargos_reason(self) -> None:
        result = check_workspace(
            packages={"crates/declining": DECLINING_MANIFEST},
            members=("crates/declining",),
        )

        self.assertEqual(result.returncode, 1, result.stdout)
        self.assertIn("`workspace` cannot be false", result.stdout)

    def test_unlisted_path_dependency_is_checked_as_an_implicit_member(self) -> None:
        result = check_workspace(
            packages={
                "listed": DEPENDING_MANIFEST,
                "implicit": NON_INHERITING_MANIFEST,
            },
            members=("listed",),
        )

        self.assertEqual(result.returncode, 1, result.stdout)
        self.assertIn("implicit/Cargo.toml", result.stdout)

    def test_excluded_crate_is_not_checked(self) -> None:
        result = check_workspace(
            packages={
                "crates/kept": INHERITING_MANIFEST,
                "crates/standalone": NON_INHERITING_MANIFEST,
            },
            members=("crates/*",),
            exclude=("crates/standalone",),
        )

        self.assertEqual(result.returncode, 0, result.stdout)
        self.assertNotIn("crates/standalone", result.stdout)

    def test_glob_matched_member_without_lints_fails(self) -> None:
        result = check_workspace(
            packages={"crates/escaped": NON_INHERITING_MANIFEST},
            members=("crates/*",),
        )

        self.assertEqual(result.returncode, 1, result.stdout)
        self.assertIn("crates/escaped/Cargo.toml", result.stdout)

    def test_unresolvable_workspace_fails_rather_than_passing(self) -> None:
        with tempfile.TemporaryDirectory() as tmp:
            root = Path(tmp)
            (root / "Cargo.toml").write_text(
                workspace_manifest(("crates/absent",)), encoding="utf-8"
            )

            result = run_checker(root)

        self.assertEqual(result.returncode, 1, result.stdout)
        # Cargo's own wording rather than the checker's prefix, so dropping
        # the captured diagnostic in favour of a generic message breaks this.
        self.assertIn("failed to load manifest", result.stdout)


class PanicGateLintTableTests(unittest.TestCase):
    def test_missing_expect_used_deny_fails(self) -> None:
        omitted = "expect_used"

        result = check_workspace(clippy_table=lint_table_without(omitted))

        self.assertEqual(result.returncode, 1, result.stdout)
        self.assertIn(f"clippy::{omitted}", result.stdout)

    def test_missing_panic_deny_fails(self) -> None:
        omitted = "panic"

        result = check_workspace(clippy_table=lint_table_without(omitted))

        self.assertEqual(result.returncode, 1, result.stdout)
        self.assertIn(f"clippy::{omitted}", result.stdout)

    def test_missing_todo_deny_fails(self) -> None:
        omitted = "todo"

        result = check_workspace(clippy_table=lint_table_without(omitted))

        self.assertEqual(result.returncode, 1, result.stdout)
        self.assertIn(f"clippy::{omitted}", result.stdout)

    def test_missing_unimplemented_deny_fails(self) -> None:
        omitted = "unimplemented"

        result = check_workspace(clippy_table=lint_table_without(omitted))

        self.assertEqual(result.returncode, 1, result.stdout)
        self.assertIn(f"clippy::{omitted}", result.stdout)

    def test_missing_unreachable_deny_fails(self) -> None:
        omitted = "unreachable"

        result = check_workspace(clippy_table=lint_table_without(omitted))

        self.assertEqual(result.returncode, 1, result.stdout)
        self.assertIn(f"clippy::{omitted}", result.stdout)

    def test_missing_unwrap_used_deny_fails(self) -> None:
        omitted = "unwrap_used"

        result = check_workspace(clippy_table=lint_table_without(omitted))

        self.assertEqual(result.returncode, 1, result.stdout)
        self.assertIn(f"clippy::{omitted}", result.stdout)

    def test_hyphenated_lint_spelling_is_accepted(self) -> None:
        result = check_workspace(clippy_table=lint_table_spelled_with_hyphens())

        self.assertEqual(result.returncode, 0, result.stdout)

    def test_one_lint_spelled_both_ways_fails_as_ambiguous(self) -> None:
        repeated = "unwrap_used"

        result = check_workspace(
            clippy_table=lint_table_spelling_one_lint_twice(repeated)
        )

        self.assertEqual(result.returncode, 1, result.stdout)
        self.assertIn(repeated, result.stdout)

    def test_panic_lint_demoted_to_warn_fails(self) -> None:
        result = check_workspace(clippy_table=lint_table_with_panic_warned())

        self.assertEqual(result.returncode, 1, result.stdout)
        self.assertIn("clippy::panic", result.stdout)

    def test_table_form_level_is_accepted(self) -> None:
        result = check_workspace(clippy_table=lint_table_in_table_form())

        self.assertEqual(result.returncode, 0, result.stdout)

    def test_forbid_satisfies_the_gate(self) -> None:
        result = check_workspace(clippy_table=lint_table_forbidding_all())

        self.assertEqual(result.returncode, 0, result.stdout)

    def test_missing_clippy_table_fails(self) -> None:
        with tempfile.TemporaryDirectory() as tmp:
            root = Path(tmp)
            (root / "Cargo.toml").write_text(
                '[workspace]\nmembers = [\n    "crates/synthetic",\n]\n'
                'resolver = "2"\n',
                encoding="utf-8",
            )
            package = root / "crates" / "synthetic"
            (package / "src").mkdir(parents=True)
            (package / "Cargo.toml").write_text(
                INHERITING_MANIFEST.format(name="synthetic"), encoding="utf-8"
            )
            (package / "src" / "lib.rs").write_text(
                "pub fn f() {}\n", encoding="utf-8"
            )

            result = run_checker(root)

        self.assertEqual(result.returncode, 1, result.stdout)
        self.assertIn("[workspace.lints.clippy]", result.stdout)


class PanicGateGroupOverrideTests(unittest.TestCase):
    """Cargo emits lint flags in ascending priority order and the last wins.

    Every required form is a clippy `restriction` lint, so one `restriction`
    entry ranked above the denies turns all six off while each deny still
    reads correctly. The passing cases here are the ones that keep this from
    being a blunt ban on ever naming the group — including the forbidden
    table, which is stricter than what the gate demands and must not be
    mistaken for an overridden one.
    """

    def test_restriction_allow_outranking_denies_fails(self) -> None:
        group = "restriction"

        result = check_workspace(
            clippy_table=lint_table_with_group(
                group=group,
                lint_level="deny",
                lint_priority=-1,
                group_level="allow",
                group_priority="0",
            )
        )

        self.assertEqual(result.returncode, 1, result.stdout)
        self.assertIn(f"clippy::{group}", result.stdout)
        self.assertIn("clippy::panic", result.stdout)

    def test_restriction_warn_outranking_denies_fails(self) -> None:
        group = "restriction"

        result = check_workspace(
            clippy_table=lint_table_with_group(
                group=group,
                lint_level="deny",
                lint_priority=0,
                group_level="warn",
                group_priority="1",
            )
        )

        self.assertEqual(result.returncode, 1, result.stdout)
        self.assertIn(f"clippy::{group}", result.stdout)

    def test_restriction_allow_below_denies_passes(self) -> None:
        result = check_workspace(
            clippy_table=lint_table_with_group(
                group="restriction",
                lint_level="deny",
                lint_priority=0,
                group_level="allow",
                group_priority="-1",
            )
        )

        self.assertEqual(result.returncode, 0, result.stdout)

    def test_restriction_allow_at_equal_priority_passes(self) -> None:
        result = check_workspace(
            clippy_table=lint_table_with_group(
                group="restriction",
                lint_level="deny",
                lint_priority=0,
                group_level="allow",
                group_priority="0",
            )
        )

        self.assertEqual(result.returncode, 0, result.stdout)

    def test_restriction_denied_above_the_lints_passes(self) -> None:
        result = check_workspace(
            clippy_table=lint_table_with_group(
                group="restriction",
                lint_level="deny",
                lint_priority=0,
                group_level="deny",
                group_priority="1",
            )
        )

        self.assertEqual(result.returncode, 0, result.stdout)

    def test_forbidden_lints_survive_an_outranking_allow(self) -> None:
        result = check_workspace(
            clippy_table=lint_table_with_group(
                group="restriction",
                lint_level="forbid",
                lint_priority=-1,
                group_level="allow",
                group_priority="0",
            )
        )

        self.assertEqual(result.returncode, 0, result.stdout)

    def test_unreadable_group_priority_fails(self) -> None:
        result = check_workspace(
            clippy_table=lint_table_with_group(
                group="restriction",
                lint_level="deny",
                lint_priority=0,
                group_level="allow",
                group_priority="true",
            )
        )

        self.assertEqual(result.returncode, 1, result.stdout)
        self.assertIn("unreadable priority", result.stdout)

    def test_unreadable_lint_priority_fails(self) -> None:
        result = check_workspace(
            clippy_table=lint_table_with_unreadable_lint_priority()
        )

        self.assertEqual(result.returncode, 1, result.stdout)
        self.assertIn("unreadable priority", result.stdout)


if __name__ == "__main__":
    unittest.main()
