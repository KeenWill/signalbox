"""Expose declared source paths to repository-wide checker tests."""

def _source_manifest_impl(ctx):
    manifest = ctx.actions.declare_file(ctx.label.name + ".json")
    ctx.actions.write(manifest, json.encode(sorted([file.short_path for file in ctx.files.srcs])))
    return [DefaultInfo(
        files = depset([manifest]),
        runfiles = ctx.runfiles(files = ctx.files.srcs),
    )]

source_manifest = rule(
    implementation = _source_manifest_impl,
    attrs = {"srcs": attr.label_list(allow_files = True)},
)
