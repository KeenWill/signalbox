"""Expose the repository's Rust toolchain selection to test environments."""

load("@toml.bzl", "toml")

def _rust_toolchain_pin_impl(ctx):
    channel = toml.decode(ctx.read(ctx.attr.manifest))["toolchain"]["channel"]
    ctx.file("BUILD.bazel", 'exports_files(["defs.bzl"])\n')
    ctx.file("defs.bzl", "RUSTUP_TOOLCHAIN = " + repr(channel) + "\n")

rust_toolchain_pin = repository_rule(
    implementation = _rust_toolchain_pin_impl,
    attrs = {"manifest": attr.label(mandatory = True)},
)
