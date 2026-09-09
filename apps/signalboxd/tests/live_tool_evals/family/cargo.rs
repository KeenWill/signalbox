//! Cargo evaluation fixtures and verification.

use crate::*;

#[cfg(unix)]
pub(crate) const GROUP_OR_OTHER_WRITE_MODE_BITS: u32 = 0o022;
#[cfg(unix)]
pub(crate) const CARGO_TARGET_DIRECTORY_MODE: u32 = 0o700;
pub(crate) const SYNTHETIC_CARGO_DIAGNOSTIC_MESSAGE: &str = "synthetic compiler diagnostic";
pub(crate) const SYNTHETIC_CARGO_DIAGNOSTIC_FILE: &str = "src/lib.rs";
pub(crate) const LIVE_CARGO_DIAGNOSTIC_MESSAGE: &str =
    "use of deprecated function `old_fixture`: tool eval fixture diagnostic";
pub(crate) const CARGO_ERROR_DIAGNOSTIC_LEVEL: &str = "error";
pub(crate) const CARGO_WARNING_DIAGNOSTIC_LEVEL: &str = "warning";
pub(crate) const SYNTHETIC_CARGO_DIAGNOSTIC_LINE: u64 = 4;
pub(crate) const SYNTHETIC_CARGO_DIAGNOSTIC_START_COLUMN: u64 = 20;
pub(crate) const SYNTHETIC_CARGO_DIAGNOSTIC_END_COLUMN: u64 = 31;
pub(crate) const SYNTHETIC_CARGO_DIAGNOSTIC_BACKWARDS_END_COLUMN: u64 = 3;
pub(crate) const SYNTHETIC_CARGO_RAN_REPORT: &str = "Cargo check ran successfully.";
pub(crate) const SYNTHETIC_CARGO_FAILURE: &str = "synthetic Cargo fixture failure";
pub(crate) fn cargo_diagnostics_workspace_matches_seed(
    root: &Path,
    seed_entries: &BTreeMap<PathBuf, WorkspaceEntrySnapshot>,
    seed_modified_times: &BTreeMap<PathBuf, SystemTime>,
    seed_entry_identities: &BTreeMap<PathBuf, FilesystemIdentity>,
    seed_extended_attributes: &BTreeMap<PathBuf, ExtendedAttributeSnapshot>,
    seed_inode_flags: &BTreeMap<PathBuf, u32>,
    execution_window: Option<FilesystemExecutionTimeWindow>,
) -> EvalResult<bool> {
    let actual_entries = workspace_entries(root)?;
    let actual_modified_times = workspace_modified_times(root)?;
    let actual_entry_identities = workspace_entry_identities(root)?;
    let actual_extended_attributes = workspace_extended_attributes(root)?;
    let target = Path::new("target");
    let seed_entries_preserved = seed_entries
        .iter()
        .all(|(path, entry)| actual_entries.get(path) == Some(entry));
    let additions_are_exact_target = actual_entries.len() == seed_entries.len() + 1
        && actual_entries.iter().all(|(path, entry)| {
            seed_entries.contains_key(path)
                || (path == target && matches!(entry, WorkspaceEntrySnapshot::Directory { .. }))
        });
    let seed_times_preserved = seed_modified_times.iter().all(|(path, modified)| {
        path.as_os_str().is_empty() || actual_modified_times.get(path) == Some(modified)
    });
    let seed_entry_identities_preserved = seed_entry_identities.iter().all(|(path, identity)| {
        if path.as_os_str().is_empty() {
            filesystem_identity_matches_without_change_time(
                actual_entry_identities.get(path).copied(),
                Some(*identity),
            )
        } else {
            actual_entry_identities.get(path) == Some(identity)
        }
    });
    let seed_extended_attributes_preserved = seed_extended_attributes
        .iter()
        .all(|(path, attributes)| actual_extended_attributes.get(path) == Some(attributes));
    let target_identities_match = cargo_target_identities_match(
        &actual_entries,
        &actual_entry_identities,
        seed_entry_identities.get(Path::new("")),
    );
    let target_times_match = cargo_target_times_match(
        &actual_entries,
        &actual_modified_times,
        &actual_entry_identities,
        execution_window,
    );
    let target_attributes_match = cargo_target_attributes_match(
        &actual_entries,
        &actual_extended_attributes,
        seed_extended_attributes,
    );
    let target_entries_are_safe = cargo_target_entries_are_safe(&actual_entries);
    Ok(seed_entries_preserved
        && additions_are_exact_target
        && seed_times_preserved
        && seed_entry_identities_preserved
        && seed_extended_attributes_preserved
        && target_identities_match
        && target_times_match
        && target_attributes_match
        && target_entries_are_safe
        && workspace_inode_flags_match_for_mutation_with_reference(
            root,
            seed_inode_flags,
            target,
            Path::new(""),
        )?
        && workspace_mutation_entry_times_match(root, Path::new(""), execution_window)?
        && matches!(
            actual_entries.get(target),
            Some(WorkspaceEntrySnapshot::Directory { .. })
        ))
}

pub(crate) fn cargo_seed_inode_flags_without_target(
    root: &Path,
) -> EvalResult<BTreeMap<PathBuf, u32>> {
    let mut flags = workspace_inode_flags(root)?;
    flags.retain(|path, _| !path.starts_with("target"));
    Ok(flags)
}

pub(crate) fn cargo_target_identities_match(
    entries: &BTreeMap<PathBuf, WorkspaceEntrySnapshot>,
    identities: &BTreeMap<PathBuf, FilesystemIdentity>,
    expected_identity: Option<&FilesystemIdentity>,
) -> bool {
    entries
        .keys()
        .filter(|path| path.starts_with("target"))
        .all(|path| match (identities.get(path), expected_identity) {
            (Some(identity), Some(expected_identity)) => {
                identity.device == expected_identity.device
                    && filesystem_ownership_matches(Some(identity), Some(expected_identity))
            }
            (None, None) => true,
            _ => false,
        })
}

