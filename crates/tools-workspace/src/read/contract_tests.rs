//! Contract tests for the `read_file`, `list_directory`, `glob_files`, and
//! `search_files` tool definitions: schema-rendered path, pattern, and
//! result-bound properties.

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
fn read_file_schema_carries_path_character_and_content_byte_bounds() {
    let schema = rendered_contract_schema::<ReadFileContract>();

    assert_eq!(
        schema["properties"]["path"]["maxLength"],
        json!(crate::path::MAX_WORKSPACE_PATH_CHARACTERS)
    );
    assert_eq!(schema["properties"]["path"]["minLength"], json!(1));
    assert_eq!(
        schema["properties"]["max_bytes"]["maximum"],
        json!(MAX_WORKSPACE_READ_BYTES)
    );
    assert_eq!(schema["properties"]["max_bytes"]["minimum"], json!(1));
    assert_eq!(schema["properties"]["offset"]["minimum"], json!(0));
}

#[test]
fn glob_files_schema_carries_pattern_path_and_result_bounds() {
    let schema = rendered_contract_schema::<GlobFilesContract>();

    assert_eq!(
        schema["properties"]["pattern"]["maxLength"],
        json!(MAX_PATTERN_CHARACTERS)
    );
    assert_eq!(
        schema["properties"]["path"]["maxLength"],
        json!(crate::path::MAX_WORKSPACE_PATH_CHARACTERS)
    );
    assert_eq!(
        schema["properties"]["max_results"]["maximum"],
        json!(MAX_RESULTS)
    );
}

#[test]
fn unicode_character_bounds_match_schema_and_runtime_validation() {
    const CHARACTER: &str = "é";
    const CHARACTER_COUNT: usize = 3_000;

    let path = CHARACTER.repeat(CHARACTER_COUNT);
    let pattern = CHARACTER.repeat(CHARACTER_COUNT);
    let workspace = tempfile::tempdir().expect("workspace fixture constructs");
    let executor = fixture_executor(&workspace);

    assert!(path.len() > crate::path::MAX_WORKSPACE_PATH_CHARACTERS);
    assert!(pattern.len() > MAX_PATTERN_CHARACTERS);

    let encoded = json!({"path": path, "pattern": pattern}).to_string();
    let _operation = decode_operation(
        ReadToolKind::SearchFiles,
        &arguments(encoded),
        &executor.filesystem,
        &executor.root,
    )
    .expect("schema-admitted Unicode bounds validate at runtime");
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
    assert_eq!(result.offset, 0);
    assert_eq!(result.next_offset, CONTENT.len() as u64);
}

