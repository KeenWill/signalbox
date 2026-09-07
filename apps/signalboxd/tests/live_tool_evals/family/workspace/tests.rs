//! Workspace evaluation coverage.

use super::*;

#[test]
fn workspace_mutation_report_rejects_a_file_not_modified_denial() {
    let tracker = OperationTracker::default();
    tracker.observe_response_text(SYNTHETIC_FILE_NOT_MODIFIED_REPORT, false);

    assert!(!tracker.final_response_reports_completion_with_file_mutation());
}

#[test]
fn workspace_mutation_report_rejects_a_direct_file_edit_denial() {
    let tracker = OperationTracker::default();
    tracker.observe_response_text(SYNTHETIC_NO_FILE_EDITED_REPORT, false);

    assert!(!tracker.final_response_reports_completion_with_file_mutation());
}

#[test]
fn unforced_workspace_tier_reports_infrastructure_for_an_exact_known_failed_attempt() {
    let outcome = CaseOutcome {
        target: None,
        expected_arguments: None,
        execution_completed: true,
        forced_verification_failed: false,
        tool_results: vec![TrackedToolResult {
            request_id: Uuid::from_u128(ARBITRARY_EVAL_REQUEST_ID),
            content: String::from("fixture result"),
            is_error: true,
            round_tripped: true,
        }],
        snapshot: CaseSnapshot {
            turn_disposition: SnapshotTurnDisposition::Completed,
            requests: vec![RequestSnapshot {
                request_id: Uuid::from_u128(ARBITRARY_EVAL_REQUEST_ID),
                producing_model_call_id: Uuid::from_u128(ARBITRARY_EVAL_MODEL_CALL_ID),
                name: String::from(WRITE_FILE_NAME),
                arguments_text: serde_json::json!({
                    "path": WORKSPACE_ANSWER_PATH,
                    "content": WORKSPACE_ANSWER,
                })
                .to_string(),
                entry_index: ARBITRARY_REQUEST_ENTRY_INDEX,
                completed_result_entry_index: None,
                attempt_succeeded: false,
                attempt_denied: false,
            }],
            model_calls: MINIMUM_MODEL_CALLS_FOR_RESULT_ROUND_TRIP,
        },
    };

    assert_eq!(
        outcome.natural_loop_disposition(EvalFamily::Workspace),
        EvalDisposition::Infrastructure
    );
}

#[test]
fn unforced_workspace_tier_keeps_a_model_caused_read_failure_as_a_miss() {
    let outcome = CaseOutcome {
        target: None,
        expected_arguments: None,
        execution_completed: true,
        forced_verification_failed: false,
        tool_results: vec![TrackedToolResult {
            request_id: Uuid::from_u128(ARBITRARY_EVAL_REQUEST_ID),
            content: String::from("fixture result"),
            is_error: false,
            round_tripped: true,
        }],
        snapshot: CaseSnapshot {
            turn_disposition: SnapshotTurnDisposition::Completed,
            requests: vec![
                RequestSnapshot {
                    request_id: Uuid::from_u128(ARBITRARY_EVAL_REQUEST_ID),
                    producing_model_call_id: Uuid::from_u128(ARBITRARY_EVAL_MODEL_CALL_ID),
                    name: String::from(WRITE_FILE_NAME),
                    arguments_text: serde_json::json!({
                        "path": WORKSPACE_SEED_PATH,
                        "content": WORKSPACE_ANSWER,
                    })
                    .to_string(),
                    entry_index: ARBITRARY_REQUEST_ENTRY_INDEX,
                    completed_result_entry_index: Some(ARBITRARY_COMPLETED_RESULT_ENTRY_INDEX),
                    attempt_succeeded: true,
                    attempt_denied: false,
                },
                RequestSnapshot {
                    request_id: Uuid::from_u128(ARBITRARY_SECOND_EVAL_REQUEST_ID),
                    producing_model_call_id: Uuid::from_u128(ARBITRARY_SECOND_EVAL_MODEL_CALL_ID),
                    name: String::from(READ_FILE_NAME),
                    arguments_text: serde_json::json!({"path": WORKSPACE_SEED_PATH}).to_string(),
                    entry_index: ARBITRARY_REQUEST_ENTRY_INDEX,
                    completed_result_entry_index: None,
                    attempt_succeeded: false,
                    attempt_denied: false,
                },
            ],
            model_calls: MINIMUM_MODEL_CALLS_FOR_RESULT_ROUND_TRIP,
        },
    };

    assert_eq!(
        outcome.natural_loop_disposition(EvalFamily::Workspace),
        EvalDisposition::Miss
    );
    assert!(
        reject_natural_executor_failure(&outcome, EvalFamily::Workspace, EvalDisposition::Pass)
            .is_ok()
    );
}

#[test]
fn unforced_workspace_tier_reports_read_failure_after_an_unrelated_mutation_as_infrastructure() {
    let outcome = CaseOutcome {
        target: None,
        expected_arguments: None,
        execution_completed: true,
        forced_verification_failed: false,
        tool_results: vec![TrackedToolResult {
            request_id: Uuid::from_u128(ARBITRARY_EVAL_REQUEST_ID),
            content: String::from("fixture result"),
            is_error: false,
            round_tripped: true,
        }],
        snapshot: CaseSnapshot {
            turn_disposition: SnapshotTurnDisposition::Completed,
            requests: vec![
                RequestSnapshot {
                    request_id: Uuid::from_u128(ARBITRARY_EVAL_REQUEST_ID),
                    producing_model_call_id: Uuid::from_u128(ARBITRARY_EVAL_MODEL_CALL_ID),
                    name: String::from(WRITE_FILE_NAME),
                    arguments_text: serde_json::json!({
                        "path": WORKSPACE_ANSWER_PATH,
                        "content": WORKSPACE_ANSWER,
                    })
                    .to_string(),
                    entry_index: ARBITRARY_REQUEST_ENTRY_INDEX,
                    completed_result_entry_index: Some(ARBITRARY_COMPLETED_RESULT_ENTRY_INDEX),
                    attempt_succeeded: true,
                    attempt_denied: false,
                },
                RequestSnapshot {
                    request_id: Uuid::from_u128(ARBITRARY_SECOND_EVAL_REQUEST_ID),
                    producing_model_call_id: Uuid::from_u128(ARBITRARY_SECOND_EVAL_MODEL_CALL_ID),
                    name: String::from(READ_FILE_NAME),
                    arguments_text: bounded_workspace_read_arguments().to_string(),
                    entry_index: ARBITRARY_REQUEST_ENTRY_INDEX,
                    completed_result_entry_index: None,
                    attempt_succeeded: false,
                    attempt_denied: false,
                },
            ],
            model_calls: MINIMUM_MODEL_CALLS_FOR_RESULT_ROUND_TRIP,
        },
    };

    assert_eq!(
        outcome.natural_loop_disposition(EvalFamily::Workspace),
        EvalDisposition::Infrastructure
    );
    assert!(
        reject_natural_executor_failure(&outcome, EvalFamily::Workspace, EvalDisposition::Pass)
            .is_err()
    );
}

#[test]
fn unforced_workspace_tier_rejects_more_than_the_bounded_model_calls() {
    let first = Uuid::from_u128(ARBITRARY_EVAL_REQUEST_ID);
    let second = Uuid::from_u128(ARBITRARY_SECOND_EVAL_REQUEST_ID);
    let third = Uuid::from_u128(ARBITRARY_THIRD_EVAL_REQUEST_ID);
    let fourth = Uuid::from_u128(ARBITRARY_FOURTH_EVAL_REQUEST_ID);
    let outcome = CaseOutcome {
        target: None,
        expected_arguments: None,
        execution_completed: true,
        forced_verification_failed: false,
        tool_results: vec![
            round_tripped_fixture_result(first),
            round_tripped_fixture_result(second),
            round_tripped_fixture_result(third),
            round_tripped_fixture_result(fourth),
        ],
        snapshot: CaseSnapshot {
            turn_disposition: SnapshotTurnDisposition::Completed,
            requests: vec![
                successful_request(first, LIST_DIRECTORY_NAME, serde_json::json!({"path": "."})),
                successful_request(second, GLOB_FILES_NAME, serde_json::json!({"pattern": "*"})),
                successful_request(
                    third,
                    READ_FILE_NAME,
                    serde_json::json!({"path": WORKSPACE_SEED_PATH}),
                ),
                successful_request(
                    fourth,
                    WRITE_FILE_NAME,
                    serde_json::json!({
                        "path": WORKSPACE_ANSWER_PATH,
                        "content": WORKSPACE_ANSWER,
                    }),
                ),
            ],
            model_calls: MAX_NATURAL_MODEL_CALLS + 1,
        },
    };

    assert_eq!(
        outcome.natural_loop_disposition(EvalFamily::Workspace),
        EvalDisposition::Miss
    );
}

