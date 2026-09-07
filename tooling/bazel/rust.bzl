"""Workspace Rust build defaults with Cargo-managed third-party dependencies."""

load("@crates//:defs.bzl", "aliases", "all_crate_deps", "crate_edition")
load("@rules_rust//rust:defs.bzl", _rust_binary = "rust_binary", _rust_library = "rust_library", _rust_proc_macro = "rust_proc_macro")

RUSTC_FLAGS = ["--forbid=unsafe_code", "--deny=warnings"]

def _cargo_build(rule, deps, proc_macro_deps, kwargs):
    if "srcs" not in kwargs:
        kwargs["srcs"] = native.glob(["src/**/*.rs"])
    kwargs.setdefault("aliases", aliases())
    kwargs.setdefault("edition", crate_edition())
    kwargs.setdefault("lint_config", ":cargo_lints")
    kwargs.setdefault("rustc_env_files", [":cargo_environment"])
    kwargs.setdefault("rustc_flags", RUSTC_FLAGS)
    kwargs.setdefault("visibility", ["//:__subpackages__"])
    rule(
        deps = deps + all_crate_deps(normal = True),
        proc_macro_deps = proc_macro_deps + all_crate_deps(proc_macro = True),
        **kwargs
    )

def rust_library(name, deps = [], proc_macro_deps = [], **kwargs):
    """Build a library with package Cargo metadata and workspace compiler policy.

    Args:
        name: Target name.
        deps: First-party dependencies, followed by Cargo normal dependencies.
        proc_macro_deps: First-party macros, followed by Cargo procedural macros.
        **kwargs: Additional rules_rust attributes or overrides of shared defaults.
    """
    _cargo_build(_rust_library, deps, proc_macro_deps, dict(kwargs, name = name))

def rust_binary(name, deps = [], proc_macro_deps = [], **kwargs):
    """Build a binary with the same defaults and dependency arguments as rust_library.

    Args:
        name: Target name.
        deps: First-party dependencies, followed by Cargo normal dependencies.
        proc_macro_deps: First-party macros, followed by Cargo procedural macros.
        **kwargs: Additional rules_rust attributes or overrides of shared defaults.
    """
    _cargo_build(_rust_binary, deps, proc_macro_deps, dict(kwargs, name = name))

def rust_proc_macro(name, deps = [], proc_macro_deps = [], **kwargs):
    """Build a procedural macro with the shared Cargo and compiler defaults.

    Args:
        name: Target name.
        deps: First-party dependencies, followed by Cargo normal dependencies.
        proc_macro_deps: First-party macros, followed by Cargo procedural macros.
        **kwargs: Additional rules_rust attributes or overrides of shared defaults.
    """
    _cargo_build(_rust_proc_macro, deps, proc_macro_deps, dict(kwargs, name = name))