pub(crate) fn cargo_target_entries_are_safe(
    entries: &BTreeMap<PathBuf, WorkspaceEntrySnapshot>,
) -> bool {
    entries
        .iter()
        .filter(|(path, _)| path.starts_with("target"))
        .all(|(_, entry)| match entry {
            WorkspaceEntrySnapshot::Directory { mode } => cargo_target_mode_is_safe(*mode),
            WorkspaceEntrySnapshot::File { mode, links, .. } => {
                cargo_target_mode_is_safe(*mode) && *links == WORKSPACE_CREATED_FILE_LINKS
            }
            WorkspaceEntrySnapshot::Symlink | WorkspaceEntrySnapshot::Other => false,
        })
}

#[cfg(unix)]
pub(crate) fn cargo_target_mode_is_safe(mode: Option<u32>) -> bool {
    mode.is_some_and(|mode| mode & GROUP_OR_OTHER_WRITE_MODE_BITS == 0)
}

#[cfg(not(unix))]
pub(crate) fn cargo_target_mode_is_safe(mode: Option<u32>) -> bool {
    mode.is_none()
}

pub(crate) fn cargo_target_times_match(
    entries: &BTreeMap<PathBuf, WorkspaceEntrySnapshot>,
    modified_times: &BTreeMap<PathBuf, SystemTime>,
    identities: &BTreeMap<PathBuf, FilesystemIdentity>,
    execution_window: Option<FilesystemExecutionTimeWindow>,
) -> bool {
    execution_window.is_some_and(|window| {
        entries
            .keys()
            .filter(|path| path.starts_with("target"))
            .all(|path| {
                modified_times
                    .get(path)
                    .is_some_and(|modified| window.contains_modified(*modified))
                    && identities
                        .get(path)
                        .is_some_and(|identity| window.contains_change_time(*identity))
            })
    })
}

pub(crate) fn cargo_target_attributes_match(
    entries: &BTreeMap<PathBuf, WorkspaceEntrySnapshot>,
    attributes: &BTreeMap<PathBuf, ExtendedAttributeSnapshot>,
    seed_attributes: &BTreeMap<PathBuf, ExtendedAttributeSnapshot>,
) -> bool {
    let expected = creation_extended_attributes(seed_attributes);
    entries
        .keys()
        .filter(|path| path.starts_with("target"))
        .all(|path| attributes.get(path) == Some(&expected))
}

#[test]
fn cargo_target_attributes_require_the_seed_selinux_label() {
    let fixture_label = b"synthetic fixture security context".to_vec();
    let label_name = b"security.selinux".to_vec();
    let seed_attributes = BTreeMap::from([(
        PathBuf::new(),
        BTreeMap::from([(label_name.clone(), fixture_label.clone())]),
    )]);
    let entries = BTreeMap::from([(
        PathBuf::from("target"),
        WorkspaceEntrySnapshot::Directory { mode: None },
    )]);
    let mut attributes = BTreeMap::from([(
        PathBuf::from("target"),
        BTreeMap::from([(label_name.clone(), fixture_label)]),
    )]);
    assert!(cargo_target_attributes_match(
        &entries,
        &attributes,
        &seed_attributes,
    ));

    attributes.insert(
        PathBuf::from("target"),
        BTreeMap::from([(label_name, b"different synthetic security context".to_vec())]),
    );
    assert!(!cargo_target_attributes_match(
        &entries,
        &attributes,
        &seed_attributes,
    ));
}

#[test]
fn creation_attributes_do_not_inherit_user_attributes() {
    let fixture_label = b"synthetic fixture security context".to_vec();
    let label_name = b"security.selinux".to_vec();
    let seed = BTreeMap::from([(
        PathBuf::new(),
        BTreeMap::from([
            (label_name.clone(), fixture_label.clone()),
            (
                SYNTHETIC_UNEXPECTED_XATTR_NAME.as_bytes().to_vec(),
                SYNTHETIC_UNEXPECTED_XATTR_VALUE.to_vec(),
            ),
        ]),
    )]);

    assert_eq!(
        creation_extended_attributes(&seed),
        BTreeMap::from([(label_name, fixture_label)]),
    );
}

#[test]
fn forced_cargo_completion_accepts_a_successful_check_report() {
    let tracker = OperationTracker::default();
    tracker.observe_response_text(SYNTHETIC_CARGO_RAN_REPORT, false);

    assert!(forced_case_completion_reported(
        CARGO_DIAGNOSTICS_NAME,
        true,
        &tracker,
    ));
}

/// The complete result shape needed before one successful Cargo diagnostics
/// exchange can count as forced-tier evidence.
#[derive(serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct CargoDiagnosticsEvalResult {
    pub(crate) command: String,
    pub(crate) execution: CargoDiagnosticsEvalExecution,
    pub(crate) diagnostics: CargoDiagnosticsEvalRecords,
    pub(crate) eval_receipt: String,
}

#[derive(serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct CargoDiagnosticsEvalExecution {
    pub(crate) confinement: CargoDiagnosticsEvalConfinement,
    pub(crate) outcome: CargoDiagnosticsEvalOutcome,
    pub(crate) stdout: CargoDiagnosticsEvalStream,
    pub(crate) stderr: CargoDiagnosticsEvalStream,
    pub(crate) cargo_failure: serde_json::Value,
}

#[derive(serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct CargoDiagnosticsEvalConfinement {
    pub(crate) kind: String,
}

#[derive(serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct CargoDiagnosticsEvalOutcome {
    pub(crate) kind: String,
    pub(crate) code: Option<i64>,
}

#[derive(serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct CargoDiagnosticsEvalStream {
    pub(crate) completeness: String,
    pub(crate) encoding: String,
}

#[derive(serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct CargoDiagnosticsEvalRecords {
    #[serde(rename = "values")]
    pub(crate) values: Vec<serde_json::Value>,
    pub(crate) limit_reached: bool,
    pub(crate) provenance: String,
    pub(crate) known_truncated: bool,
}