#[test]
fn unforced_workspace_tier_keeps_an_out_of_range_read_as_a_miss() {
    let snapshot = failed_request_snapshot(
        READ_FILE_NAME,
        serde_json::json!({"path": WORKSPACE_SEED_PATH, "max_bytes": 0}),
    );

    assert!(!snapshot.exact_natural_request_failed(EvalFamily::Workspace));
}

#[test]
fn unforced_workspace_tier_reports_a_covering_bounded_read_failure_as_infrastructure() {
    let snapshot = failed_request_snapshot(READ_FILE_NAME, bounded_workspace_read_arguments());

    assert!(snapshot.exact_natural_request_failed(EvalFamily::Workspace));
}

#[test]
fn unforced_workspace_tier_keeps_an_unknown_read_field_as_a_miss() {
    let mut arguments = bounded_workspace_read_arguments();
    arguments["unexpected"] = serde_json::json!(true);
    let snapshot = failed_request_snapshot(READ_FILE_NAME, arguments);

    assert!(!snapshot.exact_natural_request_failed(EvalFamily::Workspace));
}

#[test]
fn unforced_workspace_tier_keeps_a_malformed_read_bound_as_a_miss() {
    let mut arguments = bounded_workspace_read_arguments();
    arguments["max_bytes"] = serde_json::json!("many");
    let snapshot = failed_request_snapshot(READ_FILE_NAME, arguments);

    assert!(!snapshot.exact_natural_request_failed(EvalFamily::Workspace));
}

#[test]
fn unforced_workspace_tier_keeps_an_undersized_read_bound_as_a_miss() {
    let mut arguments = bounded_workspace_read_arguments();
    arguments["max_bytes"] = serde_json::json!(WORKSPACE_SEED.len() - 1);
    let snapshot = failed_request_snapshot(READ_FILE_NAME, arguments);

    assert!(!snapshot.exact_natural_request_failed(EvalFamily::Workspace));
}

#[test]
fn unforced_workspace_tier_keeps_an_oversized_read_bound_as_a_miss() {
    let mut arguments = bounded_workspace_read_arguments();
    arguments["max_bytes"] = serde_json::json!(MAX_WORKSPACE_READ_BYTES + 1);
    let snapshot = failed_request_snapshot(READ_FILE_NAME, arguments);

    assert!(!snapshot.exact_natural_request_failed(EvalFamily::Workspace));
}

#[test]
fn unforced_workspace_tier_requires_each_request_result_to_round_trip() {
    let outcome = CaseOutcome {
        target: None,
        expected_arguments: None,
        execution_completed: true,
        forced_verification_failed: false,
        tool_results: vec![TrackedToolResult {
            request_id: Uuid::from_u128(ARBITRARY_EVAL_REQUEST_ID),
            content: String::from("fixture result"),
            is_error: false,
            round_tripped: true,
        }],
        snapshot: CaseSnapshot {
            turn_disposition: SnapshotTurnDisposition::Completed,
            requests: vec![
                RequestSnapshot {
                    request_id: Uuid::from_u128(ARBITRARY_EVAL_REQUEST_ID),
                    producing_model_call_id: Uuid::from_u128(ARBITRARY_EVAL_MODEL_CALL_ID),
                    name: String::from(READ_FILE_NAME),
                    arguments_text: serde_json::json!({"path": WORKSPACE_SEED_PATH}).to_string(),
                    entry_index: ARBITRARY_REQUEST_ENTRY_INDEX,
                    completed_result_entry_index: Some(ARBITRARY_COMPLETED_RESULT_ENTRY_INDEX),
                    attempt_succeeded: true,
                    attempt_denied: false,
                },
                RequestSnapshot {
                    request_id: Uuid::from_u128(ARBITRARY_SECOND_EVAL_REQUEST_ID),
                    producing_model_call_id: Uuid::from_u128(ARBITRARY_SECOND_EVAL_MODEL_CALL_ID),
                    name: String::from(WRITE_FILE_NAME),
                    arguments_text: serde_json::json!({
                        "path": WORKSPACE_ANSWER_PATH,
                        "content": WORKSPACE_ANSWER,
                    })
                    .to_string(),
                    entry_index: ARBITRARY_REQUEST_ENTRY_INDEX,
                    completed_result_entry_index: Some(ARBITRARY_COMPLETED_RESULT_ENTRY_INDEX),
                    attempt_succeeded: true,
                    attempt_denied: false,
                },
            ],
            model_calls: MINIMUM_MODEL_CALLS_FOR_RESULT_ROUND_TRIP,
        },
    };

    assert_eq!(
        outcome.natural_loop_disposition(EvalFamily::Workspace),
        EvalDisposition::Miss
    );
}

#[test]
fn forced_workspace_write_verifier_rejects_collateral_mutation() -> EvalResult {
    let suite = FamilySuite::workspace()?;
    let case = WORKSPACE_CASES
        .iter()
        .find(|case| case.name == WRITE_FILE_NAME)
        .expect("the workspace write fixture exists");
    let arguments: serde_json::Value = serde_json::from_str(case.expected_arguments)?;
    let path = arguments["path"]
        .as_str()
        .expect("the workspace write fixture has a path");
    let content = arguments["content"]
        .as_str()
        .expect("the workspace write fixture has content");
    fs::write(suite.workspace.path().join(path), content)?;
    fs::write(
        suite.workspace.path().join(WORKSPACE_SEED_PATH),
        WORKSPACE_DRIFTED_SEED,
    )?;
    let result = serde_json::json!({
        "path": path,
        "bytes_written": content.len(),
        "created": true,
        EVAL_RECEIPT_FIELD: SYNTHETIC_EVAL_RECEIPT,
    })
    .to_string();

    assert!(!suite.forced_case_result_passed(case, &result)?);
    Ok(())
}

#[test]
fn forced_workspace_write_verifier_rejects_a_deleted_unrelated_fixture() -> EvalResult {
    let suite = FamilySuite::workspace()?;
    let case = WORKSPACE_CASES
        .iter()
        .find(|case| case.name == WRITE_FILE_NAME)
        .expect("the workspace write fixture exists");
    let arguments: serde_json::Value = serde_json::from_str(case.expected_arguments)?;
    let path = arguments["path"]
        .as_str()
        .expect("the workspace write fixture has a path");
    let content = arguments["content"]
        .as_str()
        .expect("the workspace write fixture has content");
    fs::write(suite.workspace.path().join(path), content)?;
    fs::remove_file(suite.workspace.path().join(WORKSPACE_GLOB_PATH))?;
    let result = serde_json::json!({
        "path": path,
        "bytes_written": content.len(),
        "created": true,
        EVAL_RECEIPT_FIELD: SYNTHETIC_EVAL_RECEIPT,
    })
    .to_string();

    assert!(!suite.forced_case_result_passed(case, &result)?);
    Ok(())
}

#[cfg(unix)]
#[test]
fn forced_workspace_write_verifier_accepts_the_private_creation_mode() -> EvalResult {
    let suite = FamilySuite::workspace()?;
    let case = WORKSPACE_CASES
        .iter()
        .find(|case| case.name == WRITE_FILE_NAME)
        .expect("the workspace write fixture exists");
    let arguments: serde_json::Value = serde_json::from_str(case.expected_arguments)?;
    let path = arguments["path"]
        .as_str()
        .expect("the workspace write fixture has a path");
    let content = arguments["content"]
        .as_str()
        .expect("the workspace write fixture has content");
    let target = suite.workspace.path().join(path);
    let started = current_filesystem_recorded_time()?;
    fs::write(&target, content)?;
    fs::set_permissions(
        &target,
        fs::Permissions::from_mode(WORKSPACE_PRIVATE_CREATION_MODE),
    )?;
    suite.executor.record_filesystem_execution_window(
        WRITE_FILE_NAME,
        FilesystemExecutionTimeWindow {
            started,
            finished: current_filesystem_recorded_time()?,
        },
    );
    let result = serde_json::json!({
        "path": path,
        "bytes_written": content.len(),
        "created": true,
        EVAL_RECEIPT_FIELD: SYNTHETIC_EVAL_RECEIPT,
    })
    .to_string();

    assert!(suite.forced_case_result_passed(case, &result)?);
    Ok(())
}

#[cfg(unix)]
#[test]
fn workspace_mutation_identity_gate_rejects_changed_target_ownership() -> EvalResult {
    let suite = FamilySuite::workspace()?;
    let target = Path::new(WORKSPACE_SEED_PATH);
    let mut actual = suite.workspace_seed_entry_identities.clone();
    let changed_group_id = actual[target].group_id.wrapping_add(1);
    actual
        .get_mut(target)
        .expect("the workspace seed has a filesystem identity")
        .group_id = changed_group_id;

    assert!(!entry_identities_match_except(
        actual,
        &suite.workspace_seed_entry_identities,
        &[target],
    ));
    Ok(())
}

