#!/usr/bin/env python3
"""Prove each ownership-seam reach-around rule accepts and rejects fixtures."""

from __future__ import annotations

import subprocess
import sys
import tempfile
import unittest
from pathlib import Path

CHECKER = Path(__file__).resolve().parent / "check_ownership_seam.py"


class OwnershipSeamCheckerTests(unittest.TestCase):
    def run_checker(
        self,
        manifest: str,
        source: str,
        core: str = "",
        module_sql: str | None = None,
        core_sql: str | None = None,
        workspace_manifest: str | None = None,
    ) -> subprocess.CompletedProcess[str]:
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary)
            module = root / "crates" / "modules" / "example"
            (module / "src").mkdir(parents=True)
            (root / "crates" / "ownership-seam").mkdir()
            (root / "crates" / "persistence" / "src").mkdir(parents=True)
            (module / "Cargo.toml").write_text(manifest, encoding="utf-8")
            if workspace_manifest is not None:
                (root / "Cargo.toml").write_text(workspace_manifest, encoding="utf-8")
            (module / "src" / "lib.rs").write_text(source, encoding="utf-8")
            if module_sql is not None:
                (module / "src" / "query.sql").write_text(module_sql, encoding="utf-8")
            (root / "crates" / "persistence" / "src" / "lib.rs").write_text(
                core, encoding="utf-8"
            )
            if core_sql is not None:
                (root / "crates" / "persistence" / "src" / "query.sql").write_text(
                    core_sql, encoding="utf-8"
                )
            return subprocess.run(
                [sys.executable, str(CHECKER)],
                cwd=root,
                check=False,
                capture_output=True,
                text=True,
            )

    def test_seam_and_external_dependencies_are_admitted(self) -> None:
        result = self.run_checker(
            """[dependencies]
signalbox-ownership-seam = { path = "../../ownership-seam" }
serde = "1"
""",
            "pub fn reduce() {}\n",
        )
        self.assertEqual(result.returncode, 0, result.stderr)

    def test_other_signalbox_dependency_is_rejected(self) -> None:
        result = self.run_checker(
            """[dependencies]
signalbox-persistence = { path = "../../persistence" }
""",
            "",
        )
        self.assertNotEqual(result.returncode, 0)
        self.assertIn("forbidden Signalbox dependency", result.stderr)

    def test_target_specific_signalbox_dependency_is_rejected(self) -> None:
        result = self.run_checker(
            """[target.'cfg(unix)'.dependencies]
signalbox-persistence = { path = "../../persistence" }
""",
            "",
        )
        self.assertNotEqual(result.returncode, 0)
        self.assertIn("forbidden Signalbox dependency", result.stderr)

    def test_renamed_signalbox_dependency_is_rejected(self) -> None:
        result = self.run_checker(
            """[dependencies]
core_store = { package = "signalbox-persistence", git = "https://example.invalid/repo" }
""",
            "",
        )
        self.assertNotEqual(result.returncode, 0)
        self.assertIn("forbidden Signalbox dependency", result.stderr)

    def test_renamed_inherited_signalbox_dependency_is_rejected(self) -> None:
        result = self.run_checker(
            """[dependencies]
core_store.workspace = true
""",
            "",
            workspace_manifest="""[workspace]
members = ["crates/modules/example"]

[workspace.dependencies]
core_store = { package = "signalbox-persistence", path = "crates/persistence" }
""",
        )
        self.assertNotEqual(result.returncode, 0)
        self.assertIn("forbidden Signalbox dependency", result.stderr)

    def test_direct_import_and_public_join_are_rejected(self) -> None:
        result = self.run_checker(
            "[dependencies]\n",
            'use signalbox_persistence::repo_watch;\nconst SQL: &str = "SELECT * FROM public.session";\n',
        )
        self.assertNotEqual(result.returncode, 0)
        self.assertIn("forbidden import", result.stderr)
        self.assertIn("module SQL names a public relation", result.stderr)

    def test_external_module_sql_public_join_is_rejected(self) -> None:
        result = self.run_checker(
            "[dependencies]\n",
            'const QUERY: &str = include_str!("query.sql");\n',
            module_sql="SELECT * FROM public.session;\n",
        )
        self.assertNotEqual(result.returncode, 0)
        self.assertIn("module SQL names a public relation", result.stderr)

    def test_core_module_join_is_rejected(self) -> None:
        result = self.run_checker(
            "[dependencies]\n",
            "",
            'const SQL: &str = "SELECT * FROM mod_repo_watch.frontier";\n',
        )
        self.assertNotEqual(result.returncode, 0)
        self.assertIn("core SQL names a module relation", result.stderr)

    def test_external_core_sql_module_join_is_rejected(self) -> None:
        result = self.run_checker(
            "[dependencies]\n",
            "",
            'const QUERY: &str = include_str!("query.sql");\n',
            core_sql="SELECT * FROM mod_repo_watch.frontier;\n",
        )
        self.assertNotEqual(result.returncode, 0)
        self.assertIn("core SQL names a module relation", result.stderr)


if __name__ == "__main__":
    unittest.main()
