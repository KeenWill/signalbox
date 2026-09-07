"""Expose the PostgreSQL suite manifest to BUILD files."""

load("@toml.bzl", "toml")

def _manifest_impl(ctx):
    suites = toml.decode(ctx.read(ctx.attr.manifest))["suite"]
    workspace = toml.decode(ctx.read(ctx.attr.workspace))
    root = ctx.path(ctx.attr.workspace).dirname
    manifests = {}
    for member in workspace["workspace"]["members"]:
        package = toml.decode(ctx.read(root.get_child(member, "Cargo.toml")))
        manifests[package["package"]["name"]] = package
    result = {}
    for suite in suites:
        features = manifests[suite["package"]].get("features", {})
        enabled = {feature: True for feature in suite["features"]}
        for _ in range(len(features)):
            for feature in list(enabled.keys()):
                for dependency in features.get(feature, []):
                    if dependency in features:
                        enabled[dependency] = True
        suite["crate_features"] = sorted(enabled.keys())
        result[suite["name"]] = suite
    ctx.file("BUILD.bazel", 'exports_files(["defs.bzl"])\n')
    ctx.file("defs.bzl", "SUITES = " + repr(result) + "\n")

_manifest = repository_rule(
    implementation = _manifest_impl,
    attrs = {
        "manifest": attr.label(default = "//:.github/postgres-integration-suites.toml"),
        "workspace": attr.label(default = "//:Cargo.toml"),
    },
)

def _extension_impl(_ctx):
    _manifest(name = "postgres_suites")

postgres_suites = module_extension(implementation = _extension_impl)
