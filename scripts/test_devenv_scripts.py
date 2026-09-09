#!/usr/bin/env python3
"""Exercise the scripts produced by `devenv shell` with disposable fixtures."""

import json
import os
from pathlib import Path
import shutil
import stat
import subprocess
import tempfile
import tomllib
import unittest


class DevenvScriptsTests(unittest.TestCase):
    def test_seeded_example_has_private_files_for_every_file_profile(self):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            primary = root / "anthropic-api-key"
            subprocess.run([
                "signalbox-dev-credential", "", str(root / "absent-home-key"),
                str(primary),
            ], check=True, capture_output=True)
            catalog = root / "catalog.toml"
            shutil.copyfile(Path(__file__).resolve().parents[1] / "config/signalboxd.example.toml", catalog)
            subprocess.run([
                "signalbox-seed-config", str(catalog), str(primary),
                str(root / "staging"), str(root / "blobs"), "true",
            ], check=True)
            document = tomllib.loads(catalog.read_text())
            profiles = [entry for entry in document["credential_profiles"] if entry["delivery"] == "file"]
            paths = [Path(entry["file"]) for entry in profiles]
            self.assertEqual(len(paths), len(set(paths)))
            self.assertIn(primary, paths)
            for path in paths:
                with self.subTest(path=path):
                    metadata = path.stat()
                    self.assertTrue(stat.S_ISREG(metadata.st_mode))
                    self.assertEqual(metadata.st_uid, os.geteuid())
                    self.assertEqual(stat.S_IMODE(metadata.st_mode), 0o600)
                    self.assertEqual(path.read_bytes(), b"")
            self.assertEqual(document["blob_storage"]["staging_directory"], str(root / "staging"))
            self.assertEqual(document["blob_storage"]["stores"][0]["root_directory"], str(root / "blobs"))

    def test_missing_integration_defaults_get_private_empty_files(self):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            for name, preferred in [("github-token", str(root / "absent-home-token")), ("brave-api-key", "")]:
                with self.subTest(integration=name):
                    placeholder = root / name
                    result = subprocess.run([
                        "signalbox-dev-credential", "", preferred, str(placeholder),
                    ], check=True, capture_output=True, text=True)
                    self.assertEqual(result.stdout.strip(), str(placeholder))
                    metadata = placeholder.stat()
                    self.assertTrue(stat.S_ISREG(metadata.st_mode))
                    self.assertEqual(metadata.st_uid, os.geteuid())
                    self.assertEqual(stat.S_IMODE(metadata.st_mode), 0o600)
                    self.assertEqual(placeholder.read_bytes(), b"")

    def test_explicit_credential_override_is_not_provisioned(self):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            override = root / "supplied-key"
            preferred = root / "home-key"
            preferred.write_bytes(b"home-key-fixture")
            placeholder = root / "placeholder"
            result = subprocess.run([
                "signalbox-dev-credential", str(override), str(preferred), str(placeholder),
            ], check=True, capture_output=True, text=True)
            self.assertEqual(result.stdout.strip(), str(override))
            self.assertFalse(override.exists())
            self.assertFalse(placeholder.exists())

    def test_existing_home_credential_is_selected_without_rewriting(self):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            preferred = root / "home-key"
            preferred.write_bytes(b"home-key-fixture")
            preferred.chmod(0o400)
            placeholder = root / "placeholder"
            result = subprocess.run([
                "signalbox-dev-credential", "", str(preferred), str(placeholder),
            ], check=True, capture_output=True, text=True)
            self.assertEqual(result.stdout.strip(), str(preferred))
            self.assertEqual(preferred.read_bytes(), b"home-key-fixture")
            self.assertEqual(stat.S_IMODE(preferred.stat().st_mode), 0o400)
            self.assertFalse(placeholder.exists())

    def test_existing_placeholder_contents_survive_provisioning(self):
        with tempfile.TemporaryDirectory() as directory:
            placeholder = Path(directory) / "key"
            placeholder.write_bytes(b"edited-key-fixture")
            placeholder.chmod(0o600)
            subprocess.run([
                "signalbox-dev-credential", "", "", str(placeholder),
            ], check=True, capture_output=True)
            self.assertEqual(placeholder.read_bytes(), b"edited-key-fixture")
            self.assertEqual(stat.S_IMODE(placeholder.stat().st_mode), 0o600)

    def test_config_materialization_resolves_the_default_supervisor(self):
        with tempfile.TemporaryDirectory() as directory:
            source = Path(directory) / "source.toml"
            destination = Path(directory) / "result.toml"
            source.write_text(
                '[daemon_tools]\n'
                'exec_supervisor_executable = "/usr/local/bin/signalbox-exec-supervisor"\n'
            )
            subprocess.run([
                "signalbox-materialize-config", str(source), str(destination),
                "/fixture/supervisor with spaces",
            ], check=True)
            self.assertEqual(tomllib.loads(destination.read_text()), {
                "daemon_tools": {"exec_supervisor_executable": "/fixture/supervisor with spaces"},
            })

    def test_config_materialization_preserves_an_explicit_supervisor(self):
        with tempfile.TemporaryDirectory() as directory:
            source = Path(directory) / "source.toml"
            destination = Path(directory) / "result.toml"
            original = '[daemon_tools]\nexec_supervisor_executable = "/fixture/custom"\n'
            source.write_text(original)
            subprocess.run([
                "signalbox-materialize-config", str(source), str(destination),
                "/fixture/default",
            ], check=True)
            self.assertEqual(destination.read_text(), original)

    def test_config_materialization_adds_an_absent_daemon_tools_table(self):
        with tempfile.TemporaryDirectory() as directory:
            source = Path(directory) / "source.toml"
            destination = Path(directory) / "result.toml"
            source.write_text('version = 1\n')
            subprocess.run([
                "signalbox-materialize-config", str(source), str(destination),
                "/fixture/supervisor",
            ], check=True)
            self.assertEqual(tomllib.loads(destination.read_text()), {
                "version": 1,
                "daemon_tools": {"exec_supervisor_executable": "/fixture/supervisor"},
            })

    def test_client_wrapper_preserves_arguments_and_exit_status(self):
        wrapper = shutil.which("signalbox")
        self.assertIsNotNone(wrapper, "run this suite inside devenv shell")
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            rustc = root / "rustc"
            rustc.write_text('#!/bin/sh\nprintf "host: fixture-host\\n"\n')
            rustc.chmod(0o700)
            cargo = root / "cargo"
            cargo.write_text('#!/bin/sh\nprintf "%s\\n" "$SMOKE_CLIENT"\n')
            cargo.chmod(0o700)
            client = root / "resolved client"
            client.write_text(
                '#!/usr/bin/env python3\nimport json, sys\n'
                'print(json.dumps(sys.argv[1:]))\nsys.exit(23)\n'
            )
            client.chmod(0o700)
            arguments = ["--fixture", "two words", "literal $value; `command`"]
            result = subprocess.run([wrapper, *arguments], check=False, capture_output=True, text=True,
                env={**os.environ, "PATH": f"{root}{os.pathsep}{os.environ['PATH']}",
                     "SMOKE_CLIENT": str(client)})
            self.assertEqual(result.returncode, 23, result.stderr)
            self.assertEqual(json.loads(result.stdout), arguments)


if __name__ == "__main__":
    unittest.main()
