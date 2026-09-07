"""Generate artifacts using a Rust binary and its declared Linux runtime."""

load(":linux_runtime.bzl", "linux_runtime")

def _native_generation_impl(ctx):
    libraries = ctx.files.libraries
    runtime = linux_runtime(libraries)
    directories = sorted({file.dirname: True for file in runtime.libraries}.keys())
    output = ctx.actions.declare_directory(ctx.label.name)
    arguments = ctx.actions.args()
    arguments.add("--library-path", ":".join(directories))
    arguments.add(ctx.executable.generator)
    arguments.add_all(ctx.attr.arguments)
    arguments.add(output.path)
    ctx.actions.run(
        executable = runtime.loader,
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
