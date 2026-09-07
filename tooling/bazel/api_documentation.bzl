"""Render API documentation from declared Rustdoc JSON and page boundaries."""

def _api_documentation_impl(ctx):
    output = ctx.actions.declare_directory(ctx.label.name)
    arguments = ctx.actions.args()
    arguments.add("--source-root", ".")
    arguments.add("--output-dir", output.path)
    arguments.add("--json-dir")
    arguments.add_all(ctx.files.json, expand_directories = False)
    ctx.actions.run(
        executable = ctx.executable.renderer,
        arguments = [arguments],
        inputs = ctx.files.json + ctx.files.sources,
        tools = [ctx.attr.renderer[DefaultInfo].files_to_run, ctx.file.rustfmt],
        env = {"RUSTFMT": ctx.file.rustfmt.path},
        outputs = [output],
        mnemonic = "RenderApi",
    )
    return [DefaultInfo(files = depset([output]))]

api_documentation = rule(
    implementation = _api_documentation_impl,
    attrs = {
        "json": attr.label_list(allow_files = True),
        "renderer": attr.label(default = Label("//scripts:render_api"), executable = True, cfg = "exec"),
        "rustfmt": attr.label(default = Label("@rules_rust//rust/toolchain:current_rustfmt_toolchain"), allow_single_file = True, cfg = "exec"),
        "sources": attr.label_list(allow_files = True),
    },
)
