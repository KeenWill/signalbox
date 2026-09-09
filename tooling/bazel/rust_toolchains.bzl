"""Resolve Rust archive checksums from release manifests and retain them in the lockfile."""

load("@rules_rust//rust/platform:triple.bzl", "get_host_triple")
load("@rules_rust//rust/private:repositories.bzl", "DEFAULT_TOOLCHAIN_TRIPLES", "rust_register_toolchains", "rust_toolchain_tools_repository")
load("@rules_rust//rust/private:repository_utils.bzl", "DEFAULT_EXTRA_TARGET_TRIPLES")
load("@toml.bzl", "toml")

_COMPONENTS = ["rustc", "cargo", "clippy-preview", "rustfmt-preview", "llvm-tools-preview", "rust-std", "rust-src", "rust-analyzer-preview"]

def _checksums(ctx, version, facts):
    channel, separator, date = version.partition("/")
    prefix = date + "/" if separator else ""
    url = "https://static.rust-lang.org/dist/" + prefix + "channel-rust-" + channel + ".toml"
    path = version.replace("/", "-") + ".toml"
    facts[url] = ctx.download(
        url = url,
        output = path,
        sha256 = ctx.facts.get(url, ""),
    ).sha256
    manifest = toml.decode(ctx.read(path))
    triples = list(DEFAULT_TOOLCHAIN_TRIPLES) + list(DEFAULT_EXTRA_TARGET_TRIPLES) + ["*"]
    archives = {}
    for component in _COMPONENTS:
        for triple, target in manifest["pkg"][component]["target"].items():
            if triple in triples and target["available"]:
                archive = prefix + target["xz_url"].rsplit("/", 1)[-1]
                archives[archive] = target["xz_hash"]
    return archives

def _rust_impl(ctx):
    facts = {}
    for mod in ctx.modules:
        if not mod.is_root:
            continue
        for toolchain in mod.tags.toolchain:
            sha256s = {}
            for version in depset(toolchain.versions + [toolchain.rustfmt_version]).to_list():
                sha256s.update(_checksums(ctx, version, facts))
            rust_register_toolchains(
                hub_name = "rust_toolchains",
                edition = toolchain.edition,
                versions = toolchain.versions,
                rustfmt_version = toolchain.rustfmt_version,
                sha256s = sha256s,
            )
    return ctx.extension_metadata(
        root_module_direct_deps = ["rust_toolchains"],
        root_module_direct_dev_deps = [],
        facts = facts,
    )

rust = module_extension(
    implementation = _rust_impl,
    tag_classes = {
        "toolchain": tag_class(attrs = {
            "edition": attr.string(mandatory = True),
            "versions": attr.string_list(mandatory = True),
            "rustfmt_version": attr.string(mandatory = True),
        }),
    },
)

def _host_tools_impl(ctx):
    facts = {}
    repositories = []
    triple = get_host_triple(ctx).str
    for mod in ctx.modules:
        if not mod.is_root:
            continue
        for tools in mod.tags.host_tools:
            rust_toolchain_tools_repository(
                name = tools.name,
                version = tools.version,
                rustfmt_version = tools.version,
                exec_triple = triple,
                target_triple = triple,
                sha256s = _checksums(ctx, tools.version, facts),
            )
            repositories.append(tools.name)
    return ctx.extension_metadata(
        root_module_direct_deps = repositories,
        root_module_direct_dev_deps = [],
        facts = facts,
    )

rust_host_tools = module_extension(
    implementation = _host_tools_impl,
    os_dependent = True,
    arch_dependent = True,
    tag_classes = {
        "host_tools": tag_class(attrs = {
            "name": attr.string(mandatory = True),
            "version": attr.string(mandatory = True),
        }),
    },
)
