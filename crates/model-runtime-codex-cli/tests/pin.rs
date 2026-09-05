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
const PIN_MANIFEST: &str = "tooling/codex-cli/package.json";

/// The pinned package, whose executable this adapter spawns.
const PIN_PACKAGE: &str = "@openai/codex";

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
        "{PIN_MANIFEST} must pin {PIN_PACKAGE} at an exact release \
         version, not the range, tag, or alias {pinned}"
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
