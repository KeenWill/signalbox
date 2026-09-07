"""Compare a generated output tree with its declared repository snapshots."""

import argparse
import json
from pathlib import Path


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("manifest", type=Path)
    parser.add_argument("generated", type=Path)
    parser.add_argument("--source-prefix", type=Path, default=Path("."))
    arguments = parser.parse_args()
    sources = [Path(path) for path in json.loads(arguments.manifest.read_text())]
    expected = {path.relative_to(arguments.source_prefix): path for path in sources}
    actual = {
        path.relative_to(arguments.generated)
        for path in arguments.generated.rglob("*")
        if path.is_file()
    }
    failed = False
    for path in sorted(actual | expected.keys()):
        source = expected.get(path)
        generated = arguments.generated / path
        if source is None or path not in actual or source.read_bytes() != generated.read_bytes():
            print(f"generated artifact differs: {path}")
            failed = True
    return int(failed)


if __name__ == "__main__":
    raise SystemExit(main())
