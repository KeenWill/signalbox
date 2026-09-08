#!/usr/bin/env python3
"""Exercise the scripts produced by `devenv shell` with disposable fixtures."""

import json
import os
from pathlib import Path
import shutil
import subprocess
import tempfile
import tomllib
import unittest


class DevenvScriptsTests(unittest.TestCase):
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
