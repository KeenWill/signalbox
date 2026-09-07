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
            'exec "${RUNFILES_DIR:?}/%s" --library-path "%s" "$@"' % (
                _runfile_path(loader[0]),
                ":".join(["${RUNFILES_DIR}/" + directory for directory in directories]),
            ),
            "",
        ]),
        is_executable = True,
    )
    return [DefaultInfo(
        executable = executable,
        runfiles = ctx.runfiles(files = files),
    )]

native_runtime = rule(
    implementation = _native_runtime_impl,
    executable = True,
    attrs = {"libraries": attr.label(mandatory = True)},
)
