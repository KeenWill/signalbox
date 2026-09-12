"""Run Linux test binaries with the native toolchain's declared runtime."""

load(":linux_runtime.bzl", "linux_runtime")

def _runfile_path(file):
    if not file.short_path.startswith("../"):
        fail("The native runtime must come from an external toolchain")
    return file.short_path[3:]

def _native_runtime_impl(ctx):
    files = ctx.attr.libraries[DefaultInfo].files.to_list()
    runtime = linux_runtime(files)
    directories = sorted({
        _runfile_path(file).rsplit("/", 1)[0]: True
        for file in runtime.libraries
    }.keys())
    executable = ctx.actions.declare_file(ctx.label.name + ".sh")
    ctx.actions.write(
        executable,
        "\n".join([
            "#!/bin/bash",
            "set -euo pipefail",
            # Repository fixtures validate ancestry through the filesystem root.
            # The runner's /tmp can be a separate mount.
            'export TMPDIR=/var/tmp',
            # Script launchers already select their declared interpreter.
            'IFS= read -r -n 4 magic < "$1" || true',
            'if [[ "$magic" != $\'\\x7fELF\' ]]; then exec "$@"; fi',
            'if [[ -n "${COVERAGE_DIR:-}" ]]; then',
            '  mkdir -p "${TEST_TMPDIR:?}/coverage-runtime"',
        ] + [
            '  ln -sfn "${RUNFILES_DIR}/%s" "${TEST_TMPDIR}/coverage-runtime/%s"' % (_runfile_path(file), file.basename)
            for file in runtime.libraries
        ] + [
            "fi",
            "patch_binary() {",
            # Bubblewrap's host profile exposes system libraries, not Bazel runfiles.
            '  if [[ -n "${SIGNALBOX_RUN_BWRAP_INTEGRATION:-}" ]]; then',
            '    "${RUNFILES_DIR}/%s" --set-interpreter /lib64/ld-linux-x86-64.so.2 --remove-rpath --output "$2" "$1"' % _runfile_path(ctx.file.patchelf),
            "    return",
            "  fi",
            '  if [[ -n "${COVERAGE_DIR:-}" ]]; then',
            '    "${RUNFILES_DIR}/%s" --set-interpreter "${RUNFILES_DIR}/%s" --output "$2" "$1"' % (
                _runfile_path(ctx.file.patchelf),
                _runfile_path(runtime.loader),
            ),
            "    return",
            "  fi",
            '  "${RUNFILES_DIR:?}/%s" --set-interpreter "${RUNFILES_DIR}/%s" --set-rpath "%s" --output "$2" "$1"' % (
                _runfile_path(ctx.file.patchelf),
                _runfile_path(runtime.loader),
                ":".join(["${RUNFILES_DIR}/" + directory for directory in directories]),
            ),
            "}",
            # Helper processes may clear their environment, so give them an ELF
            # with its runtime encoded instead of an environment-dependent script.
            "while IFS= read -r variable; do",
            '  patched_helper="${TEST_TMPDIR:?}/${variable}.${BASHPID}"',
            '  patch_binary "${!variable}" "$patched_helper"',
            '  export "$variable=$patched_helper"',
            "done < <(compgen -A variable NEXTEST_BIN_EXE_ || true)",
            # Executing the loader directly makes current_exe() return the
            # loader, breaking tests that spawn another copy of themselves.
            'patched_executable="${TEST_TMPDIR:?}/${1##*/}.${BASHPID}"',
            'patch_binary "$1" "$patched_executable"',
            "shift",
            'if [[ -n "${SIGNALBOX_TEST_MANIFEST:-}" ]]; then',
            '  manifest="$(readlink -f "$SIGNALBOX_TEST_MANIFEST")"',
            '  export CARGO_MANIFEST_DIR="${manifest%/*}"',
            '  export CARGO_TARGET_DIR="${TEST_TMPDIR}/cargo-target"',
            '  cd "${CARGO_MANIFEST_DIR}"',
            "fi",
            # CI allocates one worker per manifest shard; local Bazel schedules
            # the declared shard_count itself.
            'if [[ -n "${SIGNALBOX_TEST_TOTAL_SHARDS:-}" ]]; then',
            '  export TEST_TOTAL_SHARDS="${SIGNALBOX_TEST_TOTAL_SHARDS}"',
            '  export TEST_SHARD_INDEX="${SIGNALBOX_TEST_SHARD_INDEX:?}"',
            "fi",
            "if (( ${TEST_TOTAL_SHARDS:-0} > 1 )); then",
            '  if [[ -n "${TEST_SHARD_STATUS_FILE:-}" ]]; then touch "$TEST_SHARD_STATUS_FILE"; fi',
            '  "$patched_executable" "$@" --list > "${TEST_TMPDIR}/test-list"',
            "  selected=()",
            "  index=0",
            "  while IFS= read -r line; do",
            '    [[ "$line" == *": test" ]] || continue',
            "    if (( index % TEST_TOTAL_SHARDS == TEST_SHARD_INDEX )); then",
            '      selected+=("${line%: test}")',
            "    fi",
            "    index=$((index + 1))",
            '  done < "${TEST_TMPDIR}/test-list"',
            "  (( ${#selected[@]} )) || exit 0",
            '  set -- "$@" --exact "${selected[@]}"',
            "fi",
            # Bazel reaps detached descendants when its command exits. Finish
            # Docker cleanup while this wrapper is still alive.
            'if [[ "${SIGNALBOX_TEST_SHARED_POSTGRES:-}" == "1" ]]; then',
            '  SIGNALBOX_TEST_POSTGRES_CONTAINERS="$(mktemp -d "${TEST_TMPDIR}/postgres-containers.XXXXXXXX")"',
            "  export SIGNALBOX_TEST_POSTGRES_CONTAINERS",
            "  cleanup_postgres() {",
            "    status=$?",
            '    for record in "${SIGNALBOX_TEST_POSTGRES_CONTAINERS}"/*; do',
            '      [[ -f "$record" ]] || continue',
            '      IFS= read -r container < "$record"',
            '      if ! docker rm -f -v "$container" > /dev/null; then',
            "        if (( status == 0 )); then status=1; fi",
            "      fi",
            "    done",
            '    exit "$status"',
            "  }",
            "  trap cleanup_postgres EXIT",
            '  trap "exit 143" TERM',
            '  trap "exit 130" INT',
            '  "$patched_executable" "$@"',
            "else",
            '  exec "$patched_executable" "$@"',
            "fi",
            "",
        ]),
        is_executable = True,
    )
    return [DefaultInfo(
        executable = executable,
        runfiles = ctx.runfiles(files = files + [ctx.file.patchelf]),
    )]

native_runtime = rule(
    implementation = _native_runtime_impl,
    executable = True,
    attrs = {
        "libraries": attr.label(mandatory = True),
        "patchelf": attr.label(mandatory = True, allow_single_file = True),
    },
)
