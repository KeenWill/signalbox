#!/usr/bin/env python3
"""Check recorded GitHub evidence against frozen differential expectations."""
from __future__ import annotations

import argparse
import copy
import gzip
import json
from pathlib import Path
import subprocess
import sys

ROOT = Path(__file__).resolve().parents[2]
FIXTURES = ROOT / "crates/convergence/fixtures"
POLICY = ROOT / "crates/convergence/examples/repository.toml"


def fixtures():
    yield from sorted(FIXTURES.glob("pr-*.json*"))
    yield from sorted((FIXTURES / "mutations").glob("*.json*"))


def read_recording(path):
    with (gzip.open(path, "rt") if path.suffix == ".gz" else path.open()) as stream:
        value = json.load(stream)
    if "source" not in value:
        return value
    recording = read_recording(path.parent / value["source"])
    for mutation in value["mutations"]:
        parts = [part.replace("~1", "/").replace("~0", "~") for part in mutation["path"].split("/")[1:]]
        target = recording
        for part in parts[:-1]:
            target = target[int(part)] if isinstance(target, list) else target[part]
        key = int(parts[-1]) if isinstance(target, list) else parts[-1]
        target[key] = copy.deepcopy(mutation["value"])
    return recording


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--binary", type=Path, default=ROOT / "target/debug/signalbox-converge")
    parser.add_argument("fixtures", nargs="*", type=Path)
    args = parser.parse_args()
    cases = [path.resolve() for path in args.fixtures] or list(fixtures())
    if not cases:
        parser.error("fixture corpus is empty")
    expectations = json.loads((FIXTURES / "expected.json").read_text())
    names = {str(path.relative_to(FIXTURES)) for path in cases}
    if not names <= expectations.keys() or (not args.fixtures and names != expectations.keys()):
        parser.error("fixture corpus differs from the frozen expectation inventory")
    failed = 0
    for path in cases:
        expected = expectations[str(path.relative_to(FIXTURES))]
        completed = subprocess.run([str(args.binary), "evaluate", "--fixture", str(path), "--policy", str(POLICY)], capture_output=True, text=True, check=False)
        if "error" in expected:
            agrees = completed.returncode == 2
            actual = {"exit": completed.returncode, "stderr": completed.stderr.strip()}
        elif completed.returncode in (0, 1):
            actual = json.loads(completed.stdout)
            agrees = actual["converged"] == expected["converged"] and set(actual["reasons"]) == set(expected["reasons"])
        else:
            actual = {"exit": completed.returncode, "stderr": completed.stderr.strip()}
            agrees = False
        if not agrees:
            failed += 1
            print(json.dumps({"fixture": str(path.relative_to(ROOT)), "expected": expected, "actual": actual}, sort_keys=True))
    print(f"{len(cases)} fixtures, {failed} differences from frozen expectations")
    return int(failed != 0)


if __name__ == "__main__":
    sys.exit(main())
