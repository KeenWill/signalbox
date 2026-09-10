"""Copy the built test and its ELF runtime into the disposable benchmark image."""

import pathlib, re, shutil, subprocess, sys

root = pathlib.Path("tooling/ci-capacity")
(root / "lib").mkdir(exist_ok=True)
binary = pathlib.Path(sys.argv[1])
assert binary.read_bytes()[:4] == b"\x7fELF"
shutil.copy2(binary, root / "postgres-tests")
headers = subprocess.check_output(["readelf", "-l", str(binary)], text=True)
loader = pathlib.Path(re.search(r"Requesting program interpreter: (.*?)\]", headers)[1])
for name in ["ld-linux-x86-64.so.2", "libc.so.6", "libm.so.6"]:
    shutil.copy2(loader.parent / name, root / "lib" / name)
linked = subprocess.check_output(["ldd", str(binary)], text=True)
libgcc = re.search(r"libgcc_s\.so\.1 => (\S+)", linked)[1]
shutil.copy2(libgcc, root / "lib/libgcc_s.so.1")
