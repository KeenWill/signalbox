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
        is_exact_pin(pinned),
        "{PIN_MANIFEST} must pin {PIN_PACKAGE} at an exact `major.minor.patch` \
         version, not the range, tag, or alias {pinned}"
    );
}

/// Whether `version` is an exact `major.minor.patch` pin: exactly three
/// dot-separated components, each a nonempty run of ASCII digits — so a range
/// (`^1.2.3`), tag (`latest`), alias, prerelease suffix, or wrong component
/// count is rejected. Factored out of the manifest test so focused fixtures
/// exercise both accepted and rejected shapes, not only the live manifest.
fn is_exact_pin(version: &str) -> bool {
    let components: Vec<&str> = version.split('.').collect();
    components.len() == 3
        && components
            .iter()
            .all(|component| !component.is_empty() && component.bytes().all(|b| b.is_ascii_digit()))
}

#[test]
fn exact_pin_accepts_major_minor_patch() {
    assert!(is_exact_pin("0.145.0"));
}

#[test]
fn exact_pin_rejects_a_caret_range() {
    assert!(!is_exact_pin("^0.145.0"));
}

#[test]
fn exact_pin_rejects_a_tilde_range() {
    assert!(!is_exact_pin("~0.145.0"));
}

#[test]
fn exact_pin_rejects_a_dist_tag() {
    assert!(!is_exact_pin("latest"));
}

#[test]
fn exact_pin_rejects_too_few_components() {
    assert!(!is_exact_pin("0.145"));
}

#[test]
fn exact_pin_rejects_too_many_components() {
    assert!(!is_exact_pin("0.145.0.1"));
}

#[test]
fn exact_pin_rejects_an_empty_component() {
    assert!(!is_exact_pin("0..0"));
}

#[test]
fn exact_pin_rejects_a_prerelease_suffix() {
    assert!(!is_exact_pin("0.145.0-beta"));
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
