"""Checksum-pinned Playwright browsers, fonts, and Linux shared libraries."""

load("@bazel_tools//tools/build_defs/repo:http.bzl", "http_archive")

DEJAVU_RELEASE = "version_2_37"

def _playwright_runtime_impl(ctx):
    digest = ctx.attr.digest
    registry = "https://mcr.microsoft.com/v2/playwright/"
    ctx.download(
        url = registry + "manifests/sha256:" + digest,
        output = "manifest.json",
        sha256 = digest,
        headers = {"Accept": "application/vnd.oci.image.index.v1+json, application/vnd.oci.image.manifest.v1+json"},
    )
    manifest = json.decode(ctx.read("manifest.json"))
    if "manifests" in manifest:
        platform = [entry for entry in manifest["manifests"] if entry["platform"].get("os") == "linux" and entry["platform"].get("architecture") == "amd64"][0]
        digest = platform["digest"][len("sha256:"):]
        ctx.download(
            url = registry + "manifests/sha256:" + digest,
            output = "platform.json",
            sha256 = digest,
            headers = {"Accept": "application/vnd.oci.image.index.v1+json, application/vnd.oci.image.manifest.v1+json"},
        )
        manifest = json.decode(ctx.read("platform.json"))
    archives = []
    for layer in manifest["layers"]:
        sha256 = layer["digest"][len("sha256:"):]
        archive = sha256 + ".tar.gz"
        ctx.download(
            url = registry + "blobs/sha256:" + sha256,
            output = archive,
            sha256 = sha256,
        )
        archives.append(ctx.path(archive))
    ctx.watch(ctx.path(ctx.attr.extractor))
    result = ctx.execute([ctx.which("python3"), ctx.path(ctx.attr.extractor), ctx.path(".")] + archives, timeout = 600)
    if result.return_code:
        fail(result.stderr)
    for archive in archives:
        ctx.delete(archive)
    ctx.delete("usr/bin/X11")
    ctx.delete("usr/lib/systemd")
    fonts = ctx.path(ctx.attr.fonts).dirname
    ctx.symlink(fonts.get_child("ttf"), "usr/share/fonts/truetype/dejavu")
    for config in fonts.get_child("fontconfig").readdir():
        ctx.symlink(config, "etc/fonts/conf.d/" + config.basename)
    ctx.file("runtime-root", "")
    ctx.file("BUILD.bazel", """exports_files(["runtime-root"])
filegroup(
    name = "runtime",
    srcs = glob(["usr/**", "etc/**", "ms-playwright/**"]),
    visibility = ["//visibility:public"],
)
""")

_playwright_runtime = repository_rule(
    implementation = _playwright_runtime_impl,
    attrs = {
        "fonts": attr.label(mandatory = True, allow_single_file = True),
        "digest": attr.string(mandatory = True),
        "extractor": attr.label(default = "//tooling/bazel:extract_playwright.py", allow_single_file = True),
    },
)

def _playwright_impl(ctx):
    lock = json.decode(ctx.read(Label("//clients/web:package-lock.json")))
    version = lock["packages"]["node_modules/@playwright/test"]["version"]
    url = "https://mcr.microsoft.com/v2/playwright/manifests/v%s-noble" % version
    digest = ctx.facts.get(url)
    if not digest:
        digest = ctx.download(url = url, output = "playwright-index.json", headers = {"Accept": "application/vnd.oci.image.index.v1+json, application/vnd.oci.image.manifest.v1+json"}).sha256
    font_version = DEJAVU_RELEASE[len("version_"):].replace("_", ".")
    font_prefix = "dejavu-fonts-ttf-" + font_version
    font_url = "https://github.com/dejavu-fonts/dejavu-fonts/releases/download/%s/%s.tar.bz2" % (DEJAVU_RELEASE, font_prefix)
    font_digest = ctx.facts.get(font_url)
    if not font_digest:
        font_digest = ctx.download(url = font_url, output = "fonts.tar.bz2").sha256
    http_archive(
        name = "dejavu_fonts",
        build_file_content = 'exports_files(["LICENSE"])',
        sha256 = font_digest,
        strip_prefix = font_prefix,
        urls = [font_url],
    )
    _playwright_runtime(
        name = "playwright_runtime",
        digest = digest,
        fonts = "@dejavu_fonts//:LICENSE",
    )
    return ctx.extension_metadata(
        root_module_direct_deps = ["playwright_runtime"],
        root_module_direct_dev_deps = [],
        facts = {url: digest, font_url: font_digest},
    )

playwright = module_extension(implementation = _playwright_impl)