#[derive(serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct CargoDiagnosticsEvalDiagnostic {
    pub(crate) file: serde_json::Value,
    pub(crate) file_completeness: String,
    pub(crate) span: serde_json::Value,
    pub(crate) level: String,
    pub(crate) level_completeness: String,
    pub(crate) message: String,
    pub(crate) message_completeness: String,
}

#[derive(serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct CargoDiagnosticsEvalSpan {
    pub(crate) line_start: u64,
    pub(crate) column_start: u64,
    pub(crate) line_end: u64,
    pub(crate) column_end: u64,
}

/// Whether Cargo diagnostics returned the requested successful pass together
/// with every execution and record envelope the tool promises.
pub(crate) fn cargo_diagnostics_result_passed(result: &serde_json::Value) -> bool {
    let Ok(result) = serde_json::from_value::<CargoDiagnosticsEvalResult>(result.clone()) else {
        return false;
    };
    result.command == "check"
        && !result.eval_receipt.is_empty()
        && result.execution.confinement.kind == "filesystem_confined"
        && result.execution.outcome.kind == "exited"
        && result.execution.outcome.code == Some(0)
        && cargo_diagnostics_stream_is_valid(&result.execution.stdout)
        && cargo_diagnostics_stream_is_valid(&result.execution.stderr)
        && result.execution.cargo_failure.is_null()
        && result.diagnostics.provenance == "workspace_influenced"
        && !result.diagnostics.limit_reached
        && !result.diagnostics.known_truncated
        && cargo_diagnostics_are_exact_live_fixture_evidence(&result.diagnostics.values)
}

pub(crate) fn cargo_diagnostics_are_exact_live_fixture_evidence(
    records: &[serde_json::Value],
) -> bool {
    let [record] = records else {
        return false;
    };
    let Ok(diagnostic) = serde_json::from_value::<CargoDiagnosticsEvalDiagnostic>(record.clone())
    else {
        return false;
    };
    let Ok(span) = serde_json::from_value::<CargoDiagnosticsEvalSpan>(diagnostic.span.clone())
    else {
        return false;
    };
    diagnostic.file.as_str() == Some(SYNTHETIC_CARGO_DIAGNOSTIC_FILE)
        && diagnostic.file_completeness == "complete"
        && span.line_start == SYNTHETIC_CARGO_DIAGNOSTIC_LINE
        && span.column_start == SYNTHETIC_CARGO_DIAGNOSTIC_START_COLUMN
        && span.line_end == SYNTHETIC_CARGO_DIAGNOSTIC_LINE
        && span.column_end == SYNTHETIC_CARGO_DIAGNOSTIC_END_COLUMN
        && diagnostic.level == CARGO_WARNING_DIAGNOSTIC_LEVEL
        && diagnostic.level_completeness == "complete"
        && diagnostic.message == LIVE_CARGO_DIAGNOSTIC_MESSAGE
        && diagnostic.message_completeness == "complete"
}

pub(crate) fn cargo_diagnostics_stream_is_valid(stream: &CargoDiagnosticsEvalStream) -> bool {
    stream.completeness == "complete" && stream.encoding == "utf8"
}

/// One complete, successful Cargo check result in the eval workspace.
pub(crate) fn successful_cargo_diagnostics_result() -> serde_json::Value {
    let stream = CargoDiagnosticsStream {
        completeness: CaptureCompleteness::Complete,
        encoding: OutputEncoding::Utf8,
    };
    let mut result = cargo_diagnostics_result(CargoDiagnosticsExecution {
        confinement: ExecutionConfinement::FilesystemConfined,
        outcome: ProcessOutcome::Exited { code: Some(0) },
        stdout: stream,
        stderr: stream,
        cargo_failure: None,
    });
    result["diagnostics"]["values"] = serde_json::json!([live_cargo_diagnostic()]);
    result[EVAL_RECEIPT_FIELD] = serde_json::json!(SYNTHETIC_EVAL_RECEIPT);
    result
}

/// One serialized Cargo check result carrying the supplied execution evidence.
pub(crate) fn cargo_diagnostics_result(execution: CargoDiagnosticsExecution) -> serde_json::Value {
    let records = CargoDiagnosticRecords {
        values: Vec::new(),
        limit_reached: false,
        provenance: CargoEvidenceProvenance::WorkspaceInfluenced,
        known_truncated: false,
    };
    serde_json::to_value(CargoDiagnosticsResult {
        command: CargoDiagnosticsCommand::Check,
        execution,
        diagnostics: records,
    })
    .expect("producer Cargo diagnostics result serializes")
}

pub(crate) fn synthetic_cargo_diagnostic(level: &str) -> CargoDiagnostic {
    CargoDiagnostic {
        file: None,
        file_completeness: CaptureCompleteness::Complete,
        span: None,
        level: String::from(level),
        level_completeness: CaptureCompleteness::Complete,
        message: String::from(SYNTHETIC_CARGO_DIAGNOSTIC_MESSAGE),
        message_completeness: CaptureCompleteness::Complete,
    }
}

pub(crate) fn live_cargo_diagnostic() -> CargoDiagnostic {
    CargoDiagnostic {
        file: Some(String::from(SYNTHETIC_CARGO_DIAGNOSTIC_FILE)),
        file_completeness: CaptureCompleteness::Complete,
        span: Some(CargoDiagnosticSpan {
            line_start: SYNTHETIC_CARGO_DIAGNOSTIC_LINE,
            column_start: SYNTHETIC_CARGO_DIAGNOSTIC_START_COLUMN,
            line_end: SYNTHETIC_CARGO_DIAGNOSTIC_LINE,
            column_end: SYNTHETIC_CARGO_DIAGNOSTIC_END_COLUMN,
        }),
        level: String::from(CARGO_WARNING_DIAGNOSTIC_LEVEL),
        level_completeness: CaptureCompleteness::Complete,
        message: String::from(LIVE_CARGO_DIAGNOSTIC_MESSAGE),
        message_completeness: CaptureCompleteness::Complete,
    }
}

