"""Resolve the native V8 archive from Cargo and retain its digest in Bazel's lockfile."""

load("@bazel_tools//tools/build_defs/repo:http.bzl", "http_file")

def _v8_version(lockfile):
    versions = []
    for package in lockfile.split("[[package]]")[1:]:
        fields = {}
        for line in package.splitlines():
            key, separator, value = line.partition(" = ")
            if separator and key in ["name", "version"]:
                fields[key] = json.decode(value)
        if fields.get("name") == "v8":
            versions.append(fields["version"])
    if len(versions) != 1:
        fail("Expected one V8 version in Cargo.lock, found %s" % versions)
    return versions[0]

def _v8_impl(ctx):
    version = _v8_version(ctx.read(Label("//:Cargo.lock")))
    url = "https://github.com/denoland/rusty_v8/releases/download/v%s/librusty_v8_simdutf_release_x86_64-unknown-linux-gnu.a.gz" % version
    sha256 = ctx.facts.get(url)
    if not sha256:
        # Resolve each new release once; subsequent resolutions verify the
        # digest retained in MODULE.bazel.lock, including unrelated Cargo bumps.
        sha256 = ctx.download(url = url, output = "librusty_v8.a.gz").sha256
    http_file(
        name = "rusty_v8_archive",
        downloaded_file_path = "librusty_v8.a.gz",
        sha256 = sha256,
        urls = [url],
    )
    return ctx.extension_metadata(
        root_module_direct_deps = ["rusty_v8_archive"],
        root_module_direct_dev_deps = [],
        facts = {url: sha256},
    )

v8 = module_extension(implementation = _v8_impl)
