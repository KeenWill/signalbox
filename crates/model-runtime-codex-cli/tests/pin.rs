//! The pinned Codex CLI version and the adapter's supported version agree.
//!
//! This check is offline and unconditional, so it runs in the ordinary Rust
//! workflow on every pull request. It is the drift trip-wire behind the
//! Renovate pin: a bump to `tooling/codex-cli/package.json` fails this test
//! until someone moves [`SUPPORTED_CODEX_CLI_VERSION`] with it, and moving
//! that constant is what forces the fixture corpus and the gated compatibility
//! smoke to be re-examined. Verifying the *installed* executable against the
//! pin is the smoke's job (`tests/live_smoke.rs`); a model dispatch never
//! spends a process on a version probe.

#![allow(
    clippy::expect_used,
    clippy::panic,
    reason = "this standalone integration-test crate uses assertion panics and explicit fixture expectations; the workspace gate remains active for production targets"
)]

use signalbox_model_runtime_codex_cli::SUPPORTED_CODEX_CLI_VERSION;

/// The pin manifest, relative to the workspace root.
const PIN_MANIFEST: &str = "tooling/codex-cli/package.json";

/// The pinned package, whose executable this adapter spawns.
const PIN_PACKAGE: &str = "@openai/codex";

#[test]
fn the_pin_manifest_names_the_supported_codex_cli_version() {
    let manifest = read_pin_manifest();
    let pinned = manifest
        .get("dependencies")
        .and_then(|dependencies| dependencies.get(PIN_PACKAGE))
        .and_then(serde_json::Value::as_str)
        .unwrap_or_else(|| panic!("{PIN_MANIFEST} declares a {PIN_PACKAGE} dependency"));

    assert_eq!(
        pinned, SUPPORTED_CODEX_CLI_VERSION,
        "{PIN_MANIFEST} pins {PIN_PACKAGE} at {pinned}, but this adapter's \
         fixtures and smoke cover {SUPPORTED_CODEX_CLI_VERSION}; move the \
         constant and re-verify the corpus, or revert the pin"
    );
}

/// A range, tag, or alias would let the installed executable drift away from
/// the version the fixtures cover while this manifest still looked current.
#[test]
fn the_pin_manifest_uses_an_exact_version() {
    let manifest = read_pin_manifest();
    let pinned = manifest
        .get("dependencies")
        .and_then(|dependencies| dependencies.get(PIN_PACKAGE))
        .and_then(serde_json::Value::as_str)
        .unwrap_or_else(|| panic!("{PIN_MANIFEST} declares a {PIN_PACKAGE} dependency"));

    assert!(
        pinned.split('.').all(|component| !component.is_empty()
            && component.bytes().all(|byte| byte.is_ascii_digit())),
        "{PIN_MANIFEST} must pin {PIN_PACKAGE} at an exact `major.minor.patch` \
         version, not the range or tag {pinned}"
    );
    assert_eq!(
        pinned.split('.').count(),
        3,
        "{PIN_MANIFEST} must pin {PIN_PACKAGE} at an exact `major.minor.patch` \
         version, not {pinned}"
    );
}

fn read_pin_manifest() -> serde_json::Value {
    let path = workspace_root().join(PIN_MANIFEST);
    let content = std::fs::read_to_string(&path)
        .unwrap_or_else(|error| panic!("{} is readable: {error}", path.display()));
    serde_json::from_str(&content)
        .unwrap_or_else(|error| panic!("{} is valid JSON: {error}", path.display()))
}

fn workspace_root() -> std::path::PathBuf {
    std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .ancestors()
        .nth(2)
        .expect("this crate sits two directories below the workspace root")
        .to_path_buf()
}
