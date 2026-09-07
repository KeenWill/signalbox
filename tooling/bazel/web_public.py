"""Move public fixtures through a deterministic archive without renaming their URLs."""

from pathlib import Path
import sys
import tarfile


def main():
    mode, source, output = sys.argv[1:]
    if mode == "pack":
        root = Path(source)
        with tarfile.open(output, "w") as archive:
            for path in sorted(root.rglob("*")):
                if path.is_file():
                    info = tarfile.TarInfo(path.relative_to(root).as_posix())
                    info.size = path.stat().st_size
                    info.mode = 0o644
                    with path.open("rb") as contents:
                        archive.addfile(info, contents)
    else:
        with tarfile.open(source) as archive:
            archive.extractall(output, filter="data")


if __name__ == "__main__":
    main()
