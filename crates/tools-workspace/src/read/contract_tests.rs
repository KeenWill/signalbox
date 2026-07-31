use std::fs;

use serde_json::json;
use signalbox_domain::NormalizedToolArguments;
use signalbox_tool_contract::rendered_contract_schema;

use super::*;
use crate::LocalWorkspaceFileSystem;

fn arguments(value: String) -> NormalizedToolArguments {
    NormalizedToolArguments::try_from_provider_text(value).expect("fixture arguments are admitted")
}

fn fixture_executor(
    workspace: &tempfile::TempDir,
) -> WorkspaceReadExecutor<LocalWorkspaceFileSystem> {
    WorkspaceReadTools::try_new(LocalWorkspaceFileSystem, workspace.path())
        .expect("fixture tools construct")
        .into_parts()
        .1
}

#[test]
fn read_file_schema_carries_path_and_byte_bounds() {
    let schema = rendered_contract_schema::<ReadFileContract>();

    assert_eq!(
        schema["properties"]["path"]["maxLength"],
        json!(crate::path::MAX_WORKSPACE_PATH_BYTES)
    );
    assert_eq!(schema["properties"]["path"]["minLength"], json!(1));
    assert_eq!(
        schema["properties"]["max_bytes"]["maximum"],
        json!(MAX_READ_BYTES)
    );
    assert_eq!(schema["properties"]["max_bytes"]["minimum"], json!(1));
}

#[test]
fn glob_files_schema_carries_pattern_path_and_result_bounds() {
    let schema = rendered_contract_schema::<GlobFilesContract>();

    assert_eq!(
        schema["properties"]["pattern"]["maxLength"],
        json!(MAX_PATTERN_BYTES)
    );
    assert_eq!(
        schema["properties"]["path"]["maxLength"],
        json!(crate::path::MAX_WORKSPACE_PATH_BYTES)
    );
    assert_eq!(
        schema["properties"]["max_results"]["maximum"],
        json!(MAX_RESULTS)
    );
}

#[test]
fn read_file_at_exact_byte_cap_reports_complete() {
    const FILE_PATH: &str = "note.txt";
    const CONTENT: &str = "abcd";

    let workspace = tempfile::tempdir().expect("workspace fixture constructs");
    fs::write(workspace.path().join(FILE_PATH), CONTENT).expect("fixture file writes");
    let executor = fixture_executor(&workspace);
    let operation = decode_operation(
        ReadToolKind::ReadFile,
        &arguments(format!(
            r#"{{"max_bytes":{},"path":"{FILE_PATH}"}}"#,
            CONTENT.len()
        )),
        &executor.filesystem,
        &executor.root,
    )
    .expect("read arguments are valid");
    let ReadResult::ReadFile(result) = executor
        .execute_operation(operation)
        .expect("fixture read succeeds")
    else {
        panic!("read_file returns a read result")
    };

    assert_eq!(result.content, CONTENT);
    assert_eq!(result.bytes_read, CONTENT.len());
    assert_eq!(result.total_bytes, CONTENT.len() as u64);
    assert!(!result.truncated);
}

#[test]
fn search_at_exact_result_cap_reports_complete() {
    const FILE_PATH: &str = "one.rs";
    const MATCHING_CONTENT: &str = "fn only() {}\n";

    let workspace = tempfile::tempdir().expect("workspace fixture constructs");
    fs::write(workspace.path().join(FILE_PATH), MATCHING_CONTENT).expect("fixture file writes");
    let executor = fixture_executor(&workspace);
    let operation = decode_operation(
        ReadToolKind::SearchFiles,
        &arguments(String::from(
            r#"{"max_results":1,"path":".","pattern":"fn "}"#,
        )),
        &executor.filesystem,
        &executor.root,
    )
    .expect("search arguments are valid");
    let ReadResult::SearchFiles(result) = executor
        .execute_operation(operation)
        .expect("fixture search succeeds")
    else {
        panic!("search_files returns search matches")
    };

    assert_eq!(result.matches.len(), 1);
    assert_eq!(result.matches[0].path, FILE_PATH);
    assert!(!result.truncated);
}

#[test]
fn single_file_search_applies_glob_to_file_name() {
    const FILE_PATH: &str = "one.rs";
    const MATCHING_CONTENT: &str = "needle\n";

    let workspace = tempfile::tempdir().expect("workspace fixture constructs");
    fs::write(workspace.path().join(FILE_PATH), MATCHING_CONTENT).expect("fixture file writes");
    let executor = fixture_executor(&workspace);
    let operation = decode_operation(
        ReadToolKind::SearchFiles,
        &arguments(format!(
            r#"{{"glob":"*.rs","path":"{FILE_PATH}","pattern":"needle"}}"#
        )),
        &executor.filesystem,
        &executor.root,
    )
    .expect("search arguments are valid");
    let ReadResult::SearchFiles(result) = executor
        .execute_operation(operation)
        .expect("fixture search succeeds")
    else {
        panic!("search_files returns search matches")
    };

    assert_eq!(result.matches.len(), 1);
    assert_eq!(result.matches[0].path, FILE_PATH);
}