#[cfg(unix)]
#[test]
fn workspace_mutation_identity_gate_rejects_changed_new_target_ownership() -> EvalResult {
    let suite = FamilySuite::workspace()?;
    let target = Path::new("created-during-execution.txt");
    let mut actual = suite.workspace_seed_entry_identities.clone();
    let mut target_identity = actual[Path::new("")];
    target_identity.group_id = target_identity.group_id.wrapping_add(1);
    actual.insert(target.to_path_buf(), target_identity);

    assert!(!entry_identities_match_except(
        actual,
        &suite.workspace_seed_entry_identities,
        &[target],
    ));
    Ok(())
}

#[cfg(unix)]
#[test]
fn forced_workspace_write_verifier_rejects_an_insecure_creation_mode() -> EvalResult {
    let suite = FamilySuite::workspace()?;
    let case = WORKSPACE_CASES
        .iter()
        .find(|case| case.name == WRITE_FILE_NAME)
        .expect("the workspace write fixture exists");
    let arguments: serde_json::Value = serde_json::from_str(case.expected_arguments)?;
    let path = arguments["path"]
        .as_str()
        .expect("the workspace write fixture has a path");
    let content = arguments["content"]
        .as_str()
        .expect("the workspace write fixture has content");
    let target = suite.workspace.path().join(path);
    fs::write(&target, content)?;
    fs::set_permissions(
        &target,
        fs::Permissions::from_mode(WORKSPACE_INSECURE_CREATION_MODE),
    )?;
    let result = serde_json::json!({
        "path": path,
        "bytes_written": content.len(),
        "created": true,
        EVAL_RECEIPT_FIELD: SYNTHETIC_EVAL_RECEIPT,
    })
    .to_string();

    assert!(!suite.forced_case_result_passed(case, &result)?);
    Ok(())
}

#[test]
fn forced_workspace_apply_patch_verifier_rejects_collateral_mutation() -> EvalResult {
    let suite = FamilySuite::workspace()?;
    let case = WORKSPACE_CASES
        .iter()
        .find(|case| case.name == APPLY_PATCH_NAME)
        .expect("the workspace apply-patch fixture exists");
    fs::write(
        suite.workspace.path().join("patched.txt"),
        "patched by eval\n",
    )?;
    fs::write(
        suite.workspace.path().join(WORKSPACE_SEED_PATH),
        WORKSPACE_DRIFTED_SEED,
    )?;
    let result = serde_json::json!({
        "operations_applied": 1,
        EVAL_RECEIPT_FIELD: SYNTHETIC_EVAL_RECEIPT,
    })
    .to_string();

    assert!(!suite.forced_case_result_passed(case, &result)?);
    Ok(())
}

#[cfg(unix)]
#[test]
fn forced_workspace_apply_patch_verifier_rejects_an_insecure_creation_mode() -> EvalResult {
    let suite = FamilySuite::workspace()?;
    let case = WORKSPACE_CASES
        .iter()
        .find(|case| case.name == APPLY_PATCH_NAME)
        .expect("the workspace apply-patch fixture exists");
    let target = suite.workspace.path().join("patched.txt");
    fs::write(&target, "patched by eval\n")?;
    fs::set_permissions(
        &target,
        fs::Permissions::from_mode(WORKSPACE_INSECURE_CREATION_MODE),
    )?;
    let result = serde_json::json!({
        "operations_applied": 1,
        "truncated": true,
        EVAL_RECEIPT_FIELD: SYNTHETIC_EVAL_RECEIPT,
    })
    .to_string();

    assert!(!suite.forced_case_result_passed(case, &result)?);
    Ok(())
}

#[test]
fn forced_workspace_edit_verifier_rejects_collateral_mutation() -> EvalResult {
    let suite = FamilySuite::workspace()?;
    let case = WORKSPACE_CASES
        .iter()
        .find(|case| case.name == EDIT_FILE_NAME)
        .expect("the workspace edit fixture exists");
    let arguments: serde_json::Value = serde_json::from_str(case.expected_arguments)?;
    let old = arguments["old_string"]
        .as_str()
        .expect("the edit fixture has an old string");
    let new = arguments["new_string"]
        .as_str()
        .expect("the edit fixture has a new string");
    let expected = WORKSPACE_SEED.replace(old, new);
    fs::write(suite.workspace.path().join(WORKSPACE_SEED_PATH), &expected)?;
    fs::write(
        suite.workspace.path().join(WORKSPACE_GLOB_PATH),
        WORKSPACE_DRIFTED_GLOB_CONTENT,
    )?;
    let result = serde_json::json!({
        "path": WORKSPACE_SEED_PATH,
        "replacements": WORKSPACE_SEED.match_indices(old).count(),
        "bytes_written": expected.len(),
        EVAL_RECEIPT_FIELD: SYNTHETIC_EVAL_RECEIPT,
    })
    .to_string();

    assert!(!suite.forced_case_result_passed(case, &result)?);
    Ok(())
}

#[test]
fn forced_workspace_edit_verifier_rejects_collateral_mtime_drift() -> EvalResult {
    let suite = FamilySuite::workspace()?;
    let case = WORKSPACE_CASES
        .iter()
        .find(|case| case.name == EDIT_FILE_NAME)
        .expect("the workspace edit fixture exists");
    fs::write(
        suite.workspace.path().join(WORKSPACE_SEED_PATH),
        WORKSPACE_EDITED_SEED,
    )?;
    let collateral = suite.workspace.path().join(WORKSPACE_GLOB_PATH);
    fs::File::open(collateral)?.set_times(fs::FileTimes::new().set_modified(UNIX_EPOCH))?;
    let result = serde_json::json!({
        "path": WORKSPACE_SEED_PATH,
        "replacements": EXPECTED_WORKSPACE_EDIT_REPLACEMENTS,
        "bytes_written": EXPECTED_WORKSPACE_EDIT_BYTES,
        EVAL_RECEIPT_FIELD: SYNTHETIC_EVAL_RECEIPT,
    })
    .to_string();

    assert!(!suite.forced_case_result_passed(case, &result)?);
    Ok(())
}

#[cfg(unix)]
#[test]
fn forced_workspace_edit_verifier_rejects_a_mode_change() -> EvalResult {
    let suite = FamilySuite::workspace()?;
    let case = WORKSPACE_CASES
        .iter()
        .find(|case| case.name == EDIT_FILE_NAME)
        .expect("the workspace edit fixture exists");
    let arguments: serde_json::Value = serde_json::from_str(case.expected_arguments)?;
    let old = arguments["old_string"]
        .as_str()
        .expect("the edit fixture has an old string");
    let new = arguments["new_string"]
        .as_str()
        .expect("the edit fixture has a new string");
    let expected = WORKSPACE_SEED.replace(old, new);
    let path = suite.workspace.path().join(WORKSPACE_SEED_PATH);
    fs::write(&path, &expected)?;
    let mut permissions = fs::metadata(&path)?.permissions();
    permissions.set_mode(permissions.mode() ^ USER_EXECUTE_MODE_BIT);
    fs::set_permissions(&path, permissions)?;
    let result = serde_json::json!({
        "path": WORKSPACE_SEED_PATH,
        "replacements": WORKSPACE_SEED.match_indices(old).count(),
        "bytes_written": expected.len(),
        EVAL_RECEIPT_FIELD: SYNTHETIC_EVAL_RECEIPT,
    })
    .to_string();

    assert!(!suite.forced_case_result_passed(case, &result)?);
    Ok(())
}

#[cfg(target_os = "linux")]
#[test]
fn forced_workspace_edit_verifier_rejects_an_added_extended_attribute() -> EvalResult {
    let suite = FamilySuite::workspace()?;
    let case = WORKSPACE_CASES
        .iter()
        .find(|case| case.name == EDIT_FILE_NAME)
        .expect("the workspace edit fixture exists");
    let relative = Path::new(WORKSPACE_SEED_PATH);
    let path = suite.workspace.path().join(relative);
    fs::write(&path, WORKSPACE_EDITED_SEED)?;
    rustix::fs::setxattr(
        &path,
        SYNTHETIC_UNEXPECTED_XATTR_NAME,
        SYNTHETIC_UNEXPECTED_XATTR_VALUE,
        rustix::fs::XattrFlags::CREATE,
    )?;
    let result = serde_json::json!({
        "path": WORKSPACE_SEED_PATH,
        "replacements": EXPECTED_WORKSPACE_EDIT_REPLACEMENTS,
        "bytes_written": EXPECTED_WORKSPACE_EDIT_BYTES,
        EVAL_RECEIPT_FIELD: SYNTHETIC_EVAL_RECEIPT,
    })
    .to_string();

    assert_ne!(
        workspace_extended_attributes(suite.workspace.path())?[relative],
        suite.workspace_seed_extended_attributes[relative]
    );
    assert!(!suite.forced_case_result_passed(case, &result)?);
    Ok(())
}

#[test]
fn forced_workspace_search_verifier_rejects_an_empty_success() -> EvalResult {
    let suite = FamilySuite::workspace()?;
    let case = WORKSPACE_CASES
        .iter()
        .find(|case| case.name == SEARCH_FILES_NAME)
        .expect("the workspace search fixture exists");
    let result = serde_json::json!({
        "matches": [],
        "truncated": false,
        EVAL_RECEIPT_FIELD: SYNTHETIC_EVAL_RECEIPT,
    })
    .to_string();

    assert!(!suite.forced_case_result_passed(case, &result)?);
    Ok(())
}

