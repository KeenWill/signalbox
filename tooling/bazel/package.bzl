"""Cargo metadata and source inputs shared by Signalbox Rust packages."""

load("@rules_rust//cargo:defs.bzl", "cargo_toml_env_vars", "extract_cargo_lints")

def rust_package(exports = ["Cargo.toml"]):
    """Declare package metadata, lint settings, and Rust source fixtures.

    Args:
        exports: Files exposed to other packages.
    """
    if exports:
        native.exports_files(exports)
    cargo_toml_env_vars(
        name = "cargo_environment",
        src = "Cargo.toml",
        workspace = "//:Cargo.toml",
    )
    extract_cargo_lints(
        name = "cargo_lints",
        manifest = "Cargo.toml",
        workspace = "//:Cargo.toml",
    )
    native.filegroup(
        name = "rust_sources",
        srcs = native.glob(["**/*.rs"]),
        visibility = ["//:__pkg__"],
    )
