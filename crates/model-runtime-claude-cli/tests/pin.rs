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
//!
//! The pin also carries a security obligation, which the second group of
//! assertions keeps: the version selects which built-in tools the executable
//! can expose, and the adapter's `--disallowedTools` inventory must have been
//! reconciled against that exact version. Reconciliation is a human act
//! performed against the installed executable, so what an offline test can
//! prove is that it happened for the version now pinned — which is enough to
//! stop a dependency bump from silently widening the built-in surface a
//! daemon-driven session can reach.

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

/// The built-in surface is a property of the pinned executable, so the pin and
/// the inventory move together or the inventory is stale. Upstream 2.1.248
/// widened the cross-session tools to configurations that had not carried them,
/// and this repository learned of it from a reviewer rather than from a failing
/// check; this assertion is that missing check. A bump therefore stays red
/// until someone re-reads the installed executable's reported tool sets and
/// advances the marker, which is the point — the reconciliation is the work,
/// and the marker is only its receipt.
#[test]
fn the_builtin_inventory_is_reconciled_with_the_pin() {
    assert_eq!(
        signalbox_model_runtime_claude_cli::RECONCILED_CLAUDE_CLI_BUILTIN_INVENTORY_VERSION,
        signalbox_model_runtime_claude_cli::SUPPORTED_CLAUDE_CLI_VERSION,
        "the disallowed-built-in inventory was last reconciled against an older \
         Claude Code CLI than this manifest now pins; re-read the installed \
         executable's reported built-in tools, extend \
         DISABLED_CLAUDE_CLI_BUILTIN_TOOLS with anything new, and advance \
         RECONCILED_CLAUDE_CLI_BUILTIN_INVENTORY_VERSION"
    );
}

/// Cross-session discovery is the built-in whose absence from the inventory
/// prompted this test: it lets one session enumerate the other Claude Code
/// sessions on the host, which is reach outside the box a daemon-driven session
/// never has.
#[test]
fn the_builtin_inventory_denies_cross_session_discovery() {
    assert!(
        signalbox_model_runtime_claude_cli::DISABLED_CLAUDE_CLI_BUILTIN_TOOLS
            .contains(&"ListAgents"),
        "ListAgents enumerates other Claude Code sessions on this host and must \
         stay denied"
    );
}

/// Cross-session messaging is the other half of that surface: discovery names
/// the neighbours, messaging talks to them, and denying one without the other
/// would leave the reach intact.
#[test]
fn the_builtin_inventory_denies_cross_session_messaging() {
    assert!(
        signalbox_model_runtime_claude_cli::DISABLED_CLAUDE_CLI_BUILTIN_TOOLS
            .contains(&"SendMessage"),
        "SendMessage delivers to other Claude Code sessions on this host and \
         must stay denied"
    );
}

/// A reconciliation appends, and an append is where a name gets added twice or
/// dropped into the wrong place. The CLI reports `Task` first and the rest
/// alphabetically, and holding the inventory to that shape is what makes the
/// next append reviewable as a diff rather than a re-reading of the whole list.
#[test]
fn the_builtin_inventory_keeps_the_order_the_cli_reports() {
    let reported_tail = &signalbox_model_runtime_claude_cli::DISABLED_CLAUDE_CLI_BUILTIN_TOOLS[1..];
    let mut sorted_tail = reported_tail.to_vec();
    sorted_tail.sort_unstable();

    assert_eq!(
        reported_tail, sorted_tail,
        "after the leading Task the inventory follows the CLI's own alphabetical \
         reporting order; an entry added out of place is an unreviewed append"
    );
}

/// Naming a built-in twice reads as two decisions and hides that one of them
/// was never made; the CLI accepts a repeated name silently, so nothing else
/// would catch it.
#[test]
fn the_builtin_inventory_names_each_builtin_once() {
    let inventory = signalbox_model_runtime_claude_cli::DISABLED_CLAUDE_CLI_BUILTIN_TOOLS;
    let distinct = inventory
        .iter()
        .copied()
        .collect::<std::collections::BTreeSet<&str>>();

    assert_eq!(
        distinct.len(),
        inventory.len(),
        "the disallowed-built-in inventory names a built-in more than once"
    );
}