#[test]
fn forced_workspace_search_verifier_accepts_the_bounded_first_match() -> EvalResult {
    let suite = FamilySuite::workspace()?;
    let case = WORKSPACE_CASES
        .iter()
        .find(|case| case.name == SEARCH_FILES_NAME)
        .expect("the workspace search fixture exists");
    let match_line = WORKSPACE_SEARCH_CONTENT
        .lines()
        .nth(1)
        .expect("the workspace search fixture has a matching line");
    let result = serde_json::json!({
        "matches": [{
            "path": WORKSPACE_SEARCH_PATH,
            "line": 2,
            "column": 1,
            "text_start_column": 1,
            "text": match_line,
            "line_truncated": false,
        }],
        "truncated": true,
        EVAL_RECEIPT_FIELD: SYNTHETIC_EVAL_RECEIPT,
    })
    .to_string();

    assert!(suite.forced_case_result_passed(case, &result)?);
    Ok(())
}

#[test]
fn forced_workspace_search_verifier_rejects_collateral_mutation() -> EvalResult {
    let suite = FamilySuite::workspace()?;
    let case = WORKSPACE_CASES
        .iter()
        .find(|case| case.name == SEARCH_FILES_NAME)
        .expect("the workspace search fixture exists");
    fs::write(
        suite.workspace.path().join(WORKSPACE_SEED_PATH),
        WORKSPACE_DRIFTED_SEED,
    )?;
    let match_line = WORKSPACE_SEARCH_CONTENT
        .lines()
        .nth(1)
        .expect("the workspace search fixture has a matching line");
    let result = serde_json::json!({
        "matches": [{
            "path": WORKSPACE_SEARCH_PATH,
            "line": 2,
            "column": 1,
            "text_start_column": 1,
            "text": match_line,
            "line_truncated": false,
        }],
        "truncated": true,
        EVAL_RECEIPT_FIELD: SYNTHETIC_EVAL_RECEIPT,
    })
    .to_string();

    assert!(!suite.forced_case_result_passed(case, &result)?);
    Ok(())
}

#[test]
fn forced_workspace_search_verifier_rejects_a_root_match() -> EvalResult {
    let suite = FamilySuite::workspace()?;
    let case = WORKSPACE_CASES
        .iter()
        .find(|case| case.name == SEARCH_FILES_NAME)
        .expect("the workspace search fixture exists");
    let root_match = WORKSPACE_SEED
        .lines()
        .nth(1)
        .expect("the root fixture has a matching line");
    let result = serde_json::json!({
        "matches": [{
            "path": WORKSPACE_SEED_PATH,
            "line": 2,
            "column": 1,
            "text_start_column": 1,
            "text": root_match,
            "line_truncated": false,
        }],
        "truncated": true,
        EVAL_RECEIPT_FIELD: SYNTHETIC_EVAL_RECEIPT,
    })
    .to_string();

    assert!(!suite.forced_case_result_passed(case, &result)?);
    Ok(())
}

#[test]
fn forced_workspace_read_verifier_rejects_an_unbounded_result() -> EvalResult {
    let suite = FamilySuite::workspace()?;
    let case = WORKSPACE_CASES
        .iter()
        .find(|case| case.name == READ_FILE_NAME)
        .expect("the workspace read fixture exists");
    let result = serde_json::json!({
        "path": WORKSPACE_SEED_PATH,
        "content": WORKSPACE_SEED,
        "offset": 0,
        "bytes_read": WORKSPACE_SEED.len(),
        "next_offset": WORKSPACE_SEED.len(),
        "total_bytes": WORKSPACE_SEED.len(),
        "truncated": false,
        EVAL_RECEIPT_FIELD: SYNTHETIC_EVAL_RECEIPT,
    })
    .to_string();

    assert!(!suite.forced_case_result_passed(case, &result)?);
    Ok(())
}

#[test]
fn forced_workspace_read_verifier_rejects_an_unknown_result_field() -> EvalResult {
    let suite = FamilySuite::workspace()?;
    let case = WORKSPACE_CASES
        .iter()
        .find(|case| case.name == READ_FILE_NAME)
        .expect("the workspace read fixture exists");
    let prefix = WORKSPACE_SEED
        .get(..WORKSPACE_FORCED_READ_MAX_BYTES)
        .expect("the seeded workspace fixture covers the forced bound");
    let result = serde_json::json!({
        "path": WORKSPACE_SEED_PATH,
        "content": prefix,
        "offset": 0,
        "bytes_read": prefix.len(),
        "next_offset": prefix.len(),
        "total_bytes": WORKSPACE_SEED.len(),
        "truncated": true,
        "error": "synthetic contradictory field",
        EVAL_RECEIPT_FIELD: SYNTHETIC_EVAL_RECEIPT,
    })
    .to_string();

    assert!(!suite.forced_case_result_passed(case, &result)?);
    Ok(())
}

#[test]
fn forced_workspace_read_verifier_rejects_a_mutated_fixture() -> EvalResult {
    let suite = FamilySuite::workspace()?;
    let case = WORKSPACE_CASES
        .iter()
        .find(|case| case.name == READ_FILE_NAME)
        .expect("the workspace read fixture exists");
    fs::write(
        suite.workspace.path().join(WORKSPACE_SEED_PATH),
        WORKSPACE_DRIFTED_SEED,
    )?;
    let prefix = WORKSPACE_DRIFTED_SEED
        .get(..WORKSPACE_FORCED_READ_MAX_BYTES)
        .expect("the drifted workspace fixture covers the forced bound");
    let result = serde_json::json!({
        "path": WORKSPACE_SEED_PATH,
        "content": prefix,
        "offset": 0,
        "bytes_read": prefix.len(),
        "next_offset": prefix.len(),
        "total_bytes": WORKSPACE_DRIFTED_SEED.len(),
        "truncated": true,
        EVAL_RECEIPT_FIELD: SYNTHETIC_EVAL_RECEIPT,
    })
    .to_string();

    assert!(!suite.forced_case_result_passed(case, &result)?);
    Ok(())
}

#[test]
fn forced_workspace_read_verifier_rejects_collateral_mutation() -> EvalResult {
    let suite = FamilySuite::workspace()?;
    let case = WORKSPACE_CASES
        .iter()
        .find(|case| case.name == READ_FILE_NAME)
        .expect("the workspace read fixture exists");
    fs::write(
        suite.workspace.path().join(WORKSPACE_GLOB_NONMATCHING_PATH),
        WORKSPACE_DRIFTED_GLOB_CONTENT,
    )?;
    let prefix = WORKSPACE_SEED
        .get(..WORKSPACE_FORCED_READ_MAX_BYTES)
        .expect("the workspace fixture covers the forced bound");
    let result = serde_json::json!({
        "path": WORKSPACE_SEED_PATH,
        "content": prefix,
        "offset": 0,
        "bytes_read": prefix.len(),
        "next_offset": prefix.len(),
        "total_bytes": WORKSPACE_SEED.len(),
        "truncated": true,
        EVAL_RECEIPT_FIELD: SYNTHETIC_EVAL_RECEIPT,
    })
    .to_string();

    assert!(!suite.forced_case_result_passed(case, &result)?);
    Ok(())
}

#[test]
fn forced_workspace_read_verifier_rejects_collateral_mtime_drift() -> EvalResult {
    let suite = FamilySuite::workspace()?;
    let case = WORKSPACE_CASES
        .iter()
        .find(|case| case.name == READ_FILE_NAME)
        .expect("the workspace read fixture exists");
    let collateral = suite.workspace.path().join(WORKSPACE_GLOB_NONMATCHING_PATH);
    fs::File::open(collateral)?.set_times(fs::FileTimes::new().set_modified(UNIX_EPOCH))?;
    let prefix = WORKSPACE_SEED
        .get(..WORKSPACE_FORCED_READ_MAX_BYTES)
        .expect("the workspace fixture covers the forced bound");
    let result = serde_json::json!({
        "path": WORKSPACE_SEED_PATH,
        "content": prefix,
        "offset": 0,
        "bytes_read": prefix.len(),
        "next_offset": prefix.len(),
        "total_bytes": WORKSPACE_SEED.len(),
        "truncated": true,
        EVAL_RECEIPT_FIELD: SYNTHETIC_EVAL_RECEIPT,
    })
    .to_string();

    assert!(!suite.forced_case_result_passed(case, &result)?);
    Ok(())
}

