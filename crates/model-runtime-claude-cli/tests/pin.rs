//! Offline checks keeping package metadata and protocol constant on one pin.

use signalbox_model_runtime_claude_cli::SUPPORTED_CLAUDE_CLI_VERSION;

const PACKAGE_NAME: &str = "@anthropic-ai/claude-code";

#[test]
fn manifest_and_lockfile_pin_the_supported_cli_exactly() {
    let manifest: serde_json::Value = serde_json::from_str(include_str!("../package.json"))
        .expect("package manifest is valid JSON");
    let lock: serde_json::Value = serde_json::from_str(include_str!("../package-lock.json"))
        .expect("package lock is valid JSON");

    assert_eq!(
        manifest["dependencies"][PACKAGE_NAME],
        SUPPORTED_CLAUDE_CLI_VERSION
    );
    assert_eq!(
        lock["packages"]["node_modules/@anthropic-ai/claude-code"]["version"],
        SUPPORTED_CLAUDE_CLI_VERSION
    );
    assert_eq!(
        lock["packages"][""]["dependencies"][PACKAGE_NAME],
        SUPPORTED_CLAUDE_CLI_VERSION
    );
}
