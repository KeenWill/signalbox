//! The Codex CLI installation manifest keeps an exact version pin.
//!
//! This check is offline and unconditional, so it runs in the ordinary Rust
//! workflow on every pull request. `build.rs` derives the adapter's exported
//! supported-version marker from this manifest, while the binding smoke
//! verifies the installed executable. The smoke proves a live exchange, not
//! that the offline fixture corpus still represents the current CLI event
//! shapes; fixture regeneration or validation against the installed CLI would
//! close that residual gap.

#![allow(
    clippy::expect_used,
    clippy::panic,
    reason = "this standalone integration-test crate uses assertion panics and explicit fixture expectations; the workspace gate remains active for production targets"
)]

/// The pin manifest, relative to the workspace root.
const PIN_MANIFEST: &str = "tooling/codex-cli/release.json";

/// A range, tag, or alias would let the installed executable drift away from
/// the version the fixtures cover while this manifest still looked current.
#[test]
fn the_pin_manifest_uses_an_exact_version() {
    let manifest = read_pin_manifest();
    let pinned = manifest
        .get("release")
        .and_then(serde_json::Value::as_str)
        .unwrap_or_else(|| panic!("{PIN_MANIFEST} declares a fork release tag"));
    let executable_release = manifest
        .get("executableRelease")
        .and_then(serde_json::Value::as_str)
        .unwrap_or_else(|| panic!("{PIN_MANIFEST} declares an executable fork release tag"));

    assert_eq!(
        pinned, executable_release,
        "the archive and executable pins must use one release"
    );
    assert_eq!(
        version_pin::upstream_version(pinned),
        Some(signalbox_model_runtime_codex_cli::SUPPORTED_CODEX_CLI_VERSION),
        "the installed release and adapter must agree on the binary version"
    );
}

#[path = "../version_pin.rs"]
mod version_pin;
use version_pin::is_exact_pin;

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
fn exact_pin_rejects_a_prerelease() {
    assert!(!is_exact_pin("0.145.0-beta.1"));
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

#[test]
fn exact_pin_rejects_build_metadata() {
    assert!(!is_exact_pin("0.145.0+build.1"));
}

#[test]
fn fork_revisions_preserve_the_upstream_binary_version() {
    assert_eq!(
        version_pin::upstream_version("rust-v1.2.3-fork.12"),
        Some("1.2.3")
    );
}

#[test]
fn fork_pin_rejects_branch_names_and_inexact_releases() {
    for tag in [
        "fork",
        "rust-v^1.2.3-fork.1",
        "rust-v1.2.3-fork.0",
        "rust-v1.2.3-fork.latest",
        "rust-v1.2.3-signalbox.1",
        "rust-v1.2.3",
    ] {
        assert_eq!(version_pin::upstream_version(tag), None);
    }
}