pub(crate) fn synthetic_cargo_diagnostic_span() -> serde_json::Value {
    serde_json::json!({
        "line_start": SYNTHETIC_CARGO_DIAGNOSTIC_LINE,
        "column_start": SYNTHETIC_CARGO_DIAGNOSTIC_START_COLUMN,
        "line_end": SYNTHETIC_CARGO_DIAGNOSTIC_LINE,
        "column_end": SYNTHETIC_CARGO_DIAGNOSTIC_END_COLUMN,
    })
}

#[test]
fn forced_cargo_diagnostics_rejects_an_unconfined_zero_exit() {
    let mut result = successful_cargo_diagnostics_result();
    result["execution"]["confinement"]["kind"] = serde_json::json!("unsandboxed");
    let outcome = forced_exec_outcome(CARGO_DIAGNOSTICS_NAME, result);

    assert_eq!(
        outcome.forced_disposition(),
        EvalDisposition::Infrastructure
    );
}

#[test]
fn forced_cargo_diagnostics_accepts_a_complete_successful_check() {
    let result = successful_cargo_diagnostics_result();
    let outcome = forced_exec_outcome(CARGO_DIAGNOSTICS_NAME, result);

    assert_eq!(outcome.forced_disposition(), EvalDisposition::Pass);
}

#[test]
fn forced_cargo_diagnostics_accepts_correlated_file_and_span() {
    let mut result = successful_cargo_diagnostics_result();
    let mut diagnostic = serde_json::to_value(live_cargo_diagnostic())
        .expect("producer Cargo diagnostics serialize");
    diagnostic["span"] = synthetic_cargo_diagnostic_span();
    result["diagnostics"]["values"] = serde_json::json!([diagnostic]);
    let outcome = forced_exec_outcome(CARGO_DIAGNOSTICS_NAME, result);

    assert_eq!(outcome.forced_disposition(), EvalDisposition::Pass);
}

#[test]
fn forced_cargo_diagnostics_requires_the_live_fixture_warning() {
    let mut result = successful_cargo_diagnostics_result();
    result["diagnostics"]["values"] = serde_json::json!([]);
    let outcome = forced_exec_outcome(CARGO_DIAGNOSTICS_NAME, result);

    assert_eq!(
        outcome.forced_disposition(),
        EvalDisposition::Infrastructure
    );
}

#[test]
fn forced_cargo_diagnostics_rejects_a_duplicated_fixture_warning() {
    let mut result = successful_cargo_diagnostics_result();
    let diagnostic = serde_json::to_value(live_cargo_diagnostic())
        .expect("producer Cargo diagnostics serialize");
    result["diagnostics"]["values"] = serde_json::json!([diagnostic, diagnostic]);
    let outcome = forced_exec_outcome(CARGO_DIAGNOSTICS_NAME, result);

    assert_eq!(
        outcome.forced_disposition(),
        EvalDisposition::Infrastructure
    );
}

#[test]
fn forced_cargo_diagnostics_rejects_an_extra_recognized_diagnostic() {
    let mut result = successful_cargo_diagnostics_result();
    let fixture = serde_json::to_value(live_cargo_diagnostic())
        .expect("producer Cargo diagnostics serialize");
    let extra = serde_json::to_value(synthetic_cargo_diagnostic("note"))
        .expect("producer Cargo diagnostics serialize");
    result["diagnostics"]["values"] = serde_json::json!([fixture, extra]);
    let outcome = forced_exec_outcome(CARGO_DIAGNOSTICS_NAME, result);

    assert_eq!(
        outcome.forced_disposition(),
        EvalDisposition::Infrastructure
    );
}

#[test]
fn forced_cargo_diagnostics_rejects_a_different_valid_fixture_span() {
    let mut result = successful_cargo_diagnostics_result();
    result["diagnostics"]["values"][0]["span"]["column_end"] =
        serde_json::json!(SYNTHETIC_CARGO_DIAGNOSTIC_END_COLUMN + 1);
    let outcome = forced_exec_outcome(CARGO_DIAGNOSTICS_NAME, result);

    assert_eq!(
        outcome.forced_disposition(),
        EvalDisposition::Infrastructure
    );
}

#[test]
fn forced_cargo_diagnostics_rejects_a_marker_bearing_wrong_message() {
    let mut result = successful_cargo_diagnostics_result();
    result["diagnostics"]["values"][0]["message"] =
        serde_json::json!("synthetic prefix: tool eval fixture diagnostic");
    let outcome = forced_exec_outcome(CARGO_DIAGNOSTICS_NAME, result);

    assert_eq!(
        outcome.forced_disposition(),
        EvalDisposition::Infrastructure
    );
}

#[test]
fn forced_cargo_diagnostics_rejects_error_diagnostics_from_a_successful_check() {
    let mut result = successful_cargo_diagnostics_result();
    result["diagnostics"]["values"] = serde_json::to_value(vec![synthetic_cargo_diagnostic(
        CARGO_ERROR_DIAGNOSTIC_LEVEL,
    )])
    .expect("producer Cargo diagnostics serialize");
    let outcome = forced_exec_outcome(CARGO_DIAGNOSTICS_NAME, result);

    assert_eq!(
        outcome.forced_disposition(),
        EvalDisposition::Infrastructure
    );
}

#[test]
fn forced_cargo_diagnostics_rejects_an_unknown_diagnostic_level() {
    let mut result = successful_cargo_diagnostics_result();
    result["diagnostics"]["values"] =
        serde_json::to_value(vec![synthetic_cargo_diagnostic("fatal")])
            .expect("producer Cargo diagnostics serialize");
    let outcome = forced_exec_outcome(CARGO_DIAGNOSTICS_NAME, result);

    assert_eq!(
        outcome.forced_disposition(),
        EvalDisposition::Infrastructure
    );
}

#[test]
fn forced_cargo_diagnostics_rejects_an_unknown_top_level_field() {
    let mut result = successful_cargo_diagnostics_result();
    result["unexpected"] = serde_json::json!("synthetic field");
    let outcome = forced_exec_outcome(CARGO_DIAGNOSTICS_NAME, result);

    assert_eq!(
        outcome.forced_disposition(),
        EvalDisposition::Infrastructure
    );
}

