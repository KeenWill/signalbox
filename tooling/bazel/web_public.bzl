"""Declare public URL fixtures whose colon-containing paths cannot be Bazel labels."""

def _web_public_repository_impl(ctx):
    source = ctx.path(ctx.attr.package).dirname.get_child("public")
    ctx.watch_tree(source)
    result = ctx.execute([
        ctx.which("python3"),
        ctx.path(ctx.attr.tool),
        "pack",
        source,
        ctx.path("public.tar"),
    ])
    if result.return_code:
        fail(result.stderr)
    ctx.file("BUILD.bazel", 'exports_files(["public.tar"])')

web_public_repository = repository_rule(
    implementation = _web_public_repository_impl,
    attrs = {
        "package": attr.label(default = "//clients/web:package.json", allow_single_file = True),
        "tool": attr.label(default = "//tooling/bazel:web_public.py", allow_single_file = True),
    },
)

def _web_public_files_impl(ctx):
    output = ctx.actions.declare_directory(ctx.label.name)
    ctx.actions.run(
        executable = ctx.executable._tool,
        arguments = ["unpack", ctx.file.archive.path, output.path],
        inputs = [ctx.file.archive],
        outputs = [output],
        mnemonic = "WebPublicFiles",
    )
    return [DefaultInfo(files = depset([output]))]

web_public_files = rule(
    implementation = _web_public_files_impl,
    attrs = {
        "archive": attr.label(default = "@web_public//:public.tar", allow_single_file = True),
        "_tool": attr.label(default = "//tooling/bazel:web_public_tool", executable = True, cfg = "exec"),
    },
)
