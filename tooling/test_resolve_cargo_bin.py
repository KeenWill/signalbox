#!/usr/bin/env python3
"""Regression tests for tooling/resolve-cargo-bin.sh.

Real `cargo build` is deliberately never invoked here: a fake `cargo` and
`rustc` on `PATH` report a controlled build-message stream and host triple, so
the resolver's own path logic is exercised in isolation and deterministically,
without a real compile.
"""

from __future__ import annotations

import stat
import subprocess
import sys
import tempfile
import unittest
from pathlib import Path

RESOLVER = Path(__file__).resolve().parent / "resolve-cargo-bin.sh"


def write_fake_executable(path: Path, script_body: str) -> None:
    """Write one executable shell fixture at path, outside test bodies."""
    path.write_text(f"#!/usr/bin/env bash\n{script_body}\n")
    path.chmod(path.stat().st_mode | stat.S_IEXEC | stat.S_IXGRP | stat.S_IXOTH)


def fake_toolchain(bin_dir: Path, built_executable: str, host_triple: str) -> None:
    """Install a fake cargo and rustc reporting one fixed build and host, outside test bodies."""
    write_fake_executable(
        bin_dir / "cargo",
        f'echo \'{{"reason":"compiler-artifact","executable":"{built_executable}"}}\'',
    )
    write_fake_executable(bin_dir / "rustc", f'echo "host: {host_triple}"')


def run_resolver(bin_dir: Path, target_dir: Path, package: str, bin_name: str) -> subprocess.CompletedProcess[str]:
    """Invoke the real resolver against one fake toolchain, outside test bodies."""
    return subprocess.run(
        [
            str(RESOLVER),
            str(target_dir / "Cargo.toml"),
            str(target_dir),
            package,
            bin_name,
        ],
        env={"PATH": f"{bin_dir}:/usr/bin:/bin"},
        capture_output=True,
        text=True,
    )


class ResolveCargoBinTests(unittest.TestCase):
    def test_configured_build_target_resolves_the_nested_path(self) -> None:
        """S1: a host-named build.target places the binary under
        target/<triple>/debug, and the resolver reports that exact path
        rather than the plain target/debug path a caller with no configured
        target would produce — the defect both reviewers reported on #308."""
        with tempfile.TemporaryDirectory() as workspace:
            target_dir = Path(workspace) / "target"
            bin_dir = Path(workspace) / "fake-bin"
            bin_dir.mkdir()
            configured_path = target_dir / "x86_64-unknown-linux-gnu" / "debug" / "signalbox"
            fake_toolchain(bin_dir, str(configured_path), "x86_64-unknown-linux-gnu")

            result = run_resolver(bin_dir, target_dir, "signalbox-client", "signalbox")

        self.assertEqual(result.returncode, 0)
        self.assertEqual(result.stdout.strip(), str(configured_path))

    def test_unconfigured_build_resolves_the_plain_path(self) -> None:
        """S2: with no build.target configured, the binary lands directly
        under target/debug, and the resolver reports that plain path."""
        with tempfile.TemporaryDirectory() as workspace:
            target_dir = Path(workspace) / "target"
            bin_dir = Path(workspace) / "fake-bin"
            bin_dir.mkdir()
            plain_path = target_dir / "debug" / "signalbox"
            fake_toolchain(bin_dir, str(plain_path), "x86_64-unknown-linux-gnu")

            result = run_resolver(bin_dir, target_dir, "signalbox-client", "signalbox")

        self.assertEqual(result.returncode, 0)
        self.assertEqual(result.stdout.strip(), str(plain_path))

    def test_foreign_target_is_refused_naming_the_host_triple(self) -> None:
        """S3: a build.target this host cannot execute resolves to neither
        expected layout, and the resolver refuses rather than exec a binary
        that would fail, naming the host triple in its refusal."""
        with tempfile.TemporaryDirectory() as workspace:
            target_dir = Path(workspace) / "target"
            bin_dir = Path(workspace) / "fake-bin"
            bin_dir.mkdir()
            foreign_path = target_dir / "aarch64-unknown-linux-musl" / "debug" / "signalbox"
            fake_toolchain(bin_dir, str(foreign_path), "x86_64-unknown-linux-gnu")

            result = run_resolver(bin_dir, target_dir, "signalbox-client", "signalbox")

        self.assertNotEqual(result.returncode, 0)
        self.assertIn("x86_64-unknown-linux-gnu", result.stderr)


if __name__ == "__main__":
    unittest.main()
