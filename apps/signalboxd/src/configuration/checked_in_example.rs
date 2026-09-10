use std::path::{Path, PathBuf};

use super::{
    EXAMPLE_EXEC_SUPERVISOR, HubModelConfiguration, ModelAdapter, checked_in_example_configuration,
};

fn example_path() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../../config/signalboxd.example.toml")
}

/// The checked-in operator example is the one configuration document a
/// deployment is invited to copy. Its installation-specific supervisor
/// path is replaced by an existing fixture file before the same fail-closed
/// loader validates every other byte.
#[test]
fn the_example_catalog_parses_and_validates() {
    checked_in_example_configuration()
        .expect("the checked-in example catalog is a valid version 1 document");
}

#[test]
fn remaining_daemon_examples_parse_and_validate() {
    let temporary = tempfile::tempdir().expect("fixture directory is available");
    let executable = std::env::current_exe().expect("the test executable has a path");
    let executable = executable.to_string_lossy();
    let working_directory = temporary.path().to_string_lossy();
    std::fs::write(temporary.path().join("credential-marker"), "synthetic")
        .expect("fixture credential home is nonempty");
    let approval_judge = include_str!("../../../../config/approval-judge-eval-codex.example.toml")
        .replace("/opt/signalbox/bin/codex", &executable)
        .replace("/var/lib/signalbox/codex-eval", &working_directory)
        .replace("/var/lib/signalbox/codex-home", &working_directory);
    HubModelConfiguration::parse(&approval_judge)
        .expect("the approval-judge example is a valid daemon configuration");

    let catalog =
        std::fs::read_to_string(example_path()).expect("the checked-in daemon example is readable");
    let rules = include_str!("../../../../config/repository-watch-rules.example.toml");
    let repository_watch = format!(
        r#"{catalog}

[repository_watch]
version = 1
workflows_enabled = true
signal_reviewers = []

[[repository_watch.repositories]]
repository = "KeenWill/signalbox"
poll_interval_seconds = 60
credential_file = "/run/credentials/repository-watch-token"

{rules}"#
    )
    .replace(EXAMPLE_EXEC_SUPERVISOR, &executable);
    HubModelConfiguration::parse(&repository_watch)
        .expect("the repository-watch rules example is valid daemon configuration");
}

/// Whether one line is commented-out configuration rather than active TOML.
fn is_inactive(line: &str) -> bool {
    line == "#" || line.starts_with("# ")
}

/// Removes exactly one comment level, the edit a reader makes by hand.
fn uncomment(line: &str) -> &str {
    if let Some(remainder) = line.strip_prefix("# ") {
        return remainder;
    }
    if let Some(remainder) = line.strip_prefix('#') {
        return remainder;
    }
    line
}

/// Activates every inactive block the example ships, the transformation an
/// operator performs when adopting a family the shipped document leaves
/// commented out.
///
/// An inactive block is a run of consecutive commented lines whose first
/// line uncomments to a TOML table header. A run that opens with prose is
/// documentation, not configuration, and stays commented; prose written
/// inside an inactive block is doubly commented so that one level of
/// removal leaves it a comment.
fn activated(document: &str) -> String {
    let lines: Vec<&str> = document.lines().collect();
    let mut activated = String::with_capacity(document.len());
    let mut index = 0;
    while index < lines.len() {
        let mut end = index;
        while end < lines.len() && is_inactive(lines[end]) {
            end += 1;
        }
        let opens_a_table = end > index && uncomment(lines[index]).starts_with('[');
        while index < end {
            activated.push_str(if opens_a_table {
                uncomment(lines[index])
            } else {
                lines[index]
            });
            activated.push('\n');
            index += 1;
        }
        if index < lines.len() && !is_inactive(lines[index]) {
            activated.push_str(lines[index]);
            activated.push('\n');
            index += 1;
        }
    }
    activated
}

