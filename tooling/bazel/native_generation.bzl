"""Generate artifacts using a Rust binary and its declared Linux runtime."""

def _native_generation_impl(ctx):
    libraries = ctx.files.libraries
    loader = [file for file in libraries if file.basename == "ld-linux-x86-64.so.2"]
    if len(loader) != 1:
        fail("Expected one x86-64 Linux dynamic loader")
    directories = sorted({
        file.dirname: True
        for file in libraries
        if file.basename in ["libc.so.6", "libgcc_s.so.1"]
    }.keys())
    output = ctx.actions.declare_directory(ctx.label.name)
    arguments = ctx.actions.args()
    arguments.add("--library-path", ":".join(directories))
    arguments.add(ctx.executable.generator)
    arguments.add_all(ctx.attr.arguments)
    arguments.add(output.path)
    ctx.actions.run(
        executable = loader[0],
        arguments = [arguments],
        inputs = libraries,
        tools = [ctx.attr.generator[DefaultInfo].files_to_run],
        outputs = [output],
        mnemonic = "GenerateArtifacts",
    )
    return [DefaultInfo(files = depset([output]))]

native_generation = rule(
    implementation = _native_generation_impl,
    attrs = {
        "arguments": attr.string_list(),
        "generator": attr.label(mandatory = True, executable = True, cfg = "exec"),
        "libraries": attr.label(default = Label("@linux_test_runtime//:libraries")),
    },
)