#[test]
fn forced_cargo_diagnostics_rejects_an_unknown_span_field() {
    let mut result = successful_cargo_diagnostics_result();
    let mut diagnostic =
        serde_json::to_value(synthetic_cargo_diagnostic(CARGO_WARNING_DIAGNOSTIC_LEVEL))
            .expect("producer Cargo diagnostics serialize");
    diagnostic["file"] = serde_json::json!(SYNTHETIC_CARGO_DIAGNOSTIC_FILE);
    let mut span = synthetic_cargo_diagnostic_span();
    span["unexpected"] = serde_json::json!("synthetic field");
    diagnostic["span"] = span;
    result["diagnostics"]["values"] = serde_json::json!([diagnostic]);
    let outcome = forced_exec_outcome(CARGO_DIAGNOSTICS_NAME, result);

    assert_eq!(
        outcome.forced_disposition(),
        EvalDisposition::Infrastructure
    );
}

#[test]
fn forced_cargo_diagnostics_rejects_malformed_diagnostics() {
    let mut result = successful_cargo_diagnostics_result();
    result["diagnostics"]["values"] = serde_json::json!([{}]);
    let outcome = forced_exec_outcome(CARGO_DIAGNOSTICS_NAME, result);

    assert_eq!(
        outcome.forced_disposition(),
        EvalDisposition::Infrastructure
    );
}

#[test]
fn forced_cargo_diagnostics_rejects_a_backwards_same_line_span() {
    let mut result = successful_cargo_diagnostics_result();
    let mut diagnostic =
        serde_json::to_value(synthetic_cargo_diagnostic(CARGO_WARNING_DIAGNOSTIC_LEVEL))
            .expect("producer Cargo diagnostics serialize");
    diagnostic["file"] = serde_json::json!(SYNTHETIC_CARGO_DIAGNOSTIC_FILE);
    diagnostic["span"] = serde_json::json!({
        "line_start": SYNTHETIC_CARGO_DIAGNOSTIC_LINE,
        "column_start": SYNTHETIC_CARGO_DIAGNOSTIC_START_COLUMN,
        "line_end": SYNTHETIC_CARGO_DIAGNOSTIC_LINE,
        "column_end": SYNTHETIC_CARGO_DIAGNOSTIC_BACKWARDS_END_COLUMN,
    });
    result["diagnostics"]["values"] = serde_json::json!([diagnostic]);
    let outcome = forced_exec_outcome(CARGO_DIAGNOSTICS_NAME, result);

    assert_eq!(
        outcome.forced_disposition(),
        EvalDisposition::Infrastructure
    );
}

#[test]
fn forced_cargo_diagnostics_rejects_a_span_without_its_file() {
    let mut result = successful_cargo_diagnostics_result();
    let mut diagnostic =
        serde_json::to_value(synthetic_cargo_diagnostic(CARGO_WARNING_DIAGNOSTIC_LEVEL))
            .expect("producer Cargo diagnostics serialize");
    diagnostic["span"] = synthetic_cargo_diagnostic_span();
    result["diagnostics"]["values"] = serde_json::json!([diagnostic]);
    let outcome = forced_exec_outcome(CARGO_DIAGNOSTICS_NAME, result);

    assert_eq!(
        outcome.forced_disposition(),
        EvalDisposition::Infrastructure
    );
}

#[test]
fn forced_cargo_diagnostics_rejects_a_file_without_its_span() {
    let mut result = successful_cargo_diagnostics_result();
    let mut diagnostic =
        serde_json::to_value(synthetic_cargo_diagnostic(CARGO_WARNING_DIAGNOSTIC_LEVEL))
            .expect("producer Cargo diagnostics serialize");
    diagnostic["file"] = serde_json::json!(SYNTHETIC_CARGO_DIAGNOSTIC_FILE);
    result["diagnostics"]["values"] = serde_json::json!([diagnostic]);
    let outcome = forced_exec_outcome(CARGO_DIAGNOSTICS_NAME, result);

    assert_eq!(
        outcome.forced_disposition(),
        EvalDisposition::Infrastructure
    );
}

#[test]
fn forced_cargo_diagnostics_requires_complete_absent_location_evidence() {
    let mut result = successful_cargo_diagnostics_result();
    let mut diagnostic =
        serde_json::to_value(synthetic_cargo_diagnostic(CARGO_WARNING_DIAGNOSTIC_LEVEL))
            .expect("producer Cargo diagnostics serialize");
    diagnostic["file_completeness"] = serde_json::json!("truncated");
    result["diagnostics"]["values"] = serde_json::json!([diagnostic]);
    let outcome = forced_exec_outcome(CARGO_DIAGNOSTICS_NAME, result);

    assert_eq!(
        outcome.forced_disposition(),
        EvalDisposition::Infrastructure
    );
}

#[test]
fn forced_cargo_diagnostics_rejects_a_truncated_present_file() {
    let mut result = successful_cargo_diagnostics_result();
    let mut diagnostic =
        serde_json::to_value(synthetic_cargo_diagnostic(CARGO_WARNING_DIAGNOSTIC_LEVEL))
            .expect("producer Cargo diagnostics serialize");
    diagnostic["file"] = serde_json::json!(SYNTHETIC_CARGO_DIAGNOSTIC_FILE);
    diagnostic["file_completeness"] = serde_json::json!("truncated");
    diagnostic["span"] = synthetic_cargo_diagnostic_span();
    result["diagnostics"]["values"] = serde_json::json!([diagnostic]);
    let outcome = forced_exec_outcome(CARGO_DIAGNOSTICS_NAME, result);

    assert_eq!(
        outcome.forced_disposition(),
        EvalDisposition::Infrastructure
    );
}