#[test]
fn search_result_cap_selects_lexically_first_file() {
    const FIRST_PATH: &str = "a.rs";
    const LATER_PATH: &str = "b.rs";
    const MATCHING_CONTENT: &str = "needle\n";

    let workspace = tempfile::tempdir().expect("workspace fixture constructs");
    fs::write(workspace.path().join(LATER_PATH), MATCHING_CONTENT).expect("fixture file writes");
    fs::write(workspace.path().join(FIRST_PATH), MATCHING_CONTENT).expect("fixture file writes");
    let executor = fixture_executor(&workspace);
    let operation = decode_operation(
        ReadToolKind::SearchFiles,
        &arguments(String::from(
            r#"{"max_results":1,"path":".","pattern":"needle"}"#,
        )),
        &executor.filesystem,
        &executor.root,
    )
    .expect("search arguments are valid");
    let ReadResult::SearchFiles(result) = executor
        .execute_operation(operation)
        .expect("fixture search succeeds")
    else {
        panic!("search_files returns search matches")
    };

    assert_eq!(result.matches.len(), 1);
    assert_eq!(result.matches[0].path, FIRST_PATH);
    assert!(result.truncated);
}

#[test]
fn directory_search_skips_binary_file_and_reports_omission() {
    const BINARY_PATH: &str = "a.bin";
    const MATCH_PATH: &str = "b.txt";
    const MATCHING_CONTENT: &str = "needle\n";

    let workspace = tempfile::tempdir().expect("workspace fixture constructs");
    fs::write(workspace.path().join(BINARY_PATH), [0xff]).expect("binary fixture writes");
    fs::write(workspace.path().join(MATCH_PATH), MATCHING_CONTENT).expect("text fixture writes");
    let executor = fixture_executor(&workspace);
    let operation = decode_operation(
        ReadToolKind::SearchFiles,
        &arguments(String::from(r#"{"path":".","pattern":"needle"}"#)),
        &executor.filesystem,
        &executor.root,
    )
    .expect("search arguments are valid");
    let ReadResult::SearchFiles(result) = executor
        .execute_operation(operation)
        .expect("directory search skips binary file")
    else {
        panic!("search_files returns search matches")
    };

    assert_eq!(result.matches.len(), 1);
    assert_eq!(result.matches[0].path, MATCH_PATH);
    assert!(result.truncated);
}

#[test]
fn single_file_search_rejects_binary_content() {
    const BINARY_PATH: &str = "binary.bin";

    let workspace = tempfile::tempdir().expect("workspace fixture constructs");
    fs::write(workspace.path().join(BINARY_PATH), [0xff]).expect("binary fixture writes");
    let executor = fixture_executor(&workspace);
    let operation = decode_operation(
        ReadToolKind::SearchFiles,
        &arguments(format!(r#"{{"path":"{BINARY_PATH}","pattern":"needle"}}"#)),
        &executor.filesystem,
        &executor.root,
    )
    .expect("search arguments are valid");

    let result = executor.execute_operation(operation);
    let Err(error) = result else {
        panic!("single binary file is a strict search failure")
    };

    assert_eq!(error, ReadFailure::NotUtf8);
}

#[test]
fn directory_search_stops_at_aggregate_byte_budget() {
    const FIRST_PATH: &str = "a.txt";
    const SECOND_PATH: &str = "b.txt";
    const THIRD_PATH: &str = "c.txt";
    const FOURTH_PATH: &str = "d.txt";
    const OMITTED_PATH: &str = "e.txt";
    const NEEDLE: &str = "needle";

    let filler = "x".repeat(MAX_SEARCH_FILE_BYTES + 1);
    let workspace = tempfile::tempdir().expect("workspace fixture constructs");
    fs::write(workspace.path().join(FIRST_PATH), &filler).expect("first fixture writes");
    fs::write(workspace.path().join(SECOND_PATH), &filler).expect("second fixture writes");
    fs::write(workspace.path().join(THIRD_PATH), &filler).expect("third fixture writes");
    fs::write(workspace.path().join(FOURTH_PATH), &filler).expect("fourth fixture writes");
    fs::write(
        workspace.path().join(OMITTED_PATH),
        format!("{NEEDLE}{filler}"),
    )
    .expect("omitted fixture writes");
    let executor = fixture_executor(&workspace);
    let operation = decode_operation(
        ReadToolKind::SearchFiles,
        &arguments(format!(r#"{{"path":".","pattern":"{NEEDLE}"}}"#)),
        &executor.filesystem,
        &executor.root,
    )
    .expect("search arguments are valid");
    let ReadResult::SearchFiles(result) = executor
        .execute_operation(operation)
        .expect("bounded directory search succeeds")
    else {
        panic!("search_files returns search matches")
    };

    assert!(result.matches.is_empty());
    assert!(result.truncated);
}

#[test]
fn long_line_window_contains_late_match_start() {
    const PREFIX_BYTES: usize = MAX_SEARCH_LINE_BYTES + 1;
    const NEEDLE: &str = "needle";

    let line = format!("{}{NEEDLE}", "x".repeat(PREFIX_BYTES));
    let (window, text_start_column, truncated) =
        bounded_match_window(&line, PREFIX_BYTES, MAX_SEARCH_LINE_BYTES);

    assert!(window.starts_with(NEEDLE));
    assert_eq!(text_start_column, PREFIX_BYTES + 1);
    assert!(truncated);
}
