#!/usr/bin/env python3
"""Prove check_panic_gate.py actually fails on an escaped crate or a lost lint.

A checker that cannot be shown to fail is the false-confidence trap this
repository documents, and this checker is especially exposed to it: the tree
it guards is clean, so a version that returned zero unconditionally would look
identical in CI to one that works. Each rule therefore gets a positive and a
negative case — a member that inherits passes, one that omits the stanza fails
naming its manifest, one that sets `workspace = false` fails rather than
counting as an opt-in, a deleted panic lint fails naming the lint, and a lint
demoted to `warn` fails even though it is still present. The accepted spelling
variants get positive cases of their own, because a checker that rejected the
table form or `forbid` would be a false alarm on a legitimate manifest.

Each case runs the checker as a subprocess against a synthetic workspace in a
temporary working directory, so its root-relative discovery sees only the
fixture.
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
name = "synthetic"
version = "0.0.0"

[lints]
workspace = true
"""

NON_INHERITING_MANIFEST = """[package]
name = "synthetic"
version = "0.0.0"
"""

DECLINING_MANIFEST = """[package]
name = "synthetic"
version = "0.0.0"

[lints]
workspace = false
"""


def deny_all_lints() -> str:
    """Return a clippy lint table denying every required panic form."""
    return "\n".join(f'{lint} = "deny"' for lint in REQUIRED_PANIC_LINTS)


def check_workspace(
    members: dict[str, str] | None = None,
    clippy_table: str | None = None,
) -> subprocess.CompletedProcess:
    """Run the checker over a synthetic workspace, outside test bodies.

    Writes a root manifest listing exactly `members` by directory with the
    given `[workspace.lints.clippy]` body, plus each member's own manifest,
    into a temporary working directory, then runs the checker there.
    """
    if members is None:
        members = {"crates/synthetic": INHERITING_MANIFEST}
    if clippy_table is None:
        clippy_table = deny_all_lints()
    with tempfile.TemporaryDirectory() as tmp:
        root = Path(tmp)
        listing = "".join(f'    "{directory}",\n' for directory in members)
        (root / "Cargo.toml").write_text(
            f"[workspace]\nmembers = [\n{listing}]\n\n"
            f"[workspace.lints.clippy]\n{clippy_table}\n",
            encoding="utf-8",
        )
        for directory, manifest in members.items():
            package = root / directory
            package.mkdir(parents=True)
            (package / "Cargo.toml").write_text(manifest, encoding="utf-8")
        return subprocess.run(
            [sys.executable, str(CHECKER)],
            cwd=root,
            capture_output=True,
            text=True,
        )


class PanicGateInheritanceTests(unittest.TestCase):
    def test_inheriting_member_passes(self) -> None:
        result = check_workspace()

        self.assertEqual(result.returncode, 0, result.stdout)

    def test_member_without_lints_stanza_fails_naming_its_manifest(self) -> None:
        result = check_workspace(
            {
                "crates/inheriting": INHERITING_MANIFEST,
                "crates/escaped": NON_INHERITING_MANIFEST,
            }
        )

        self.assertEqual(result.returncode, 1, result.stdout)
        self.assertIn("crates/escaped/Cargo.toml", result.stdout)
        self.assertNotIn("crates/inheriting/Cargo.toml", result.stdout)

    def test_member_declining_inheritance_fails(self) -> None:
        result = check_workspace({"crates/declining": DECLINING_MANIFEST})

        self.assertEqual(result.returncode, 1, result.stdout)
        self.assertIn("crates/declining/Cargo.toml", result.stdout)

    def test_member_with_missing_manifest_fails(self) -> None:
        with tempfile.TemporaryDirectory() as tmp:
            root = Path(tmp)
            (root / "Cargo.toml").write_text(
                '[workspace]\nmembers = [\n    "crates/absent",\n]\n\n'
                f"[workspace.lints.clippy]\n{deny_all_lints()}\n",
                encoding="utf-8",
            )
            result = subprocess.run(
                [sys.executable, str(CHECKER)],
                cwd=root,
                capture_output=True,
                text=True,
            )

        self.assertEqual(result.returncode, 1, result.stdout)
        self.assertIn("crates/absent/Cargo.toml", result.stdout)

    def test_glob_member_entry_is_expanded(self) -> None:
        with tempfile.TemporaryDirectory() as tmp:
            root = Path(tmp)
            (root / "Cargo.toml").write_text(
                '[workspace]\nmembers = [\n    "crates/*",\n]\n\n'
                f"[workspace.lints.clippy]\n{deny_all_lints()}\n",
                encoding="utf-8",
            )
            escaped = root / "crates" / "escaped"
            escaped.mkdir(parents=True)
            (escaped / "Cargo.toml").write_text(
                NON_INHERITING_MANIFEST, encoding="utf-8"
            )
            result = subprocess.run(
                [sys.executable, str(CHECKER)],
                cwd=root,
                capture_output=True,
                text=True,
            )

        self.assertEqual(result.returncode, 1, result.stdout)
        self.assertIn("crates/escaped/Cargo.toml", result.stdout)