/// Binds the example's documented placeholder paths to paths that exist,
/// which the wrapped-CLI process tables require of a real deployment.
///
/// The bridge is not a placeholder: the example names it the way a
/// deployment that installs it on the daemon's own search path does, which
/// this test process is not, so the whole assignment is rebound to a path.
fn with_existing_paths(document: &str, executable: &Path, working_directory: &Path) -> String {
    let executable = executable.to_string_lossy();
    let working_directory = working_directory.to_string_lossy();
    document
        .replace(CLAUDE_EXECUTABLE_PLACEHOLDER, &executable)
        .replace(
            &bridge_assignment(CLAUDE_BRIDGE_NAME),
            &bridge_assignment(&executable),
        )
        .replace(CODEX_EXECUTABLE_PLACEHOLDER, &executable)
        .replace(EXAMPLE_EXEC_SUPERVISOR, &executable)
        .replace(WORKING_DIRECTORY_PLACEHOLDER, &working_directory)
}

fn bridge_assignment(value: &str) -> String {
    format!("mcp_bridge_executable = \"{value}\"")
}

const CLAUDE_EXECUTABLE_PLACEHOLDER: &str = "/absolute/path/to/claude";
const CLAUDE_BRIDGE_NAME: &str = "signalbox-claude-mcp-bridge";
const CODEX_EXECUTABLE_PLACEHOLDER: &str = "/absolute/path/to/codex";
const WORKING_DIRECTORY_PLACEHOLDER: &str = "/absolute/path/to/workspace";

#[test]
fn activation_uncomments_a_block_that_opens_a_table() {
    let activated = activated("# [codex_cli]\n# executable = \"codex\"\n");

    assert_eq!(activated, "[codex_cli]\nexecutable = \"codex\"\n");
}

#[test]
fn activation_leaves_a_block_that_opens_with_prose_commented() {
    let activated = activated("# To serve it, add:\n# [codex_cli]\n");

    assert_eq!(activated, "# To serve it, add:\n# [codex_cli]\n");
}

#[test]
fn activation_leaves_prose_inside_an_activated_block_commented() {
    let activated = activated("# [[models]]\n# # Hard cap: 128000.\n");

    assert_eq!(activated, "[[models]]\n# Hard cap: 128000.\n");
}

#[test]
fn activation_leaves_active_lines_unchanged() {
    let activated = activated("version = 1\n\n[compaction]\n");

    assert_eq!(activated, "version = 1\n\n[compaction]\n");
}

/// A commented model row that activation walks past would be catalog data
/// no test ever loads, so the example is written so that activation
/// consumes every one of them.
#[test]
fn activation_leaves_no_commented_model_row_behind() {
    let document =
        std::fs::read_to_string(example_path()).expect("the checked-in example is readable");

    let activated = activated(&document);

    assert!(!activated.contains("\n# [[models]]"));
}

/// Every model row the example ships for a family it leaves commented out
/// is catalog data an operator is invited to adopt by uncommenting it, so
/// the whole activated document must satisfy the same fail-closed loader —
/// including the adapter-specific reasoning levels and service tiers each
/// row declares, and the one-adapter-per-provider-model rule that a
/// spelling offered on two surfaces would violate.
#[test]
fn every_inactive_example_block_activates_into_a_valid_catalog() {
    let temporary = tempfile::tempdir().expect("fixture directory is available");
    let executable = std::env::current_exe().expect("the test executable has a path");
    let document =
        std::fs::read_to_string(example_path()).expect("the checked-in example is readable");

    let activated = with_existing_paths(&activated(&document), &executable, temporary.path());

    let configuration = HubModelConfiguration::parse(&activated)
        .expect("the example's commented families activate into a valid version 1 document");

    assert_eq!(
        configuration.adapter_for_provider_model("claude-fable-5"),
        Some(ModelAdapter::Anthropic)
    );
    assert_eq!(
        configuration.adapter_for_provider_model("gpt-5.6"),
        Some(ModelAdapter::OpenAi)
    );
    assert_eq!(
        configuration.adapter_for_provider_model("gpt-5.6-sol"),
        Some(ModelAdapter::CodexCli)
    );
    assert_eq!(
        configuration.adapter_for_provider_model("fable"),
        Some(ModelAdapter::ClaudeCli)
    );
}
