"""Run Linux test binaries with the native toolchain's declared runtime."""

def _runfile_path(file):
    if not file.short_path.startswith("../"):
        fail("The native runtime must come from an external toolchain")
    return file.short_path[3:]

def _native_runtime_impl(ctx):
    files = ctx.attr.libraries[DefaultInfo].files.to_list()
    loader = [file for file in files if file.basename == "ld-linux-x86-64.so.2"]
    if len(loader) != 1:
        fail("Expected one x86-64 Linux dynamic loader")
    directories = sorted({
        _runfile_path(file).rsplit("/", 1)[0]: True
        for file in files
        if file.basename in ["libc.so.6", "libgcc_s.so.1"]
    }.keys())
    executable = ctx.actions.declare_file(ctx.label.name + ".sh")
    ctx.actions.write(
        executable,
        "\n".join([
            "#!/bin/bash",
            "set -euo pipefail",
            # Script launchers already select their declared interpreter.
            'IFS= read -r -n 4 magic < "$1" || true',
            'if [[ "$magic" != $\'\\x7fELF\' ]]; then exec "$@"; fi',
            "patch_binary() {",
            '  "${RUNFILES_DIR:?}/%s" --set-interpreter "${RUNFILES_DIR}/%s" --set-rpath "%s" --output "$2" "$1"' % (
                _runfile_path(ctx.file.patchelf),
                _runfile_path(loader[0]),
                ":".join(["${RUNFILES_DIR}/" + directory for directory in directories]),
            ),
            "}",
            # Helper processes may clear their environment, so give them an ELF
            # with its runtime encoded instead of an environment-dependent script.
            "while IFS= read -r variable; do",
            '  patched_helper="${TEST_TMPDIR:?}/${variable}.${BASHPID}"',
            '  patch_binary "${!variable}" "$patched_helper"',
            '  export "$variable=$patched_helper"',
            "done < <(compgen -A variable NEXTEST_BIN_EXE_ || true)",
            # Executing the loader directly makes current_exe() return the
            # loader, breaking tests that spawn another copy of themselves.
            'patched_executable="${TEST_TMPDIR:?}/${1##*/}.${BASHPID}"',
            'patch_binary "$1" "$patched_executable"',
            "shift",
            'exec "$patched_executable" "$@"',
            "",
        ]),
        is_executable = True,
    )
    return [DefaultInfo(
        executable = executable,
        runfiles = ctx.runfiles(files = files + [ctx.file.patchelf]),
    )]

native_runtime = rule(
    implementation = _native_runtime_impl,
    executable = True,
    attrs = {
        "libraries": attr.label(mandatory = True),
        "patchelf": attr.label(mandatory = True, allow_single_file = True),
    },
)