class PanicGateLintTableTests(unittest.TestCase):
    def test_deleted_panic_lint_fails_naming_the_lint(self) -> None:
        without_unreachable = "\n".join(
            f'{lint} = "deny"'
            for lint in REQUIRED_PANIC_LINTS
            if lint != "unreachable"
        )

        result = check_workspace(clippy_table=without_unreachable)

        self.assertEqual(result.returncode, 1, result.stdout)
        self.assertIn("clippy::unreachable", result.stdout)

    def test_panic_lint_demoted_to_warn_fails(self) -> None:
        demoted = deny_all_lints().replace('panic = "deny"', 'panic = "warn"')

        result = check_workspace(clippy_table=demoted)

        self.assertEqual(result.returncode, 1, result.stdout)
        self.assertIn("clippy::panic", result.stdout)

    def test_every_required_lint_is_checked_individually(self) -> None:
        for lint in REQUIRED_PANIC_LINTS:
            with self.subTest(lint=lint):
                without_one = "\n".join(
                    f'{present} = "deny"'
                    for present in REQUIRED_PANIC_LINTS
                    if present != lint
                )

                result = check_workspace(clippy_table=without_one)

                self.assertEqual(result.returncode, 1, result.stdout)
                self.assertIn(f"clippy::{lint}", result.stdout)

    def test_table_form_level_is_accepted(self) -> None:
        table_form = "\n".join(
            f'{lint} = {{ level = "deny", priority = -1 }}'
            for lint in REQUIRED_PANIC_LINTS
        )

        result = check_workspace(clippy_table=table_form)

        self.assertEqual(result.returncode, 0, result.stdout)

    def test_forbid_satisfies_the_gate(self) -> None:
        forbidding = "\n".join(
            f'{lint} = "forbid"' for lint in REQUIRED_PANIC_LINTS
        )

        result = check_workspace(clippy_table=forbidding)

        self.assertEqual(result.returncode, 0, result.stdout)

    def test_missing_clippy_table_fails(self) -> None:
        with tempfile.TemporaryDirectory() as tmp:
            root = Path(tmp)
            (root / "Cargo.toml").write_text(
                '[workspace]\nmembers = [\n    "crates/synthetic",\n]\n',
                encoding="utf-8",
            )
            package = root / "crates" / "synthetic"
            package.mkdir(parents=True)
            (package / "Cargo.toml").write_text(
                INHERITING_MANIFEST, encoding="utf-8"
            )
            result = subprocess.run(
                [sys.executable, str(CHECKER)],
                cwd=root,
                capture_output=True,
                text=True,
            )

        self.assertEqual(result.returncode, 1, result.stdout)
        self.assertIn("[workspace.lints.clippy]", result.stdout)


if __name__ == "__main__":
    unittest.main()
