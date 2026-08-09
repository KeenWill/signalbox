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
        self.assertIn("cargo metadata", result.stdout)


class PanicGateLintTableTests(unittest.TestCase):
    def test_missing_expect_used_deny_fails(self) -> None:
        result = check_workspace(clippy_table=lint_table_without("expect_used"))

        self.assertEqual(result.returncode, 1, result.stdout)
        self.assertIn("clippy::expect_used", result.stdout)

    def test_missing_panic_deny_fails(self) -> None:
        result = check_workspace(clippy_table=lint_table_without("panic"))

        self.assertEqual(result.returncode, 1, result.stdout)
        self.assertIn("clippy::panic", result.stdout)

    def test_missing_todo_deny_fails(self) -> None:
        result = check_workspace(clippy_table=lint_table_without("todo"))

        self.assertEqual(result.returncode, 1, result.stdout)
        self.assertIn("clippy::todo", result.stdout)

    def test_missing_unimplemented_deny_fails(self) -> None:
        result = check_workspace(clippy_table=lint_table_without("unimplemented"))

        self.assertEqual(result.returncode, 1, result.stdout)
        self.assertIn("clippy::unimplemented", result.stdout)

    def test_missing_unreachable_deny_fails(self) -> None:
        result = check_workspace(clippy_table=lint_table_without("unreachable"))

        self.assertEqual(result.returncode, 1, result.stdout)
        self.assertIn("clippy::unreachable", result.stdout)

    def test_missing_unwrap_used_deny_fails(self) -> None:
        result = check_workspace(clippy_table=lint_table_without("unwrap_used"))

        self.assertEqual(result.returncode, 1, result.stdout)
        self.assertIn("clippy::unwrap_used", result.stdout)

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


if __name__ == "__main__":
    unittest.main()