#[test]
fn read_file_offset_reaches_content_past_the_per_call_byte_cap() {
    const FILE_PATH: &str = "large.txt";
    const TAIL: &str = "the tail past the first page";

    let workspace = tempfile::tempdir().expect("workspace fixture constructs");
    let head = "a".repeat(MAX_WORKSPACE_READ_BYTES);
    let content = format!("{head}{TAIL}");
    fs::write(workspace.path().join(FILE_PATH), &content).expect("fixture file writes");
    let executor = fixture_executor(&workspace);
    let operation = decode_operation(
        ReadToolKind::ReadFile,
        &arguments(format!(
            r#"{{"offset":{},"path":"{FILE_PATH}"}}"#,
            head.len()
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

    assert_eq!(result.content, TAIL);
    assert_eq!(result.offset, head.len() as u64);
    assert_eq!(result.next_offset, content.len() as u64);
    assert_eq!(result.total_bytes, content.len() as u64);
    assert!(!result.truncated);
}

#[test]
fn read_file_reports_the_cursor_that_continues_a_truncated_page() {
    const FILE_PATH: &str = "paged.txt";
    const CONTENT: &str = "abcdefgh";
    const PAGE_BYTES: usize = 3;

    let workspace = tempfile::tempdir().expect("workspace fixture constructs");
    fs::write(workspace.path().join(FILE_PATH), CONTENT).expect("fixture file writes");
    let executor = fixture_executor(&workspace);
    let operation = decode_operation(
        ReadToolKind::ReadFile,
        &arguments(format!(
            r#"{{"max_bytes":{PAGE_BYTES},"path":"{FILE_PATH}"}}"#
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

    assert_eq!(result.content, CONTENT[..PAGE_BYTES]);
    assert_eq!(result.next_offset, PAGE_BYTES as u64);
    assert!(result.truncated);

    let continuation = decode_operation(
        ReadToolKind::ReadFile,
        &arguments(format!(
            r#"{{"offset":{},"path":"{FILE_PATH}"}}"#,
            result.next_offset
        )),
        &executor.filesystem,
        &executor.root,
    )
    .expect("continuation arguments are valid");
    let ReadResult::ReadFile(continued) = executor
        .execute_operation(continuation)
        .expect("continuation read succeeds")
    else {
        panic!("read_file returns a read result")
    };

    assert_eq!(continued.content, CONTENT[PAGE_BYTES..]);
    assert_eq!(continued.offset, PAGE_BYTES as u64);
    assert!(!continued.truncated);
}

#[test]
fn read_file_offset_beyond_the_file_returns_an_empty_complete_page() {
    const FILE_PATH: &str = "short.txt";
    const CONTENT: &str = "abcd";

    let workspace = tempfile::tempdir().expect("workspace fixture constructs");
    fs::write(workspace.path().join(FILE_PATH), CONTENT).expect("fixture file writes");
    let executor = fixture_executor(&workspace);
    let operation = decode_operation(
        ReadToolKind::ReadFile,
        &arguments(format!(
            r#"{{"offset":{},"path":"{FILE_PATH}"}}"#,
            CONTENT.len() + 1
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

    assert_eq!(result.content, "");
    assert_eq!(result.bytes_read, 0);
    assert_eq!(result.total_bytes, CONTENT.len() as u64);
    assert!(!result.truncated);
}

#[test]
fn read_file_offset_inside_a_character_starts_at_the_next_boundary() {
    const FILE_PATH: &str = "unicode.txt";
    const CONTENT: &str = "é!";

    let workspace = tempfile::tempdir().expect("workspace fixture constructs");
    fs::write(workspace.path().join(FILE_PATH), CONTENT).expect("fixture file writes");
    let executor = fixture_executor(&workspace);
    let operation = decode_operation(
        ReadToolKind::ReadFile,
        &arguments(format!(r#"{{"offset":1,"path":"{FILE_PATH}"}}"#)),
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

    assert_eq!(result.content, "!");
    assert_eq!(result.offset, 2);
    assert_eq!(result.next_offset, CONTENT.len() as u64);
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
fn truncated_search_ignores_an_incomplete_final_line() {
    const FILE_PATH: &str = "long.txt";
    const NEEDLE: &str = "needle";
    const CONTINUATION: &str = "-continues";

    let retained_prefix = format!(
        "{}{NEEDLE}",
        "x".repeat(MAX_SEARCH_FILE_BYTES - NEEDLE.len())
    );
    let content = format!("{retained_prefix}{CONTINUATION}\n");
    let workspace = tempfile::tempdir().expect("workspace fixture constructs");
    fs::write(workspace.path().join(FILE_PATH), content).expect("fixture file writes");
    let executor = fixture_executor(&workspace);
    let operation = decode_operation(
        ReadToolKind::SearchFiles,
        &arguments(format!(r#"{{"path":"{FILE_PATH}","pattern":"{NEEDLE}$"}}"#)),
        &executor.filesystem,
        &executor.root,
    )
    .expect("search arguments are valid");
    let ReadResult::SearchFiles(result) = executor
        .execute_operation(operation)
        .expect("bounded search succeeds")
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

#[test]
fn long_line_window_keeps_multibyte_match_crossing_the_byte_boundary() {
    const PREFIX_BYTES: usize = MAX_SEARCH_LINE_BYTES - 1;
    const MATCHED_SCALAR: &str = "é";

    let line = format!("{}{MATCHED_SCALAR}", "x".repeat(PREFIX_BYTES));
    let (window, text_start_column, truncated) =
        bounded_match_window(&line, PREFIX_BYTES, MAX_SEARCH_LINE_BYTES);

    assert!(window.ends_with(MATCHED_SCALAR));
    assert_eq!(window.len(), MAX_SEARCH_LINE_BYTES);
    assert_eq!(text_start_column, 2);
    assert!(truncated);
}

#[test]
fn malformed_utf8_starting_before_the_byte_boundary_is_rejected() {
    const MAX_BYTES: usize = 2;
    const MALFORMED: &[u8] = b"a\xc3x";

    assert_eq!(utf8_prefix(MALFORMED, MAX_BYTES), None);
}

#[test]
fn valid_utf8_scalar_crossing_the_byte_boundary_is_trimmed() {
    const MAX_BYTES: usize = 2;
    const CONTENT: &[u8] = "aé".as_bytes();

    assert_eq!(utf8_prefix(CONTENT, MAX_BYTES), Some("a"));
}

#[test]
fn maximum_escaped_read_content_remains_inside_result_text_admission() {
    const FILE_PATH: &str = "control.txt";

    let content = "\0".repeat(MAX_WORKSPACE_READ_BYTES);
    let content_bytes = content.len();
    let result = ReadResult::ReadFile(ReadFileResult {
        path: String::from(FILE_PATH),
        content,
        offset: 0,
        bytes_read: content_bytes,
        next_offset: content_bytes as u64,
        total_bytes: content_bytes as u64,
        truncated: false,
    });

    let encoded = encode_read_result(result).expect("bounded result encoding succeeds");
    let admitted = ToolResultText::try_new(encoded.clone()).expect("encoded result is admitted");
    let decoded: serde_json::Value =
        serde_json::from_str(admitted.as_str()).expect("encoded result is JSON");
    let retained = decoded["content"]
        .as_str()
        .expect("read result content is text");
    let bytes_read = decoded["bytes_read"]
        .as_u64()
        .expect("read byte count is unsigned") as usize;

    assert_eq!(retained.len(), bytes_read);
    assert_eq!(bytes_read, content_bytes);
    assert_eq!(decoded["truncated"], serde_json::Value::Bool(false));
}

#[test]
fn escaped_search_evidence_is_truncated_to_result_text_admission() {
    const FILE_PATH: &str = "control.txt";

    let evidence = SearchMatch {
        path: String::from(FILE_PATH),
        line: 1,
        column: 1,
        text_start_column: 1,
        text: "\0".repeat(MAX_SEARCH_LINE_BYTES),
        line_truncated: false,
    };
    let result = ReadResult::SearchFiles(SearchFilesResult {
        matches: std::iter::repeat_n(evidence, MAX_RESULTS).collect(),
        truncated: false,
    });

    let encoded = encode_read_result(result).expect("bounded result encoding succeeds");
    let admitted = ToolResultText::try_new(encoded.clone()).expect("encoded result is admitted");
    let decoded: serde_json::Value =
        serde_json::from_str(admitted.as_str()).expect("encoded result is JSON");
    let retained_matches = decoded["matches"]
        .as_array()
        .expect("search result matches are an array");

    assert!(retained_matches.len() < MAX_RESULTS);
    assert_eq!(decoded["truncated"], serde_json::Value::Bool(true));
}

#[test]
fn recursive_walk_stops_at_aggregate_generated_path_byte_budget() {
    const FIRST_DIRECTORY: &str = "a";
    const NESTED_DIRECTORY: &str = "a/b";

    let workspace = tempfile::tempdir().expect("workspace fixture constructs");
    fs::create_dir_all(workspace.path().join(NESTED_DIRECTORY))
        .expect("nested fixture directories create");
    let executor = fixture_executor(&workspace);

    let walk = executor
        .walk_with_limits(Path::new("."), MAX_WALK_ENTRIES, FIRST_DIRECTORY.len())
        .expect("bounded traversal succeeds");

    assert_eq!(walk.entries.len(), 1);
    assert_eq!(walk.entries[0].path, PathBuf::from(FIRST_DIRECTORY));
    assert!(walk.truncated);
}
