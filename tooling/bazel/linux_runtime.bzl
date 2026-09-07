"""Select the loader and runtime libraries used by native Linux actions."""

def linux_runtime(files):
    """Return the dynamic loader and libraries that supply its search paths.

    Args:
        files: Files from the pinned Linux runtime.

    Returns:
        A struct with a loader File and the libc/libgcc library Files.
    """
    loaders = [file for file in files if file.basename == "ld-linux-x86-64.so.2"]
    if len(loaders) != 1:
        fail("Expected one x86-64 Linux dynamic loader")
    return struct(
        loader = loaders[0],
        libraries = [file for file in files if file.basename in ["libc.so.6", "libgcc_s.so.1"]],
    )
