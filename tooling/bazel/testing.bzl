"""Rust unit, integration, PostgreSQL, and API-documentation defaults."""

load("@crates//:defs.bzl", "aliases", "all_crate_deps", "crate_edition")
load("@postgres_suites//:defs.bzl", "SUITES")
load("@rules_rust//rust:defs.bzl", _rust_doc = "rust_doc", _rust_doc_test = "rust_doc_test", _rust_test = "rust_test")
load(":rust.bzl", "RUSTC_FLAGS")

_TEST_ARGS = ["--test-threads=16"]
_UNIT_RUSTC_FLAGS = ["--deny=warnings", "--forbid=unsafe_code"]

def _test_defaults(kwargs):
    kwargs.setdefault("size", "large")
    kwargs.setdefault("visibility", ["//:__pkg__"])
    kwargs.setdefault("args", _TEST_ARGS)
    kwargs.setdefault("aliases", aliases(normal_dev = True, proc_macro_dev = True))
    kwargs.setdefault("lint_config", ":cargo_lints")

def rust_unit_test(name, crate, deps = [], proc_macro_deps = [], **kwargs):
    """Test an existing crate with its Cargo development dependencies.

    Args:
        name: Target name.
        crate: Library, binary, or procedural macro under test.
        deps: Additional dependencies beyond the crate and Cargo dev dependencies.
        proc_macro_deps: Additional macros beyond Cargo dev procedural macros.
        **kwargs: rules_rust attributes, including overrides of shared defaults.
    """
    _test_defaults(kwargs)
    kwargs.setdefault("rustc_flags", _UNIT_RUSTC_FLAGS)
    _rust_test(
        name = name,
        crate = crate,
        deps = deps + all_crate_deps(normal_dev = True),
        proc_macro_deps = proc_macro_deps + all_crate_deps(proc_macro_dev = True),
        **kwargs
    )

def rust_integration_test(name, crate_root, deps = [], proc_macro_deps = [], **kwargs):
    """Build a standalone test with Cargo normal and development dependencies.

    Args:
        name: Target name.
        crate_root: Test entrypoint; also the default source list.
        deps: Additional dependencies beyond Cargo normal and dev dependencies.
        proc_macro_deps: Additional macros beyond Cargo normal and dev macros.
        **kwargs: rules_rust attributes, including sources, fixtures and overrides.
    """
    _test_defaults(kwargs)
    kwargs.setdefault("srcs", [crate_root])
    kwargs.setdefault("edition", crate_edition())
    kwargs.setdefault("rustc_env_files", [":cargo_environment"])
    kwargs.setdefault("rustc_flags", RUSTC_FLAGS)
    _rust_test(
        name = name,
        crate_root = crate_root,
        deps = deps + all_crate_deps(normal = True, normal_dev = True),
        proc_macro_deps = proc_macro_deps + all_crate_deps(proc_macro = True, proc_macro_dev = True),
        **kwargs
    )

def rust_postgres_test(name, compile_data = [], tags = [], **kwargs):
    """Run an ignored PostgreSQL integration test with the shared image input.

    Args:
        name: Target name.
        compile_data: Fixtures in addition to the shared PostgreSQL image pin.
        tags: Tags in addition to manual and requires-network.
        **kwargs: rust_integration_test arguments and rules_rust overrides.
    """
    kwargs.setdefault("args", ["--ignored"] + _TEST_ARGS)
    rust_integration_test(
        name = name,
        compile_data = ["//:tooling/postgres_test_image.rs"] + compile_data,
        tags = ["manual", "requires-network", "external"] + tags,
        **kwargs
    )

def _test_selection_impl(ctx):
    executable = ctx.actions.declare_file(ctx.label.name)
    ctx.actions.symlink(
        output = executable,
        target_file = ctx.executable.test,
        is_executable = True,
    )
    return [
        DefaultInfo(
            executable = executable,
            runfiles = ctx.attr.test[0][DefaultInfo].default_runfiles,
        ),
        RunEnvironmentInfo(
            environment = dict(ctx.attr.test[0][RunEnvironmentInfo].environment, **ctx.attr.env),
            inherited_environment = ctx.attr.test[0][RunEnvironmentInfo].inherited_environment,
        ),
    ]

def _json_feature_impl(ctx):
    return []

json_preserve_order = rule(
    implementation = _json_feature_impl,
    build_setting = config.bool(flag = True),
)

def _cargo_json_transition_impl(settings, attr):
    return {"//tooling/bazel:json_preserve_order": attr.json_preserve_order}

_cargo_json_transition = transition(
    implementation = _cargo_json_transition_impl,
    inputs = [],
    outputs = ["//tooling/bazel:json_preserve_order"],
)

rust_selection_test = rule(
    implementation = _test_selection_impl,
    test = True,
    attrs = {
        "test": attr.label(executable = True, cfg = _cargo_json_transition, mandatory = True),
        "json_preserve_order": attr.bool(default = True),
        "_allowlist_function_transition": attr.label(default = "@bazel_tools//tools/allowlists/function_transition_allowlist"),
        "env": attr.string_dict(),
    },
    doc = "Run another selection of a compiled libtest binary using the target's args.",
)

def rust_doc_test(name, deps = [], proc_macro_deps = [], **kwargs):
    """Run a crate's documentation examples with its development dependencies.

    Args:
        name: Target name.
        deps: First-party development dependencies.
        proc_macro_deps: First-party development procedural macros.
        **kwargs: rust_doc_test attributes, including crate and features.
    """
    _rust_doc_test(
        name = name,
        deps = deps + all_crate_deps(normal_dev = True),
        proc_macro_deps = proc_macro_deps + all_crate_deps(proc_macro_dev = True),
        **kwargs
    )

def rust_api_json(name = "api_json", **kwargs):
    """Expose a crate's nightly Rustdoc JSON to the workspace API renderer.

    Args:
        name: Target name.
        **kwargs: rust_doc attributes, including crate and crate_features.
    """
    kwargs.setdefault("rustdoc_flags", ["-Z", "unstable-options", "--output-format", "json"])
    kwargs.setdefault("visibility", ["//:__pkg__"])
    _rust_doc(name = name, **kwargs)

def rust_postgres_suite(name, binaries, json_preserve_order = True):
    """Select ignored tests and shards from the shared suite manifest.

    Args:
        name: Manifest suite name.
        binaries: Compiled Bazel test labels mapped to Cargo test binary names.
        json_preserve_order: Whether the Cargo suite enables serde_json/preserve_order.
    """
    suite = SUITES[name]
    selected = []
    for test, binary in binaries.items():
        if suite.get("include_binaries") and binary not in suite["include_binaries"]:
            continue
        if binary in suite.get("exclude_binaries", []):
            continue
        target = "postgres_" + name.replace("-", "_") + "_" + test.rsplit(":", 1)[-1]
        rust_selection_test(
            name = target,
            test = test,
            json_preserve_order = json_preserve_order,
            args = ["--ignored"] + _TEST_ARGS + [arg for skip in suite["skip"] for arg in ["--skip", skip]],
            env = {"RUST_MIN_STACK": "8388608"},
            shard_count = suite["shards"],
            tags = ["manual", "external", "no-sandbox", "requires-network"],
            size = "enormous",
        )
        selected.append(":" + target)
    native.test_suite(
        name = "postgres_" + name.replace("-", "_"),
        tests = selected,
        tags = ["manual"],
    )