#[cfg(unix)]
#[test]
fn forced_workspace_read_verifier_rejects_restored_mode_ctime_drift() -> EvalResult {
    let suite = FamilySuite::workspace()?;
    let case = WORKSPACE_CASES
        .iter()
        .find(|case| case.name == READ_FILE_NAME)
        .expect("the workspace read fixture exists");
    let relative = Path::new(WORKSPACE_GLOB_NONMATCHING_PATH);
    let path = suite.workspace.path().join(relative);
    let original_permissions = fs::metadata(&path)?.permissions();
    let mut changed_permissions = original_permissions.clone();
    changed_permissions.set_mode(original_permissions.mode() ^ GROUP_WRITE_MODE_BIT);
    std::thread::sleep(Duration::from_millis(1));
    fs::set_permissions(&path, changed_permissions)?;
    fs::set_permissions(&path, original_permissions)?;
    let actual_identities = workspace_entry_identities(suite.workspace.path())?;
    let expected_identity = suite.workspace_seed_entry_identities[relative];
    let actual_identity = actual_identities[relative];
    let prefix = WORKSPACE_SEED
        .get(..WORKSPACE_FORCED_READ_MAX_BYTES)
        .expect("the workspace fixture covers the forced bound");
    let result = serde_json::json!({
        "path": WORKSPACE_SEED_PATH,
        "content": prefix,
        "offset": 0,
        "bytes_read": prefix.len(),
        "next_offset": prefix.len(),
        "total_bytes": WORKSPACE_SEED.len(),
        "truncated": true,
        EVAL_RECEIPT_FIELD: SYNTHETIC_EVAL_RECEIPT,
    })
    .to_string();

    assert_eq!(
        workspace_entries(suite.workspace.path())?,
        suite.workspace_seed_entries
    );
    assert_eq!(
        workspace_modified_times(suite.workspace.path())?,
        suite.workspace_seed_modified_times
    );
    assert_eq!(actual_identity.device, expected_identity.device);
    assert_eq!(actual_identity.inode, expected_identity.inode);
    assert_ne!(actual_identity, expected_identity);
    assert!(!suite.forced_case_result_passed(case, &result)?);
    Ok(())
}

#[cfg(unix)]
#[test]
fn forced_workspace_read_verifier_rejects_byte_identical_file_replacement() -> EvalResult {
    let suite = FamilySuite::workspace()?;
    let case = WORKSPACE_CASES
        .iter()
        .find(|case| case.name == READ_FILE_NAME)
        .expect("the workspace read fixture exists");
    let target = suite.workspace.path().join(WORKSPACE_SEED_PATH);
    let replacement = suite.workspace.path().join("replacement-fixture");
    let target_modified = *suite
        .workspace_seed_modified_times
        .get(Path::new(WORKSPACE_SEED_PATH))
        .expect("the workspace seed file has a captured modified time");
    let root_modified = *suite
        .workspace_seed_modified_times
        .get(Path::new(""))
        .expect("the workspace root has a captured modified time");
    let permissions = fs::metadata(&target)?.permissions();
    fs::write(&replacement, WORKSPACE_SEED)?;
    fs::set_permissions(&replacement, permissions)?;
    fs::rename(&replacement, &target)?;
    fs::File::open(&target)?.set_times(fs::FileTimes::new().set_modified(target_modified))?;
    fs::File::open(suite.workspace.path())?
        .set_times(fs::FileTimes::new().set_modified(root_modified))?;
    let prefix = WORKSPACE_SEED
        .get(..WORKSPACE_FORCED_READ_MAX_BYTES)
        .expect("the workspace fixture covers the forced bound");
    let result = serde_json::json!({
        "path": WORKSPACE_SEED_PATH,
        "content": prefix,
        "offset": 0,
        "bytes_read": prefix.len(),
        "next_offset": prefix.len(),
        "total_bytes": WORKSPACE_SEED.len(),
        "truncated": true,
        EVAL_RECEIPT_FIELD: SYNTHETIC_EVAL_RECEIPT,
    })
    .to_string();

    assert_eq!(
        workspace_entries(suite.workspace.path())?,
        suite.workspace_seed_entries
    );
    assert_eq!(
        workspace_modified_times(suite.workspace.path())?,
        suite.workspace_seed_modified_times
    );
    assert_ne!(
        workspace_entry_identities(suite.workspace.path())?,
        suite.workspace_seed_entry_identities
    );
    assert!(!suite.forced_case_result_passed(case, &result)?);
    Ok(())
}

#[cfg(unix)]
#[test]
fn forced_workspace_read_verifier_rejects_byte_identical_directory_replacement() -> EvalResult {
    let suite = FamilySuite::workspace()?;
    let case = WORKSPACE_CASES
        .iter()
        .find(|case| case.name == READ_FILE_NAME)
        .expect("the workspace read fixture exists");
    let relative = Path::new(WORKSPACE_GLOB_DIRECTORY);
    let target = suite.workspace.path().join(relative);
    let moved = suite.workspace.path().join("moved-glob-scope");
    let target_modified = suite.workspace_seed_modified_times[relative];
    let root_modified = suite.workspace_seed_modified_times[Path::new("")];
    let permissions = fs::metadata(&target)?.permissions();
    let matching_name = Path::new(WORKSPACE_GLOB_PATH)
        .file_name()
        .expect("the matching fixture has a filename");
    let overflow_name = Path::new(WORKSPACE_GLOB_OVERFLOW_PATH)
        .file_name()
        .expect("the overflow fixture has a filename");
    let nonmatching_name = Path::new(WORKSPACE_GLOB_NONMATCHING_PATH)
        .file_name()
        .expect("the nonmatching fixture has a filename");
    fs::rename(&target, &moved)?;
    fs::create_dir(&target)?;
    fs::set_permissions(&target, permissions)?;
    fs::hard_link(moved.join(matching_name), target.join(matching_name))?;
    fs::hard_link(moved.join(overflow_name), target.join(overflow_name))?;
    fs::hard_link(moved.join(nonmatching_name), target.join(nonmatching_name))?;
    fs::remove_file(moved.join(matching_name))?;
    fs::remove_file(moved.join(overflow_name))?;
    fs::remove_file(moved.join(nonmatching_name))?;
    fs::remove_dir(moved)?;
    fs::File::open(&target)?.set_times(fs::FileTimes::new().set_modified(target_modified))?;
    fs::File::open(suite.workspace.path())?
        .set_times(fs::FileTimes::new().set_modified(root_modified))?;
    let prefix = WORKSPACE_SEED
        .get(..WORKSPACE_FORCED_READ_MAX_BYTES)
        .expect("the workspace fixture covers the forced bound");
    let result = serde_json::json!({
        "path": WORKSPACE_SEED_PATH,
        "content": prefix,
        "offset": 0,
        "bytes_read": prefix.len(),
        "next_offset": prefix.len(),
        "total_bytes": WORKSPACE_SEED.len(),
        "truncated": true,
        EVAL_RECEIPT_FIELD: SYNTHETIC_EVAL_RECEIPT,
    })
    .to_string();

    assert_eq!(
        workspace_entries(suite.workspace.path())?,
        suite.workspace_seed_entries
    );
    assert_eq!(
        workspace_modified_times(suite.workspace.path())?,
        suite.workspace_seed_modified_times
    );
    assert_ne!(
        workspace_entry_identities(suite.workspace.path())?,
        suite.workspace_seed_entry_identities
    );
    assert!(!suite.forced_case_result_passed(case, &result)?);
    Ok(())
}

#[cfg(unix)]
#[test]
fn workspace_inventory_rejects_a_symlink_replacing_its_root() -> EvalResult {
    let parent = tempfile::tempdir()?;
    let root = parent.path().join("workspace-root");
    let moved = parent.path().join("moved-workspace");
    fs::create_dir(&root)?;
    fs::write(root.join(WORKSPACE_SEED_PATH), WORKSPACE_SEED)?;
    let expected = workspace_entries(&root)?;
    fs::rename(&root, &moved)?;
    symlink(&moved, &root)?;

    assert_ne!(workspace_entries(&root)?, expected);
    Ok(())
}

#[cfg(unix)]
#[test]
fn forced_workspace_read_verifier_rejects_directory_mode_drift() -> EvalResult {
    let suite = FamilySuite::workspace()?;
    let case = WORKSPACE_CASES
        .iter()
        .find(|case| case.name == READ_FILE_NAME)
        .expect("the workspace read fixture exists");
    let path = suite.workspace.path().join(WORKSPACE_GLOB_DIRECTORY);
    let mut permissions = fs::metadata(&path)?.permissions();
    permissions.set_mode(permissions.mode() ^ GROUP_WRITE_MODE_BIT);
    fs::set_permissions(&path, permissions)?;
    let prefix = WORKSPACE_SEED
        .get(..WORKSPACE_FORCED_READ_MAX_BYTES)
        .expect("the workspace fixture covers the forced bound");
    let result = serde_json::json!({
        "path": WORKSPACE_SEED_PATH,
        "content": prefix,
        "offset": 0,
        "bytes_read": prefix.len(),
        "next_offset": prefix.len(),
        "total_bytes": WORKSPACE_SEED.len(),
        "truncated": true,
        EVAL_RECEIPT_FIELD: SYNTHETIC_EVAL_RECEIPT,
    })
    .to_string();

    assert!(!suite.forced_case_result_passed(case, &result)?);
    Ok(())
}