#[test]
fn forced_cargo_diagnostics_rejects_a_truncated_message() {
    let mut result = successful_cargo_diagnostics_result();
    let mut diagnostic =
        serde_json::to_value(synthetic_cargo_diagnostic(CARGO_WARNING_DIAGNOSTIC_LEVEL))
            .expect("producer Cargo diagnostics serialize");
    diagnostic["message_completeness"] = serde_json::json!("truncated");
    result["diagnostics"]["values"] = serde_json::json!([diagnostic]);
    let outcome = forced_exec_outcome(CARGO_DIAGNOSTICS_NAME, result);

    assert_eq!(
        outcome.forced_disposition(),
        EvalDisposition::Infrastructure
    );
}

#[test]
fn forced_cargo_diagnostics_rejects_a_truncated_stdout_capture() {
    let mut result = successful_cargo_diagnostics_result();
    result["execution"]["stdout"]["completeness"] = serde_json::json!("truncated");
    let outcome = forced_exec_outcome(CARGO_DIAGNOSTICS_NAME, result);

    assert_eq!(
        outcome.forced_disposition(),
        EvalDisposition::Infrastructure
    );
}

#[test]
fn forced_cargo_diagnostics_rejects_lossy_stderr_capture() {
    let mut result = successful_cargo_diagnostics_result();
    result["execution"]["stderr"]["encoding"] = serde_json::json!("lossy_utf8");
    let outcome = forced_exec_outcome(CARGO_DIAGNOSTICS_NAME, result);

    assert_eq!(
        outcome.forced_disposition(),
        EvalDisposition::Infrastructure
    );
}

#[test]
fn forced_cargo_diagnostics_rejects_capped_records() {
    let mut result = successful_cargo_diagnostics_result();
    result["diagnostics"]["limit_reached"] = serde_json::json!(true);
    let outcome = forced_exec_outcome(CARGO_DIAGNOSTICS_NAME, result);

    assert_eq!(
        outcome.forced_disposition(),
        EvalDisposition::Infrastructure
    );
}

#[test]
fn forced_cargo_workspace_verification_failure_is_infrastructure() {
    let mut outcome = forced_exec_outcome(
        CARGO_DIAGNOSTICS_NAME,
        successful_cargo_diagnostics_result(),
    );
    outcome.execution_completed = false;
    outcome.forced_verification_failed = true;

    assert_eq!(
        outcome.forced_disposition(),
        EvalDisposition::Infrastructure
    );
    assert_eq!(outcome.infrastructure_label(), "exact state mismatch");
    assert!(reject_forced_executor_failures(&[outcome]).is_err());
}

#[test]
fn forced_cargo_diagnostics_reports_sandbox_setup_failure_as_infrastructure() {
    let stream = CargoDiagnosticsStream {
        completeness: CaptureCompleteness::Complete,
        encoding: OutputEncoding::Utf8,
    };
    let result = cargo_diagnostics_result(CargoDiagnosticsExecution {
        confinement: ExecutionConfinement::SandboxSetupFailed,
        outcome: ProcessOutcome::SpawnFailed {
            reason: ProcessSpawnFailure::SandboxSetup,
        },
        stdout: stream,
        stderr: stream,
        cargo_failure: None,
    });
    let outcome = forced_exec_outcome(CARGO_DIAGNOSTICS_NAME, result);

    assert_eq!(
        outcome.forced_disposition(),
        EvalDisposition::Infrastructure
    );
}

#[test]
fn forced_cargo_diagnostics_reports_a_cargo_failure_as_infrastructure() {
    let stream = CargoDiagnosticsStream {
        completeness: CaptureCompleteness::Complete,
        encoding: OutputEncoding::Utf8,
    };
    let result = cargo_diagnostics_result(CargoDiagnosticsExecution {
        confinement: ExecutionConfinement::FilesystemConfined,
        outcome: ProcessOutcome::Exited { code: Some(1) },
        stdout: stream,
        stderr: stream,
        cargo_failure: Some(CargoFailureDetail {
            message: String::from(SYNTHETIC_CARGO_FAILURE),
            message_completeness: CaptureCompleteness::Complete,
        }),
    });
    let outcome = forced_exec_outcome(CARGO_DIAGNOSTICS_NAME, result);

    assert_eq!(
        outcome.forced_disposition(),
        EvalDisposition::Infrastructure
    );
    assert_eq!(outcome.infrastructure_label(), "Cargo failure");
    assert!(reject_forced_executor_failures(&[outcome]).is_err());
}

#[test]
fn forced_cargo_diagnostics_rejects_an_incomplete_result_shape() {
    let outcome = forced_exec_outcome(
        CARGO_DIAGNOSTICS_NAME,
        serde_json::json!({
            "execution": confined_exit(""),
        }),
    );

    assert_eq!(
        outcome.forced_disposition(),
        EvalDisposition::Infrastructure
    );
}

pub(crate) fn create_cargo_target_directory(
    root: &Path,
) -> EvalResult<FilesystemExecutionTimeWindow> {
    let started = current_filesystem_recorded_time()?;
    let target = root.join("target");
    fs::create_dir(&target)?;
    #[cfg(unix)]
    fs::set_permissions(
        target,
        fs::Permissions::from_mode(CARGO_TARGET_DIRECTORY_MODE),
    )?;
    Ok(FilesystemExecutionTimeWindow {
        started,
        finished: current_filesystem_recorded_time()?,
    })
}

#[test]
fn forced_cargo_diagnostics_workspace_accepts_the_exact_target_directory() -> EvalResult {
    let (workspace, seed_entries, seed_modified_times, seed_entry_identities) =
        prepared_exec_seed_workspace()?;
    let seed_extended_attributes = workspace_extended_attributes(workspace.path())?;
    let execution_window = create_cargo_target_directory(workspace.path())?;

    assert!(cargo_diagnostics_workspace_matches_seed(
        workspace.path(),
        &seed_entries,
        &seed_modified_times,
        &seed_entry_identities,
        &seed_extended_attributes,
        &cargo_seed_inode_flags_without_target(workspace.path())?,
        Some(execution_window),
    )?);
    Ok(())
}

