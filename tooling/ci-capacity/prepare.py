"""Copy the built test and its ELF runtime into the disposable benchmark image."""

import pathlib, re, shutil, subprocess, sys

root = pathlib.Path("tooling/ci-capacity")
(root / "lib").mkdir(exist_ok=True)
binary = pathlib.Path(sys.argv[1])
assert binary.read_bytes()[:4] == b"\x7fELF"
shutil.copy2(binary, root / "postgres-tests")
headers = subprocess.check_output(["readelf", "-l", str(binary)], text=True)
loader = pathlib.Path(re.search(r"Requesting program interpreter: (.*?)\]", headers)[1])
shutil.copy2(loader, root / "lib/ld-linux-x86-64.so.2")
linked = subprocess.check_output([str(loader), "--list", str(binary)], text=True)
if "not found" in linked:
    raise RuntimeError(f"Missing executable runtime dependency: {linked}")
for name, source in re.findall(r"^\s*([^/\s]+) => (/\S+)", linked, re.MULTILINE):
    shutil.copy2(source, root / "lib" / name)
subprocess.run(
    [
        str((root / "lib/ld-linux-x86-64.so.2").resolve()),
        "--library-path",
        str((root / "lib").resolve()),
        str((root / "postgres-tests").resolve()),
        "--list",
    ],
    check=True,
    stdout=subprocess.DEVNULL,
)