#[cfg(unix)]
#[test]
fn forced_workspace_read_verifier_rejects_root_mode_drift() -> EvalResult {
    let suite = FamilySuite::workspace()?;
    let case = WORKSPACE_CASES
        .iter()
        .find(|case| case.name == READ_FILE_NAME)
        .expect("the workspace read fixture exists");
    let mut permissions = fs::metadata(suite.workspace.path())?.permissions();
    permissions.set_mode(permissions.mode() ^ GROUP_WRITE_MODE_BIT);
    fs::set_permissions(suite.workspace.path(), permissions)?;
    let prefix = WORKSPACE_SEED
        .get(..WORKSPACE_FORCED_READ_MAX_BYTES)
        .expect("the workspace fixture covers the forced bound");
    let result = serde_json::json!({
        "path": WORKSPACE_SEED_PATH,
        "content": prefix,
        "offset": 0,
        "bytes_read": prefix.len(),
        "next_offset": prefix.len(),
        "total_bytes": WORKSPACE_SEED.len(),
        "truncated": true,
        EVAL_RECEIPT_FIELD: SYNTHETIC_EVAL_RECEIPT,
    })
    .to_string();

    assert!(!suite.forced_case_result_passed(case, &result)?);
    Ok(())
}

#[test]
fn forced_workspace_listing_verifiers_reject_the_wrong_entry_kind() -> EvalResult {
    let suite = FamilySuite::workspace()?;
    let list = WORKSPACE_CASES
        .iter()
        .find(|case| case.name == LIST_DIRECTORY_NAME)
        .expect("the workspace list fixture exists");
    let glob = WORKSPACE_CASES
        .iter()
        .find(|case| case.name == GLOB_FILES_NAME)
        .expect("the workspace glob fixture exists");
    let mut list_entries = workspace_listing_json(expected_workspace_listing());
    list_entries[0]["kind"] = serde_json::json!("directory");
    let list_result = serde_json::json!({
        "entries": list_entries,
        "truncated": true,
        EVAL_RECEIPT_FIELD: SYNTHETIC_EVAL_RECEIPT,
    })
    .to_string();
    let glob_result = serde_json::json!({
        "matches": [{"path": WORKSPACE_GLOB_PATH, "kind": "directory"}],
        "truncated": true,
        EVAL_RECEIPT_FIELD: SYNTHETIC_EVAL_RECEIPT,
    })
    .to_string();

    assert!(!suite.forced_case_result_passed(list, &list_result)?);
    assert!(!suite.forced_case_result_passed(glob, &glob_result)?);
    Ok(())
}

#[test]
fn forced_workspace_list_verifier_rejects_an_unknown_entry_field() -> EvalResult {
    let suite = FamilySuite::workspace()?;
    let case = WORKSPACE_CASES
        .iter()
        .find(|case| case.name == LIST_DIRECTORY_NAME)
        .expect("the workspace list fixture exists");
    let mut entries = workspace_listing_json(expected_workspace_listing());
    entries[0]["error"] = serde_json::json!("synthetic contradictory field");
    let result = serde_json::json!({
        "entries": entries,
        "truncated": true,
        EVAL_RECEIPT_FIELD: SYNTHETIC_EVAL_RECEIPT,
    })
    .to_string();

    assert!(!suite.forced_case_result_passed(case, &result)?);
    Ok(())
}

#[test]
fn forced_workspace_list_verifier_rejects_an_unbounded_result() -> EvalResult {
    let suite = FamilySuite::workspace()?;
    let case = WORKSPACE_CASES
        .iter()
        .find(|case| case.name == LIST_DIRECTORY_NAME)
        .expect("the workspace list fixture exists");
    let result = serde_json::json!({
        "entries": workspace_listing_json(complete_workspace_listing()),
        "truncated": false,
        EVAL_RECEIPT_FIELD: SYNTHETIC_EVAL_RECEIPT,
    })
    .to_string();

    assert!(!suite.forced_case_result_passed(case, &result)?);
    Ok(())
}

#[test]
fn forced_workspace_list_verifier_rejects_collateral_mutation() -> EvalResult {
    let suite = FamilySuite::workspace()?;
    let case = WORKSPACE_CASES
        .iter()
        .find(|case| case.name == LIST_DIRECTORY_NAME)
        .expect("the workspace list fixture exists");
    fs::write(
        suite.workspace.path().join(WORKSPACE_SEED_PATH),
        WORKSPACE_DRIFTED_SEED,
    )?;
    let result = serde_json::json!({
        "entries": workspace_listing_json(expected_workspace_listing()),
        "truncated": true,
        EVAL_RECEIPT_FIELD: SYNTHETIC_EVAL_RECEIPT,
    })
    .to_string();

    assert!(!suite.forced_case_result_passed(case, &result)?);
    Ok(())
}

#[cfg(unix)]
#[test]
fn forced_workspace_list_verifier_rejects_collateral_hard_links() -> EvalResult {
    let suite = FamilySuite::workspace()?;
    let case = WORKSPACE_CASES
        .iter()
        .find(|case| case.name == LIST_DIRECTORY_NAME)
        .expect("the workspace list fixture exists");
    let source = suite.workspace.path().join(workspace_list_entry_path(0));
    let alias = suite.workspace.path().join(workspace_list_entry_path(1));
    fs::remove_file(&alias)?;
    fs::hard_link(&source, &alias)?;
    let result = serde_json::json!({
        "entries": workspace_listing_json(expected_workspace_listing()),
        "truncated": true,
        EVAL_RECEIPT_FIELD: SYNTHETIC_EVAL_RECEIPT,
    })
    .to_string();

    assert!(!suite.forced_case_result_passed(case, &result)?);
    Ok(())
}

#[test]
fn forced_workspace_glob_verifier_rejects_a_nonmatching_path() -> EvalResult {
    let suite = FamilySuite::workspace()?;
    let case = WORKSPACE_CASES
        .iter()
        .find(|case| case.name == GLOB_FILES_NAME)
        .expect("the workspace glob fixture exists");
    let result = serde_json::json!({
        "matches": [
            {"path": WORKSPACE_SEED_PATH, "kind": "file"},
            {"path": workspace_nonmatching_path(0), "kind": "file"}
        ],
        "truncated": true,
        EVAL_RECEIPT_FIELD: SYNTHETIC_EVAL_RECEIPT,
    })
    .to_string();

    assert!(!suite.forced_case_result_passed(case, &result)?);
    Ok(())
}

#[test]
fn forced_workspace_glob_verifier_rejects_a_scoped_pattern_miss() -> EvalResult {
    let suite = FamilySuite::workspace()?;
    let case = WORKSPACE_CASES
        .iter()
        .find(|case| case.name == GLOB_FILES_NAME)
        .expect("the workspace glob fixture exists");
    let result = serde_json::json!({
        "matches": [{"path": WORKSPACE_GLOB_NONMATCHING_PATH, "kind": "file"}],
        "truncated": true,
        EVAL_RECEIPT_FIELD: SYNTHETIC_EVAL_RECEIPT,
    })
    .to_string();

    assert_eq!(
        fs::read(suite.workspace.path().join(WORKSPACE_GLOB_NONMATCHING_PATH))?,
        WORKSPACE_GLOB_NONMATCHING_CONTENT.as_bytes()
    );
    assert!(!suite.forced_case_result_passed(case, &result)?);
    Ok(())
}

#[test]
fn forced_workspace_glob_verifier_accepts_the_bounded_first_match() -> EvalResult {
    let suite = FamilySuite::workspace()?;
    let case = WORKSPACE_CASES
        .iter()
        .find(|case| case.name == GLOB_FILES_NAME)
        .expect("the workspace glob fixture exists");
    let result = serde_json::json!({
        "matches": [{"path": WORKSPACE_GLOB_PATH, "kind": "file"}],
        "truncated": true,
        EVAL_RECEIPT_FIELD: SYNTHETIC_EVAL_RECEIPT,
    })
    .to_string();

    assert!(suite.forced_case_result_passed(case, &result)?);
    Ok(())
}

#[test]
fn forced_workspace_glob_verifier_rejects_collateral_mutation() -> EvalResult {
    let suite = FamilySuite::workspace()?;
    let case = WORKSPACE_CASES
        .iter()
        .find(|case| case.name == GLOB_FILES_NAME)
        .expect("the workspace glob fixture exists");
    fs::write(
        suite.workspace.path().join(WORKSPACE_SEED_PATH),
        WORKSPACE_DRIFTED_SEED,
    )?;
    let result = serde_json::json!({
        "matches": [{"path": WORKSPACE_GLOB_PATH, "kind": "file"}],
        "truncated": true,
        EVAL_RECEIPT_FIELD: SYNTHETIC_EVAL_RECEIPT,
    })
    .to_string();

    assert!(!suite.forced_case_result_passed(case, &result)?);
    Ok(())
}

