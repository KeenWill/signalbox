//! The Claude Code CLI installation manifest keeps an exact version pin.
//!
//! This check is offline and unconditional, so it runs in the ordinary Rust
//! workflow on every pull request. `build.rs` derives the adapter's exported
//! supported-version marker from this manifest, so a manifest-versus-constant
//! comparison could no longer fail; what the manifest cannot state on its own
//! is that the committed lockfile installs the same version, and that is what
//! these assertions keep. The compatibility smoke verifies the installed
//! executable. That smoke proves a live exchange, not that the offline fixture
//! corpus still represents the current CLI event shapes; fixture regeneration
//! or validation against the installed CLI would close that residual gap.

#![allow(
    clippy::expect_used,
    clippy::panic,
    reason = "this standalone integration-test crate uses assertion panics and explicit fixture expectations; the workspace gate remains active for production targets"
)]

/// The pinned package, whose executable this adapter spawns.
const PIN_PACKAGE: &str = "@anthropic-ai/claude-code";

/// The lockfile path of the installed package, which npm keys by install
/// location rather than by package name.
const LOCK_INSTALL_PATH: &str = "node_modules/@anthropic-ai/claude-code";

/// A range, tag, or alias would let the installed executable drift away from
/// the version the fixtures cover while this manifest still looked current.
#[test]
fn the_pin_manifest_uses_an_exact_version() {
    let pinned = manifest_dependency();

    assert!(
        is_exact_pin(&pinned),
        "package.json must pin {PIN_PACKAGE} at an exact `major.minor.patch` \
         version, not the range, tag, or alias {pinned}"
    );
}

/// Renovate maintains the manifest and the lockfile together, and only the
/// manifest reaches the build script. A lockfile that installed a different
/// version would put an executable the adapter does not claim to support on
/// every machine that runs `npm ci`, while the derived constant kept reporting
/// the manifest's version.
#[test]
fn the_lockfile_installs_the_manifest_pin() {
    let lock = read_lockfile();
    let pinned = manifest_dependency();

    assert_eq!(
        lock["packages"][LOCK_INSTALL_PATH]["version"], pinned,
        "the lockfile installs a different {PIN_PACKAGE} version than package.json pins"
    );
}

/// The lockfile also restates the root manifest's dependency range; a stale
/// copy here is the shape npm reconciles by resolving a different version.
#[test]
fn the_lockfile_root_dependency_matches_the_manifest() {
    let lock = read_lockfile();
    let pinned = manifest_dependency();

    assert_eq!(
        lock["packages"][""]["dependencies"][PIN_PACKAGE], pinned,
        "the lockfile's root dependency entry contradicts package.json"
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
    assert!(is_exact_pin("2.1.220"));
}

#[test]
fn exact_pin_rejects_a_caret_range() {
    assert!(!is_exact_pin("^2.1.220"));
}

#[test]
fn exact_pin_rejects_a_tilde_range() {
    assert!(!is_exact_pin("~2.1.220"));
}

#[test]
fn exact_pin_rejects_a_dist_tag() {
    assert!(!is_exact_pin("latest"));
}

#[test]
fn exact_pin_rejects_too_few_components() {
    assert!(!is_exact_pin("2.1"));
}

#[test]
fn exact_pin_rejects_too_many_components() {
    assert!(!is_exact_pin("2.1.220.1"));
}

#[test]
fn exact_pin_rejects_an_empty_component() {
    assert!(!is_exact_pin("2..220"));
}

#[test]
fn exact_pin_rejects_a_prerelease_suffix() {
    assert!(!is_exact_pin("2.1.220-beta"));
}

/// The manifest's declared dependency version, as the single value every
/// assertion above compares against — so no assertion restates a literal the
/// manifest already carries.
fn manifest_dependency() -> String {
    let manifest: serde_json::Value =
        serde_json::from_str(include_str!("../package.json")).expect("package.json is valid JSON");
    manifest["dependencies"][PIN_PACKAGE]
        .as_str()
        .unwrap_or_else(|| panic!("package.json declares a {PIN_PACKAGE} dependency"))
        .to_string()
}

fn read_lockfile() -> serde_json::Value {
    serde_json::from_str(include_str!("../package-lock.json"))
        .expect("package-lock.json is valid JSON")
}

/// Each of these built-ins reaches past the process the adapter owns — other
/// Claude Code sessions on the host, the user directly, a shell, the wider
/// filesystem, or an ambient instruction surface — so a deny list that stopped
/// naming one would widen what a daemon-driven session can do.
#[test]
fn the_deny_list_names_every_escape_capable_builtin() {
    let denied = signalbox_model_runtime_claude_cli::DISABLED_CLAUDE_CLI_BUILTIN_TOOLS;

    assert!(
        denied.contains(&"ListAgents"),
        "ListAgents must stay denied"
    );
    assert!(
        denied.contains(&"SendMessage"),
        "SendMessage must stay denied"
    );
    assert!(
        denied.contains(&"SendUserMessage"),
        "SendUserMessage must stay denied"
    );
    assert!(denied.contains(&"REPL"), "REPL must stay denied");
    assert!(
        denied.contains(&"PowerShell"),
        "PowerShell must stay denied"
    );
    assert!(denied.contains(&"Skill"), "Skill must stay denied");
    assert!(denied.contains(&"Glob"), "Glob must stay denied");
    assert!(denied.contains(&"Grep"), "Grep must stay denied");
}
