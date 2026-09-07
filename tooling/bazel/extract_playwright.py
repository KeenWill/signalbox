"""Extract the browser runtime paths from a pinned Playwright image layer."""

import pathlib
import posixpath
import sys
import tarfile

RUNTIME_PATHS = (
    "usr/bin/",
    "usr/lib/",
    "usr/share/fonts/",
    "usr/share/fontconfig/",
    "usr/share/glib-2.0/",
    "etc/fonts/",
    "etc/alternatives/",
    "ms-playwright/",
)


def main():
    destination, *archives = sys.argv[1:]
    root = pathlib.Path(destination)
    for archive_path in archives:
        with tarfile.open(archive_path) as archive:
            for member in archive:
                if not member.name.startswith(RUNTIME_PATHS):
                    continue
                if member.issym() and member.linkname.startswith("/"):
                    member.linkname = posixpath.relpath(
                        member.linkname.lstrip("/"), posixpath.dirname(member.name)
                    )
                archive.extract(member, root, filter="data")
    for path in root.rglob("*"):
        if path.is_symlink() and not path.exists():
            path.unlink()


if __name__ == "__main__":
    main()
