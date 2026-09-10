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

#[test]
fn exact_pin_rejects_build_metadata() {
    assert!(!is_exact_pin("0.145.0+build.1"));
}

#[test]
fn fork_revisions_preserve_the_upstream_binary_version() {
    assert_eq!(
        version_pin::upstream_version("rust-v1.2.3-signalbox.12"),
        Some("1.2.3")
    );
}

#[test]
fn fork_pin_rejects_branch_names_and_inexact_releases() {
    for tag in [
        "signalbox",
        "rust-v^1.2.3-signalbox.1",
        "rust-v1.2.3-signalbox.0",
        "rust-v1.2.3-signalbox.latest",
        "rust-v1.2.3",
    ] {
        assert_eq!(version_pin::upstream_version(tag), None);
    }
}