#[cfg(target_os = "linux")]
#[test]
fn forced_cargo_diagnostics_workspace_rejects_target_inode_flag_drift() -> EvalResult {
    let (workspace, seed_entries, seed_modified_times, seed_entry_identities) =
        prepared_exec_seed_workspace()?;
    let seed_extended_attributes = workspace_extended_attributes(workspace.path())?;
    let seed_inode_flags = workspace_inode_flags(workspace.path())?;
    let execution_window = create_cargo_target_directory(workspace.path())?;
    let target = fs::File::open(workspace.path().join("target"))?;
    let flags = rustix::fs::ioctl_getflags(&target)?;
    rustix::fs::ioctl_setflags(&target, flags | rustix::fs::IFlags::NOATIME)?;

    assert!(!cargo_diagnostics_workspace_matches_seed(
        workspace.path(),
        &seed_entries,
        &seed_modified_times,
        &seed_entry_identities,
        &seed_extended_attributes,
        &seed_inode_flags,
        Some(execution_window),
    )?);
    Ok(())
}

#[test]
fn forced_cargo_diagnostics_workspace_rejects_an_unexpected_target_descendant() -> EvalResult {
    let (workspace, seed_entries, seed_modified_times, seed_entry_identities) =
        prepared_exec_seed_workspace()?;
    let seed_extended_attributes = workspace_extended_attributes(workspace.path())?;
    let execution_window = create_cargo_target_directory(workspace.path())?;
    fs::write(
        workspace.path().join("target/unexpected"),
        "synthetic unexpected target descendant\n",
    )?;

    assert!(!cargo_diagnostics_workspace_matches_seed(
        workspace.path(),
        &seed_entries,
        &seed_modified_times,
        &seed_entry_identities,
        &seed_extended_attributes,
        &cargo_seed_inode_flags_without_target(workspace.path())?,
        Some(execution_window),
    )?);
    Ok(())
}

#[cfg(unix)]
#[test]
fn forced_cargo_diagnostics_workspace_rejects_a_writable_target_directory() -> EvalResult {
    let (workspace, seed_entries, seed_modified_times, seed_entry_identities) =
        prepared_exec_seed_workspace()?;
    let seed_extended_attributes = workspace_extended_attributes(workspace.path())?;
    let started = current_filesystem_recorded_time()?;
    fs::create_dir(workspace.path().join("target"))?;
    fs::set_permissions(
        workspace.path().join("target"),
        fs::Permissions::from_mode(WORKSPACE_INSECURE_CREATION_MODE),
    )?;
    let execution_window = FilesystemExecutionTimeWindow {
        started,
        finished: current_filesystem_recorded_time()?,
    };

    assert!(!cargo_diagnostics_workspace_matches_seed(
        workspace.path(),
        &seed_entries,
        &seed_modified_times,
        &seed_entry_identities,
        &seed_extended_attributes,
        &cargo_seed_inode_flags_without_target(workspace.path())?,
        Some(execution_window),
    )?);
    Ok(())
}

#[cfg(unix)]
#[test]
fn forced_cargo_diagnostics_workspace_rejects_a_hard_linked_target_artifact() -> EvalResult {
    let (workspace, seed_entries, seed_modified_times, seed_entry_identities) =
        prepared_exec_seed_workspace()?;
    let support = tempfile::tempdir()?;
    let seed_extended_attributes = workspace_extended_attributes(workspace.path())?;
    let artifact = workspace.path().join("target/artifact");
    let started = current_filesystem_recorded_time()?;
    fs::create_dir(workspace.path().join("target"))?;
    fs::write(&artifact, "synthetic target artifact\n")?;
    fs::hard_link(&artifact, support.path().join("artifact-link"))?;
    let execution_window = FilesystemExecutionTimeWindow {
        started,
        finished: current_filesystem_recorded_time()?,
    };

    assert!(!cargo_diagnostics_workspace_matches_seed(
        workspace.path(),
        &seed_entries,
        &seed_modified_times,
        &seed_entry_identities,
        &seed_extended_attributes,
        &cargo_seed_inode_flags_without_target(workspace.path())?,
        Some(execution_window),
    )?);
    Ok(())
}

#[cfg(unix)]
#[test]
fn forced_cargo_diagnostics_workspace_rejects_a_symlinked_target_artifact() -> EvalResult {
    let (workspace, seed_entries, seed_modified_times, seed_entry_identities) =
        prepared_exec_seed_workspace()?;
    let seed_extended_attributes = workspace_extended_attributes(workspace.path())?;
    let execution_window = create_cargo_target_directory(workspace.path())?;
    symlink(
        workspace.path().join("Cargo.toml"),
        workspace.path().join("target/escape"),
    )?;

    assert!(!cargo_diagnostics_workspace_matches_seed(
        workspace.path(),
        &seed_entries,
        &seed_modified_times,
        &seed_entry_identities,
        &seed_extended_attributes,
        &cargo_seed_inode_flags_without_target(workspace.path())?,
        Some(execution_window),
    )?);
    Ok(())
}

#[test]
fn forced_cargo_diagnostics_workspace_rejects_a_mutated_seed_file() -> EvalResult {
    let (workspace, seed_entries, seed_modified_times, seed_entry_identities) =
        prepared_exec_seed_workspace()?;
    let seed_extended_attributes = workspace_extended_attributes(workspace.path())?;
    let execution_window = create_cargo_target_directory(workspace.path())?;
    fs::write(workspace.path().join("src/lib.rs"), "pub fn drifted() {}\n")?;

    assert!(!cargo_diagnostics_workspace_matches_seed(
        workspace.path(),
        &seed_entries,
        &seed_modified_times,
        &seed_entry_identities,
        &seed_extended_attributes,
        &cargo_seed_inode_flags_without_target(workspace.path())?,
        Some(execution_window),
    )?);
    Ok(())
}