#[test]
fn forced_workspace_glob_verifier_rejects_a_root_match() -> EvalResult {
    let suite = FamilySuite::workspace()?;
    let case = WORKSPACE_CASES
        .iter()
        .find(|case| case.name == GLOB_FILES_NAME)
        .expect("the workspace glob fixture exists");
    let result = serde_json::json!({
        "matches": [{"path": WORKSPACE_SEED_PATH, "kind": "file"}],
        "truncated": true,
        EVAL_RECEIPT_FIELD: SYNTHETIC_EVAL_RECEIPT,
    })
    .to_string();

    assert!(!suite.forced_case_result_passed(case, &result)?);
    Ok(())
}

#[test]
fn forced_workspace_edit_fixture_exercises_replace_all() -> EvalResult {
    let suite = FamilySuite::workspace()?;
    let case = WORKSPACE_CASES
        .iter()
        .find(|case| case.name == EDIT_FILE_NAME)
        .expect("the workspace edit fixture exists");
    let arguments: serde_json::Value = serde_json::from_str(case.expected_arguments)?;
    let started = current_filesystem_recorded_time()?;
    fs::write(
        suite.workspace.path().join(WORKSPACE_SEED_PATH),
        WORKSPACE_EDITED_SEED,
    )?;
    fs::File::open(suite.workspace.path())?
        .set_times(fs::FileTimes::new().set_modified(current_filesystem_recorded_time()?))?;
    suite.executor.record_filesystem_execution_window(
        EDIT_FILE_NAME,
        FilesystemExecutionTimeWindow {
            started,
            finished: current_filesystem_recorded_time()?,
        },
    );
    let result = serde_json::json!({
        "path": WORKSPACE_SEED_PATH,
        "replacements": EXPECTED_WORKSPACE_EDIT_REPLACEMENTS,
        "bytes_written": EXPECTED_WORKSPACE_EDIT_BYTES,
        EVAL_RECEIPT_FIELD: SYNTHETIC_EVAL_RECEIPT,
    })
    .to_string();

    assert_eq!(arguments["replace_all"], true);
    assert!(suite.forced_case_result_passed(case, &result)?);
    Ok(())
}

#[test]
fn forced_workspace_edit_verifier_accepts_atomic_parent_mtime_change() -> EvalResult {
    let suite = FamilySuite::workspace()?;
    let case = WORKSPACE_CASES
        .iter()
        .find(|case| case.name == EDIT_FILE_NAME)
        .expect("the workspace edit fixture exists");
    let started = current_filesystem_recorded_time()?;
    fs::write(
        suite.workspace.path().join(WORKSPACE_SEED_PATH),
        WORKSPACE_EDITED_SEED,
    )?;
    fs::File::open(suite.workspace.path())?
        .set_times(fs::FileTimes::new().set_modified(current_filesystem_recorded_time()?))?;
    suite.executor.record_filesystem_execution_window(
        EDIT_FILE_NAME,
        FilesystemExecutionTimeWindow {
            started,
            finished: current_filesystem_recorded_time()?,
        },
    );
    let result = serde_json::json!({
        "path": WORKSPACE_SEED_PATH,
        "replacements": EXPECTED_WORKSPACE_EDIT_REPLACEMENTS,
        "bytes_written": EXPECTED_WORKSPACE_EDIT_BYTES,
        EVAL_RECEIPT_FIELD: SYNTHETIC_EVAL_RECEIPT,
    })
    .to_string();

    assert!(suite.forced_case_result_passed(case, &result)?);
    Ok(())
}

#[test]
fn workspace_natural_state_requires_the_read_before_the_write() {
    let snapshot = CaseSnapshot {
        turn_disposition: SnapshotTurnDisposition::Completed,
        requests: vec![
            RequestSnapshot {
                request_id: Uuid::from_u128(ARBITRARY_EVAL_REQUEST_ID),
                producing_model_call_id: Uuid::from_u128(ARBITRARY_EVAL_MODEL_CALL_ID),
                name: String::from(WRITE_FILE_NAME),
                arguments_text: serde_json::json!({
                    "content": WORKSPACE_ANSWER,
                    "path": WORKSPACE_ANSWER_PATH,
                })
                .to_string(),
                entry_index: ARBITRARY_REQUEST_ENTRY_INDEX,
                completed_result_entry_index: Some(ARBITRARY_COMPLETED_RESULT_ENTRY_INDEX),
                attempt_succeeded: true,
                attempt_denied: false,
            },
            RequestSnapshot {
                request_id: Uuid::from_u128(ARBITRARY_EVAL_REQUEST_ID),
                producing_model_call_id: Uuid::from_u128(ARBITRARY_SECOND_EVAL_MODEL_CALL_ID),
                name: String::from(READ_FILE_NAME),
                arguments_text: serde_json::json!({"path": WORKSPACE_SEED_PATH}).to_string(),
                entry_index: ARBITRARY_REQUEST_ENTRY_INDEX,
                completed_result_entry_index: Some(ARBITRARY_COMPLETED_RESULT_ENTRY_INDEX),
                attempt_succeeded: true,
                attempt_denied: false,
            },
        ],
        model_calls: MINIMUM_MODEL_CALLS_FOR_RESULT_ROUND_TRIP,
    };

    assert!(!snapshot.workspace_natural_requests_passed());
}

#[test]
fn workspace_natural_state_requires_the_read_to_cover_the_full_brief() {
    let snapshot = CaseSnapshot {
        turn_disposition: SnapshotTurnDisposition::Completed,
        requests: vec![
            RequestSnapshot {
                request_id: Uuid::from_u128(ARBITRARY_EVAL_REQUEST_ID),
                producing_model_call_id: Uuid::from_u128(ARBITRARY_EVAL_MODEL_CALL_ID),
                name: String::from(READ_FILE_NAME),
                arguments_text: serde_json::json!({
                    "max_bytes": 1,
                    "path": WORKSPACE_SEED_PATH,
                })
                .to_string(),
                entry_index: ARBITRARY_REQUEST_ENTRY_INDEX,
                completed_result_entry_index: Some(ARBITRARY_COMPLETED_RESULT_ENTRY_INDEX),
                attempt_succeeded: true,
                attempt_denied: false,
            },
            RequestSnapshot {
                request_id: Uuid::from_u128(ARBITRARY_SECOND_EVAL_REQUEST_ID),
                producing_model_call_id: Uuid::from_u128(ARBITRARY_SECOND_EVAL_MODEL_CALL_ID),
                name: String::from(WRITE_FILE_NAME),
                arguments_text: serde_json::json!({
                    "content": WORKSPACE_ANSWER,
                    "path": WORKSPACE_ANSWER_PATH,
                })
                .to_string(),
                entry_index: ARBITRARY_REQUEST_ENTRY_INDEX,
                completed_result_entry_index: Some(ARBITRARY_COMPLETED_RESULT_ENTRY_INDEX),
                attempt_succeeded: true,
                attempt_denied: false,
            },
        ],
        model_calls: MINIMUM_MODEL_CALLS_FOR_RESULT_ROUND_TRIP,
    };

    assert!(!snapshot.workspace_natural_requests_passed());
}

#[test]
fn workspace_natural_state_requires_a_later_model_call_for_the_write() {
    let snapshot = CaseSnapshot {
        turn_disposition: SnapshotTurnDisposition::Completed,
        requests: vec![
            RequestSnapshot {
                request_id: Uuid::from_u128(ARBITRARY_EVAL_REQUEST_ID),
                producing_model_call_id: Uuid::from_u128(ARBITRARY_EVAL_MODEL_CALL_ID),
                name: String::from(READ_FILE_NAME),
                arguments_text: serde_json::json!({"path": WORKSPACE_SEED_PATH}).to_string(),
                entry_index: ARBITRARY_REQUEST_ENTRY_INDEX,
                completed_result_entry_index: Some(ARBITRARY_COMPLETED_RESULT_ENTRY_INDEX),
                attempt_succeeded: true,
                attempt_denied: false,
            },
            RequestSnapshot {
                request_id: Uuid::from_u128(ARBITRARY_EVAL_REQUEST_ID),
                producing_model_call_id: Uuid::from_u128(ARBITRARY_EVAL_MODEL_CALL_ID),
                name: String::from(WRITE_FILE_NAME),
                arguments_text: serde_json::json!({
                    "content": WORKSPACE_ANSWER,
                    "path": WORKSPACE_ANSWER_PATH,
                })
                .to_string(),
                entry_index: ARBITRARY_REQUEST_ENTRY_INDEX,
                completed_result_entry_index: Some(ARBITRARY_COMPLETED_RESULT_ENTRY_INDEX),
                attempt_succeeded: true,
                attempt_denied: false,
            },
        ],
        model_calls: MINIMUM_MODEL_CALLS_FOR_RESULT_ROUND_TRIP,
    };

    assert!(!snapshot.workspace_natural_requests_passed());
}

#[test]
fn workspace_natural_state_requires_the_read_result_before_the_write_call() {
    let mut snapshot = successful_workspace_natural_snapshot();
    snapshot.requests[0].completed_result_entry_index = Some(ARBITRARY_LATE_RESULT_ENTRY_INDEX);

    assert!(!snapshot.workspace_natural_requests_passed());
}

