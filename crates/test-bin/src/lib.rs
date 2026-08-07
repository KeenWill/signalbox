//! Locate a workspace binary from an integration test, relocatably.
//!
//! Cargo hands an integration test the absolute path of every binary its own
//! package builds through the compile-time `CARGO_BIN_EXE_<name>` variable, and
//! `env!` bakes that path into the test executable as a string literal. That is
//! correct for `cargo test`, where the test binary only ever runs on the machine
//! and in the target directory that built it. It is wrong the moment the test
//! binary is moved: a [`cargo nextest archive`] built in CI's build job and
//! extracted on a shard runner still carries the build job's `target/` path, and
//! every spawn of a companion binary fails with `ENOENT`.
//!
//! nextest solves this by re-exporting the same information as a *runtime*
//! variable, `NEXTEST_BIN_EXE_<name>`, pointing at the extracted copy. It cannot
//! rewrite `env!`, because that expansion already happened. So the resolution
//! order has to be runtime-first, and [`test_bin_path!`] is the one place that
//! order is written down:
//!
//! 1. `NEXTEST_BIN_EXE_<name>` with hyphens folded to underscores — the form
//!    nextest recommends, because some shells and debuggers drop environment
//!    variables whose names contain hyphens,
//! 2. `NEXTEST_BIN_EXE_<name>` verbatim, which nextest also sets,
//! 3. the compile-time `CARGO_BIN_EXE_<name>`, which is what plain `cargo test`
//!    runs on and is the only form available when nextest is not the runner.
//!
//! Step 3 is a fallback, not a deprecation: outside nextest it is the only
//! answer, and it keeps `cargo test` working unchanged for local development.
//!
//! Why a macro and not a function: `CARGO_BIN_EXE_<name>` is set by Cargo only
//! while compiling the integration tests of the package that declares the
//! binary, so `env!` has to expand at the call site. A function in this crate
//! would read this crate's environment, where the variable does not exist.
//!
//! [`cargo nextest archive`]: https://nexte.st/docs/ci-features/archiving/

use std::path::{Path, PathBuf};

/// Resolve one binary path from the runtime environment, or the compiled path.
///
/// Callers reach this through [`test_bin_path!`], which supplies `compiled`
/// from its own expansion of `env!`. It is public only because a macro's
/// expansion is compiled in the caller's crate.
///
/// The two arguments are a target name and a path, and they carry different
/// types so that neither can be passed for the other: as two `&str` they would
/// transpose silently, and a transposed call resolves the wrong variable and
/// then falls back to a path that is really a binary name.
///
/// An environment variable that is present but empty is treated as absent: an
/// empty path can only spawn as a failure whose message names nothing, and the
/// compiled path is a strictly better answer than that.
#[doc(hidden)]
pub fn resolve(name: &str, compiled: &Path) -> PathBuf {
    let candidates = [
        format!("NEXTEST_BIN_EXE_{}", name.replace('-', "_")),
        format!("NEXTEST_BIN_EXE_{name}"),
    ];
    for variable in candidates {
        match std::env::var_os(variable) {
            Some(value) if !value.is_empty() => return PathBuf::from(value),
            _ => {}
        }
    }
    compiled.to_path_buf()
}

/// Return the path of one binary built by the calling test's own package.
///
/// The argument is the binary's Cargo target name, hyphens and all — the same
/// spelling `env!("CARGO_BIN_EXE_<name>")` takes. The result is a [`PathBuf`],
/// which every consumer here accepts: `Command::new`, `std::fs::copy`, and any
/// `impl AsRef<Path>` parameter.
///
/// ```ignore
/// let output = std::process::Command::new(signalbox_test_bin::test_bin_path!(
///     "signalbox-exec-supervisor"
/// ))
/// .output()?;
/// ```
///
/// The example is `ignore`d rather than run: it only compiles inside a package
/// that actually builds a binary of that name, which this dependency-free
/// support crate deliberately does not.
#[macro_export]
macro_rules! test_bin_path {
    ($name:literal) => {
        $crate::resolve(
            $name,
            ::std::path::Path::new(::core::env!(::core::concat!("CARGO_BIN_EXE_", $name))),
        )
    };
}