#[test]
fn forced_cargo_diagnostics_workspace_rejects_a_deleted_seed_file() -> EvalResult {
    let (workspace, seed_entries, seed_modified_times, seed_entry_identities) =
        prepared_exec_seed_workspace()?;
    let seed_extended_attributes = workspace_extended_attributes(workspace.path())?;
    let execution_window = create_cargo_target_directory(workspace.path())?;
    fs::remove_file(workspace.path().join("Cargo.toml"))?;

    assert!(!cargo_diagnostics_workspace_matches_seed(
        workspace.path(),
        &seed_entries,
        &seed_modified_times,
        &seed_entry_identities,
        &seed_extended_attributes,
        &cargo_seed_inode_flags_without_target(workspace.path())?,
        Some(execution_window),
    )?);
    Ok(())
}

#[cfg(unix)]
#[test]
fn forced_cargo_diagnostics_rejects_byte_identical_seed_replacement() -> EvalResult {
    let (workspace, seed_entries, seed_modified_times, seed_entry_identities) =
        prepared_exec_seed_workspace()?;
    let seed_extended_attributes = workspace_extended_attributes(workspace.path())?;
    let execution_window = create_cargo_target_directory(workspace.path())?;
    replace_exec_seed_file_byte_identically(workspace.path(), &seed_modified_times)?;

    assert!(!cargo_diagnostics_workspace_matches_seed(
        workspace.path(),
        &seed_entries,
        &seed_modified_times,
        &seed_entry_identities,
        &seed_extended_attributes,
        &cargo_seed_inode_flags_without_target(workspace.path())?,
        Some(execution_window),
    )?);
    Ok(())
}

#[cfg(target_os = "linux")]
#[test]
fn forced_cargo_diagnostics_rejects_root_extended_attribute_drift() -> EvalResult {
    let (workspace, seed_entries, seed_modified_times, seed_entry_identities) =
        prepared_exec_seed_workspace()?;
    let seed_extended_attributes = workspace_extended_attributes(workspace.path())?;
    let execution_window = create_cargo_target_directory(workspace.path())?;
    rustix::fs::setxattr(
        workspace.path(),
        SYNTHETIC_UNEXPECTED_XATTR_NAME,
        SYNTHETIC_UNEXPECTED_XATTR_VALUE,
        rustix::fs::XattrFlags::CREATE,
    )?;

    assert!(!cargo_diagnostics_workspace_matches_seed(
        workspace.path(),
        &seed_entries,
        &seed_modified_times,
        &seed_entry_identities,
        &seed_extended_attributes,
        &cargo_seed_inode_flags_without_target(workspace.path())?,
        Some(execution_window),
    )?);
    Ok(())
}

#[cfg(target_os = "linux")]
#[test]
fn forced_cargo_diagnostics_rejects_target_extended_attributes() -> EvalResult {
    let (workspace, seed_entries, seed_modified_times, seed_entry_identities) =
        prepared_exec_seed_workspace()?;
    let seed_extended_attributes = workspace_extended_attributes(workspace.path())?;
    let started = current_filesystem_recorded_time()?;
    fs::create_dir(workspace.path().join("target"))?;
    rustix::fs::setxattr(
        workspace.path().join("target"),
        SYNTHETIC_UNEXPECTED_XATTR_NAME,
        SYNTHETIC_UNEXPECTED_XATTR_VALUE,
        rustix::fs::XattrFlags::CREATE,
    )?;
    let execution_window = FilesystemExecutionTimeWindow {
        started,
        finished: current_filesystem_recorded_time()?,
    };

    assert!(!cargo_diagnostics_workspace_matches_seed(
        workspace.path(),
        &seed_entries,
        &seed_modified_times,
        &seed_entry_identities,
        &seed_extended_attributes,
        &cargo_seed_inode_flags_without_target(workspace.path())?,
        Some(execution_window),
    )?);
    Ok(())
}

#[cfg(unix)]
#[test]
fn cargo_target_identity_gate_rejects_changed_identity() -> EvalResult {
    let (workspace, _seed_entries, _seed_modified_times, seed_entry_identities) =
        prepared_exec_seed_workspace()?;
    let _execution_window = create_cargo_target_directory(workspace.path())?;
    let entries = workspace_entries(workspace.path())?;
    let mut identities = workspace_entry_identities(workspace.path())?;
    identities
        .get_mut(Path::new("target"))
        .expect("the Cargo target fixture has a filesystem identity")
        .user_id = seed_entry_identities[Path::new("")].user_id.wrapping_add(1);

    assert!(!cargo_target_identities_match(
        &entries,
        &identities,
        seed_entry_identities.get(Path::new("")),
    ));
    Ok(())
}

#[cfg(unix)]
#[test]
fn cargo_target_identity_gate_rejects_a_different_device() -> EvalResult {
    let (workspace, _seed_entries, _seed_modified_times, seed_entry_identities) =
        prepared_exec_seed_workspace()?;
    let _execution_window = create_cargo_target_directory(workspace.path())?;
    let entries = workspace_entries(workspace.path())?;
    let mut identities = workspace_entry_identities(workspace.path())?;
    identities
        .get_mut(Path::new("target"))
        .expect("the Cargo target fixture has a filesystem identity")
        .device = seed_entry_identities[Path::new("")].device.wrapping_add(1);

    assert!(!cargo_target_identities_match(
        &entries,
        &identities,
        seed_entry_identities.get(Path::new("")),
    ));
    Ok(())
}

#[test]
fn forced_cargo_diagnostics_rejects_out_of_window_target_times() -> EvalResult {
    let (workspace, seed_entries, seed_modified_times, seed_entry_identities) =
        prepared_exec_seed_workspace()?;
    let seed_extended_attributes = workspace_extended_attributes(workspace.path())?;
    let execution_window = create_cargo_target_directory(workspace.path())?;
    fs::File::open(workspace.path().join("target"))?
        .set_times(fs::FileTimes::new().set_modified(UNIX_EPOCH))?;

    assert!(!cargo_diagnostics_workspace_matches_seed(
        workspace.path(),
        &seed_entries,
        &seed_modified_times,
        &seed_entry_identities,
        &seed_extended_attributes,
        &cargo_seed_inode_flags_without_target(workspace.path())?,
        Some(execution_window),
    )?);
    Ok(())
}