#[test]
fn workspace_natural_state_rejects_an_unrelated_mutation() {
    let snapshot = CaseSnapshot {
        turn_disposition: SnapshotTurnDisposition::Completed,
        requests: vec![
            RequestSnapshot {
                request_id: Uuid::from_u128(ARBITRARY_EVAL_REQUEST_ID),
                producing_model_call_id: Uuid::from_u128(ARBITRARY_EVAL_MODEL_CALL_ID),
                name: String::from(READ_FILE_NAME),
                arguments_text: serde_json::json!({"path": WORKSPACE_SEED_PATH}).to_string(),
                entry_index: ARBITRARY_REQUEST_ENTRY_INDEX,
                completed_result_entry_index: Some(ARBITRARY_COMPLETED_RESULT_ENTRY_INDEX),
                attempt_succeeded: true,
                attempt_denied: false,
            },
            RequestSnapshot {
                request_id: Uuid::from_u128(ARBITRARY_SECOND_EVAL_REQUEST_ID),
                producing_model_call_id: Uuid::from_u128(ARBITRARY_SECOND_EVAL_MODEL_CALL_ID),
                name: String::from(WRITE_FILE_NAME),
                arguments_text: serde_json::json!({
                    "content": WORKSPACE_ANSWER,
                    "path": WORKSPACE_ANSWER_PATH,
                })
                .to_string(),
                entry_index: ARBITRARY_REQUEST_ENTRY_INDEX,
                completed_result_entry_index: Some(ARBITRARY_COMPLETED_RESULT_ENTRY_INDEX),
                attempt_succeeded: true,
                attempt_denied: false,
            },
            RequestSnapshot {
                request_id: Uuid::from_u128(ARBITRARY_THIRD_EVAL_REQUEST_ID),
                producing_model_call_id: Uuid::from_u128(ARBITRARY_SECOND_EVAL_MODEL_CALL_ID),
                name: String::from(EDIT_FILE_NAME),
                arguments_text: serde_json::json!({
                    "new_string": "changed",
                    "old_string": WORKSPACE_SEED.trim_end(),
                    "path": WORKSPACE_SEED_PATH,
                })
                .to_string(),
                entry_index: ARBITRARY_REQUEST_ENTRY_INDEX,
                completed_result_entry_index: Some(ARBITRARY_COMPLETED_RESULT_ENTRY_INDEX),
                attempt_succeeded: true,
                attempt_denied: false,
            },
        ],
        model_calls: MINIMUM_MODEL_CALLS_FOR_RESULT_ROUND_TRIP,
    };

    assert!(!snapshot.workspace_natural_requests_passed());
}

#[test]
fn workspace_natural_state_rejects_collateral_fixture_mutation() -> EvalResult {
    let suite = FamilySuite::workspace()?;
    fs::write(
        suite.workspace.path().join(WORKSPACE_ANSWER_PATH),
        WORKSPACE_ANSWER,
    )?;
    fs::write(
        suite.workspace.path().join(WORKSPACE_SEED_PATH),
        WORKSPACE_DRIFTED_SEED,
    )?;
    let snapshot = successful_workspace_natural_snapshot();

    assert!(!suite.natural_state_passed(&snapshot)?);
    Ok(())
}

#[test]
fn workspace_natural_state_rejects_a_collateral_directory() -> EvalResult {
    let suite = FamilySuite::workspace()?;
    fs::write(
        suite.workspace.path().join(WORKSPACE_ANSWER_PATH),
        WORKSPACE_ANSWER,
    )?;
    fs::create_dir(suite.workspace.path().join(WORKSPACE_COLLATERAL_DIRECTORY))?;
    let snapshot = successful_workspace_natural_snapshot();

    assert!(!suite.natural_state_passed(&snapshot)?);
    Ok(())
}

#[cfg(unix)]
#[test]
fn workspace_natural_state_accepts_the_private_answer_mode() -> EvalResult {
    let suite = FamilySuite::workspace()?;
    let answer = suite.workspace.path().join(WORKSPACE_ANSWER_PATH);
    let started = current_filesystem_recorded_time()?;
    fs::write(&answer, WORKSPACE_ANSWER)?;
    fs::set_permissions(
        &answer,
        fs::Permissions::from_mode(WORKSPACE_PRIVATE_CREATION_MODE),
    )?;
    suite.executor.record_filesystem_execution_window(
        WRITE_FILE_NAME,
        FilesystemExecutionTimeWindow {
            started,
            finished: current_filesystem_recorded_time()?,
        },
    );
    let snapshot = successful_workspace_natural_snapshot();

    assert!(suite.natural_state_passed(&snapshot)?);
    Ok(())
}

#[cfg(target_os = "linux")]
#[test]
fn workspace_existing_mutation_target_rejects_inode_flag_drift() -> EvalResult {
    let suite = FamilySuite::workspace()?;
    let target = Path::new(WORKSPACE_SEED_PATH);
    let file = fs::File::open(suite.workspace.path().join(target))?;
    let flags = rustix::fs::ioctl_getflags(&file)?;
    rustix::fs::ioctl_setflags(&file, flags | rustix::fs::IFlags::NOATIME)?;

    assert!(!workspace_inode_flags_match_for_mutation(
        suite.workspace.path(),
        &suite.workspace_seed_inode_flags,
        target,
    )?);
    Ok(())
}

#[cfg(target_os = "linux")]
#[test]
fn workspace_created_mutation_target_rejects_inode_flag_drift() -> EvalResult {
    let suite = FamilySuite::workspace()?;
    let target = Path::new("written.txt");
    fs::write(
        suite.workspace.path().join(target),
        "synthetic created file\n",
    )?;
    let file = fs::File::open(suite.workspace.path().join(target))?;
    let flags = rustix::fs::ioctl_getflags(&file)?;
    rustix::fs::ioctl_setflags(&file, flags | rustix::fs::IFlags::NOATIME)?;

    assert!(!workspace_inode_flags_match_for_mutation(
        suite.workspace.path(),
        &suite.workspace_seed_inode_flags,
        target,
    )?);
    Ok(())
}

#[cfg(unix)]
#[test]
fn workspace_natural_state_rejects_collateral_hard_links() -> EvalResult {
    let suite = FamilySuite::workspace()?;
    let answer = suite.workspace.path().join(WORKSPACE_ANSWER_PATH);
    fs::write(&answer, WORKSPACE_ANSWER)?;
    fs::set_permissions(
        &answer,
        fs::Permissions::from_mode(WORKSPACE_PRIVATE_CREATION_MODE),
    )?;
    let source = suite.workspace.path().join(workspace_list_entry_path(0));
    let alias = suite.workspace.path().join(workspace_list_entry_path(1));
    fs::remove_file(&alias)?;
    fs::hard_link(&source, &alias)?;
    let snapshot = successful_workspace_natural_snapshot();

    assert!(!suite.natural_state_passed(&snapshot)?);
    Ok(())
}

#[cfg(unix)]
#[test]
fn workspace_natural_state_rejects_root_mode_drift() -> EvalResult {
    let suite = FamilySuite::workspace()?;
    let answer = suite.workspace.path().join(WORKSPACE_ANSWER_PATH);
    fs::write(&answer, WORKSPACE_ANSWER)?;
    fs::set_permissions(
        &answer,
        fs::Permissions::from_mode(WORKSPACE_PRIVATE_CREATION_MODE),
    )?;
    let mut permissions = fs::metadata(suite.workspace.path())?.permissions();
    permissions.set_mode(permissions.mode() ^ GROUP_WRITE_MODE_BIT);
    fs::set_permissions(suite.workspace.path(), permissions)?;
    let snapshot = successful_workspace_natural_snapshot();

    assert!(!suite.natural_state_passed(&snapshot)?);
    Ok(())
}

#[cfg(unix)]
#[test]
fn workspace_natural_state_rejects_an_insecure_answer_mode() -> EvalResult {
    let suite = FamilySuite::workspace()?;
    let answer = suite.workspace.path().join(WORKSPACE_ANSWER_PATH);
    fs::write(&answer, WORKSPACE_ANSWER)?;
    fs::set_permissions(
        &answer,
        fs::Permissions::from_mode(WORKSPACE_INSECURE_CREATION_MODE),
    )?;
    let snapshot = successful_workspace_natural_snapshot();

    assert!(!suite.natural_state_passed(&snapshot)?);
    Ok(())
}

#[test]
fn workspace_natural_state_propagates_inspection_failures() -> EvalResult {
    let suite = FamilySuite::workspace()?;
    fs::create_dir(suite.workspace.path().join(WORKSPACE_ANSWER_PATH))?;
    let snapshot = CaseSnapshot {
        turn_disposition: SnapshotTurnDisposition::Completed,
        requests: Vec::new(),
        model_calls: MINIMUM_MODEL_CALLS_FOR_RESULT_ROUND_TRIP,
    };

    assert!(suite.natural_state_passed(&snapshot).is_err());
    Ok(())
}
