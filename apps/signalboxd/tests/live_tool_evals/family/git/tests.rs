//! Git evaluation coverage.

use super::*;

#[test]
fn filesystem_execution_window_rejects_a_precise_earlier_git_mtime_in_the_same_second() {
    let window = FilesystemExecutionTimeWindow {
        started: UNIX_EPOCH + Duration::new(1, 500),
        finished: UNIX_EPOCH + Duration::new(1, 900),
    };
    let earlier = UNIX_EPOCH + Duration::new(1, 400);
    let identity = synthetic_filesystem_identity(700);

    assert!(!window.contains_git_modified(earlier, identity));
}

#[test]
fn filesystem_execution_window_accepts_a_coarse_git_mtime_with_precise_ctime() {
    let window = FilesystemExecutionTimeWindow {
        started: UNIX_EPOCH + Duration::new(1, 500),
        finished: UNIX_EPOCH + Duration::new(1, 900),
    };
    let coarse = UNIX_EPOCH + Duration::from_secs(1);
    let identity = synthetic_filesystem_identity(700);

    assert!(window.contains_git_modified(coarse, identity));
}

#[test]
fn filesystem_execution_window_rejects_a_coarse_git_mtime_without_in_window_ctime() {
    let window = FilesystemExecutionTimeWindow {
        started: UNIX_EPOCH + Duration::new(1, 500),
        finished: UNIX_EPOCH + Duration::new(1, 900),
    };
    let coarse = UNIX_EPOCH + Duration::from_secs(1);
    let identity = synthetic_filesystem_identity(400);

    assert!(!window.contains_git_modified(coarse, identity));
}

#[test]
fn git_execution_window_accepts_a_recorded_time_within_its_bounds() {
    let window = GitExecutionTimeWindow {
        started: GitRecordedTime {
            seconds: SYNTHETIC_GIT_EXECUTION_STARTED_SECONDS,
            offset_minutes: SYNTHETIC_GIT_EXECUTION_TIMEZONE_OFFSET,
        },
        finished: GitRecordedTime {
            seconds: SYNTHETIC_GIT_EXECUTION_FINISHED_SECONDS,
            offset_minutes: SYNTHETIC_GIT_EXECUTION_TIMEZONE_OFFSET,
        },
    };

    assert!(window.contains(Time::new(
        SYNTHETIC_GIT_EXECUTION_RECORDED_SECONDS,
        SYNTHETIC_GIT_EXECUTION_TIMEZONE_OFFSET,
    )));
}

#[test]
fn git_execution_window_rejects_a_timestamp_before_its_bounds() {
    let window = GitExecutionTimeWindow {
        started: GitRecordedTime {
            seconds: SYNTHETIC_GIT_EXECUTION_STARTED_SECONDS,
            offset_minutes: SYNTHETIC_GIT_EXECUTION_TIMEZONE_OFFSET,
        },
        finished: GitRecordedTime {
            seconds: SYNTHETIC_GIT_EXECUTION_FINISHED_SECONDS,
            offset_minutes: SYNTHETIC_GIT_EXECUTION_TIMEZONE_OFFSET,
        },
    };

    assert!(!window.contains(Time::new(
        SYNTHETIC_GIT_EXECUTION_STARTED_SECONDS - 1,
        SYNTHETIC_GIT_EXECUTION_TIMEZONE_OFFSET,
    )));
}

#[test]
fn git_execution_window_rejects_a_timezone_outside_its_bounds() {
    let window = GitExecutionTimeWindow {
        started: GitRecordedTime {
            seconds: SYNTHETIC_GIT_EXECUTION_STARTED_SECONDS,
            offset_minutes: SYNTHETIC_GIT_EXECUTION_TIMEZONE_OFFSET,
        },
        finished: GitRecordedTime {
            seconds: SYNTHETIC_GIT_EXECUTION_FINISHED_SECONDS,
            offset_minutes: SYNTHETIC_GIT_EXECUTION_TIMEZONE_OFFSET,
        },
    };

    assert!(!window.contains(Time::new(
        SYNTHETIC_GIT_EXECUTION_RECORDED_SECONDS,
        SYNTHETIC_OTHER_GIT_TIMEZONE_OFFSET,
    )));
}

#[test]
fn git_commit_times_accept_equal_values_within_the_execution_window() {
    let recorded = Time::new(
        SYNTHETIC_GIT_EXECUTION_RECORDED_SECONDS,
        SYNTHETIC_GIT_EXECUTION_TIMEZONE_OFFSET,
    );
    let window = GitExecutionTimeWindow {
        started: GitRecordedTime {
            seconds: SYNTHETIC_GIT_EXECUTION_STARTED_SECONDS,
            offset_minutes: SYNTHETIC_GIT_EXECUTION_TIMEZONE_OFFSET,
        },
        finished: GitRecordedTime {
            seconds: SYNTHETIC_GIT_EXECUTION_FINISHED_SECONDS,
            offset_minutes: SYNTHETIC_GIT_EXECUTION_TIMEZONE_OFFSET,
        },
    };

    assert!(git_commit_times_match_execution(
        recorded,
        recorded,
        Some(window),
    ));
}

#[test]
fn git_commit_times_reject_equal_values_outside_the_execution_window() {
    let recorded = Time::new(
        SYNTHETIC_GIT_EXECUTION_STARTED_SECONDS - 1,
        SYNTHETIC_GIT_EXECUTION_TIMEZONE_OFFSET,
    );
    let window = GitExecutionTimeWindow {
        started: GitRecordedTime {
            seconds: SYNTHETIC_GIT_EXECUTION_STARTED_SECONDS,
            offset_minutes: SYNTHETIC_GIT_EXECUTION_TIMEZONE_OFFSET,
        },
        finished: GitRecordedTime {
            seconds: SYNTHETIC_GIT_EXECUTION_FINISHED_SECONDS,
            offset_minutes: SYNTHETIC_GIT_EXECUTION_TIMEZONE_OFFSET,
        },
    };

    assert!(!git_commit_times_match_execution(
        recorded,
        recorded,
        Some(window),
    ));
}

#[test]
fn git_commit_times_reject_distinct_author_and_committer_values() {
    let author = Time::new(
        SYNTHETIC_GIT_EXECUTION_RECORDED_SECONDS,
        SYNTHETIC_GIT_EXECUTION_TIMEZONE_OFFSET,
    );
    let committer = Time::new(
        SYNTHETIC_GIT_EXECUTION_RECORDED_SECONDS,
        SYNTHETIC_OTHER_GIT_TIMEZONE_OFFSET,
    );
    let window = GitExecutionTimeWindow {
        started: GitRecordedTime {
            seconds: SYNTHETIC_GIT_EXECUTION_STARTED_SECONDS,
            offset_minutes: SYNTHETIC_GIT_EXECUTION_TIMEZONE_OFFSET,
        },
        finished: GitRecordedTime {
            seconds: SYNTHETIC_GIT_EXECUTION_FINISHED_SECONDS,
            offset_minutes: SYNTHETIC_OTHER_GIT_TIMEZONE_OFFSET,
        },
    };

    assert!(!git_commit_times_match_execution(
        author,
        committer,
        Some(window),
    ));
}

#[test]
fn unforced_git_tier_reports_infrastructure_for_an_exact_known_failed_attempt() {
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
                name: String::from(GIT_STAGE_NAME),
                arguments_text: serde_json::json!({"paths": [GIT_NATURAL_PATH]}).to_string(),
                entry_index: ARBITRARY_REQUEST_ENTRY_INDEX,
                completed_result_entry_index: None,
                attempt_succeeded: false,
                attempt_denied: false,
            }],
            model_calls: MINIMUM_MODEL_CALLS_FOR_RESULT_ROUND_TRIP,
        },
    };

    assert_eq!(
        outcome.natural_loop_disposition(EvalFamily::Git),
        EvalDisposition::Infrastructure
    );
    assert!(
        reject_natural_executor_failure(&outcome, EvalFamily::Git, EvalDisposition::Pass).is_err()
    );
}

#[test]
fn unforced_git_tier_reports_a_premature_commit_failure_as_a_miss() {
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
                name: String::from(GIT_CREATE_COMMIT_NAME),
                arguments_text: serde_json::json!({"message": GIT_NATURAL_MESSAGE}).to_string(),
                entry_index: ARBITRARY_REQUEST_ENTRY_INDEX,
                completed_result_entry_index: None,
                attempt_succeeded: false,
                attempt_denied: false,
            }],
            model_calls: MINIMUM_MODEL_CALLS_FOR_RESULT_ROUND_TRIP,
        },
    };

    assert_eq!(
        outcome.natural_loop_disposition(EvalFamily::Git),
        EvalDisposition::Miss
    );
}

#[test]
fn unforced_git_tier_reports_a_post_stage_commit_failure_as_infrastructure() {
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
            requests: vec![
                RequestSnapshot {
                    request_id: Uuid::from_u128(ARBITRARY_EVAL_REQUEST_ID),
                    producing_model_call_id: Uuid::from_u128(ARBITRARY_EVAL_MODEL_CALL_ID),
                    name: String::from(GIT_STAGE_NAME),
                    arguments_text: serde_json::json!({"paths": [GIT_NATURAL_PATH]}).to_string(),
                    entry_index: ARBITRARY_REQUEST_ENTRY_INDEX,
                    completed_result_entry_index: Some(ARBITRARY_COMPLETED_RESULT_ENTRY_INDEX),
                    attempt_succeeded: true,
                    attempt_denied: false,
                },
                RequestSnapshot {
                    request_id: Uuid::from_u128(ARBITRARY_SECOND_EVAL_REQUEST_ID),
                    producing_model_call_id: Uuid::from_u128(ARBITRARY_SECOND_EVAL_MODEL_CALL_ID),
                    name: String::from(GIT_CREATE_COMMIT_NAME),
                    arguments_text: serde_json::json!({"message": GIT_NATURAL_MESSAGE}).to_string(),
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
        outcome.natural_loop_disposition(EvalFamily::Git),
        EvalDisposition::Infrastructure
    );
}

#[test]
fn unforced_git_tier_keeps_a_duplicate_commit_failure_as_a_miss() {
    let snapshot = CaseSnapshot {
        turn_disposition: SnapshotTurnDisposition::Completed,
        requests: vec![
            RequestSnapshot {
                request_id: Uuid::from_u128(ARBITRARY_EVAL_REQUEST_ID),
                producing_model_call_id: Uuid::from_u128(ARBITRARY_EVAL_MODEL_CALL_ID),
                name: String::from(GIT_STAGE_NAME),
                arguments_text: serde_json::json!({"paths": [GIT_NATURAL_PATH]}).to_string(),
                entry_index: ARBITRARY_REQUEST_ENTRY_INDEX,
                completed_result_entry_index: Some(ARBITRARY_COMPLETED_RESULT_ENTRY_INDEX),
                attempt_succeeded: true,
                attempt_denied: false,
            },
            RequestSnapshot {
                request_id: Uuid::from_u128(ARBITRARY_SECOND_EVAL_REQUEST_ID),
                producing_model_call_id: Uuid::from_u128(ARBITRARY_SECOND_EVAL_MODEL_CALL_ID),
                name: String::from(GIT_CREATE_COMMIT_NAME),
                arguments_text: serde_json::json!({"message": GIT_NATURAL_MESSAGE}).to_string(),
                entry_index: ARBITRARY_REQUEST_ENTRY_INDEX,
                completed_result_entry_index: Some(ARBITRARY_COMPLETED_RESULT_ENTRY_INDEX),
                attempt_succeeded: true,
                attempt_denied: false,
            },
            RequestSnapshot {
                request_id: Uuid::from_u128(ARBITRARY_THIRD_EVAL_REQUEST_ID),
                producing_model_call_id: Uuid::from_u128(ARBITRARY_SECOND_EVAL_MODEL_CALL_ID),
                name: String::from(GIT_CREATE_COMMIT_NAME),
                arguments_text: serde_json::json!({"message": GIT_NATURAL_MESSAGE}).to_string(),
                entry_index: ARBITRARY_REQUEST_ENTRY_INDEX,
                completed_result_entry_index: None,
                attempt_succeeded: false,
                attempt_denied: false,
            },
        ],
        model_calls: MINIMUM_MODEL_CALLS_FOR_RESULT_ROUND_TRIP,
    };

    assert!(!snapshot.exact_natural_request_failed(EvalFamily::Git));
}

#[test]
fn unforced_git_tier_keeps_a_schema_invalid_extra_field_as_a_miss() {
    let snapshot = failed_request_snapshot(
        GIT_STAGE_NAME,
        serde_json::json!({"paths": [GIT_NATURAL_PATH], "unexpected": true}),
    );

    assert!(!snapshot.exact_natural_request_failed(EvalFamily::Git));
}

#[test]
fn unforced_git_tier_requires_both_task_tools() {
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
            requests: vec![RequestSnapshot {
                request_id: Uuid::from_u128(ARBITRARY_EVAL_REQUEST_ID),
                producing_model_call_id: Uuid::from_u128(ARBITRARY_EVAL_MODEL_CALL_ID),
                name: String::from(GIT_STAGE_NAME),
                arguments_text: serde_json::json!({"paths": [GIT_NATURAL_PATH]}).to_string(),
                entry_index: ARBITRARY_REQUEST_ENTRY_INDEX,
                completed_result_entry_index: Some(ARBITRARY_COMPLETED_RESULT_ENTRY_INDEX),
                attempt_succeeded: true,
                attempt_denied: false,
            }],
            model_calls: MINIMUM_MODEL_CALLS_FOR_RESULT_ROUND_TRIP,
        },
    };

    assert_eq!(
        outcome.natural_loop_disposition(EvalFamily::Git),
        EvalDisposition::Miss
    );
}

#[test]
fn git_natural_object_entries_accept_the_stage_and_commit_windows() -> EvalResult {
    let fixture = git_natural_filesystem_fixture()?;
    let repository = Repository::open(fixture.suite.workspace.path())?;
    let head = repository.head()?.peel_to_commit()?;
    let entries = fixture
        .suite
        .git_pre_execution_object_entries
        .lock()
        .expect("Git pre-execution object-entry lock is available");
    let modified_times = fixture
        .suite
        .git_pre_execution_object_modified_times
        .lock()
        .expect("Git pre-execution object-time lock is available");
    let entry_identities = fixture
        .suite
        .git_pre_execution_object_entry_identities
        .lock()
        .expect("Git pre-execution object-identity lock is available");

    assert!(git_natural_object_entries_match(
        fixture.suite.workspace.path(),
        &head,
        &fixture.suite.git_seed_fixture,
        Some(fixture.stage_window),
        GitObjectEntryVerification {
            pre_execution_entries: entries.as_ref(),
            pre_execution_modified_times: modified_times.as_ref(),
            pre_execution_entry_identities: entry_identities.as_ref(),
            execution_window: Some(fixture.commit_window),
        },
    )?);
    Ok(())
}

#[test]
fn git_natural_object_entries_reject_a_disjoint_stage_window_for_the_staged_blob() -> EvalResult {
    let fixture = git_natural_filesystem_fixture()?;
    let repository = Repository::open(fixture.suite.workspace.path())?;
    let head = repository.head()?.peel_to_commit()?;
    let entries = fixture
        .suite
        .git_pre_execution_object_entries
        .lock()
        .expect("Git pre-execution object-entry lock is available");
    let modified_times = fixture
        .suite
        .git_pre_execution_object_modified_times
        .lock()
        .expect("Git pre-execution object-time lock is available");
    let entry_identities = fixture
        .suite
        .git_pre_execution_object_entry_identities
        .lock()
        .expect("Git pre-execution object-identity lock is available");

    assert!(!git_natural_object_entries_match(
        fixture.suite.workspace.path(),
        &head,
        &fixture.suite.git_seed_fixture,
        Some(FilesystemExecutionTimeWindow {
            started: UNIX_EPOCH,
            finished: UNIX_EPOCH,
        }),
        GitObjectEntryVerification {
            pre_execution_entries: entries.as_ref(),
            pre_execution_modified_times: modified_times.as_ref(),
            pre_execution_entry_identities: entry_identities.as_ref(),
            execution_window: Some(fixture.commit_window),
        },
    )?);
    Ok(())
}

#[test]
fn git_natural_metadata_root_accepts_an_operation_window() -> EvalResult {
    let fixture = git_natural_filesystem_fixture()?;

    assert!(git_natural_metadata_root_times_match(
        fixture.suite.workspace.path(),
        &fixture.suite.git_seed_fixture,
        Some(fixture.stage_window),
        Some(fixture.commit_window),
    )?);
    Ok(())
}

#[test]
fn git_natural_metadata_root_rejects_post_commit_timestamp_drift() -> EvalResult {
    let fixture = git_natural_filesystem_fixture()?;
    let repository = Repository::open(fixture.suite.workspace.path())?;
    fs::File::open(repository.path())?.set_times(fs::FileTimes::new().set_modified(UNIX_EPOCH))?;

    assert!(!git_natural_metadata_root_times_match(
        fixture.suite.workspace.path(),
        &fixture.suite.git_seed_fixture,
        Some(fixture.stage_window),
        Some(fixture.commit_window),
    )?);
    Ok(())
}

#[test]
fn git_natural_state_rejects_a_commit_with_an_unrelated_fixture() -> EvalResult {
    let workspace = tempfile::tempdir()?;
    let (seed, seed_refs, seed_fixture) = seed_git_repository_with_refs(workspace.path())?;
    stage_path(workspace.path(), GIT_NATURAL_PATH)?;
    stage_path(workspace.path(), GIT_STAGE_PATH)?;
    commit_staged_paths(workspace.path(), GIT_NATURAL_MESSAGE)?;

    assert!(!git_natural_state_passed(
        workspace.path(),
        seed,
        &seed_refs,
        &seed_fixture,
    )?);
    Ok(())
}

#[test]
fn git_natural_state_rejects_a_commit_with_drifted_bytes() -> EvalResult {
    let workspace = tempfile::tempdir()?;
    let (seed, seed_refs, seed_fixture) = seed_git_repository_with_refs(workspace.path())?;
    fs::write(
        workspace.path().join(GIT_NATURAL_PATH),
        GIT_DRIFTED_NATURAL_CONTENT,
    )?;
    stage_path(workspace.path(), GIT_NATURAL_PATH)?;
    commit_staged_paths(workspace.path(), GIT_NATURAL_MESSAGE)?;

    assert!(!git_natural_state_passed(
        workspace.path(),
        seed,
        &seed_refs,
        &seed_fixture,
    )?);
    Ok(())
}

#[cfg(unix)]
#[test]
fn git_natural_state_rejects_a_byte_identical_target_replacement() -> EvalResult {
    let workspace = tempfile::tempdir()?;
    let (_, _, seed_fixture) = seed_git_repository_with_refs(workspace.path())?;
    stage_path(workspace.path(), GIT_NATURAL_PATH)?;
    commit_staged_paths(workspace.path(), GIT_NATURAL_MESSAGE)?;

    assert!(git_natural_worktree_entry_identities_match(
        workspace.path(),
        &seed_fixture,
    )?);

    let target = workspace.path().join(GIT_NATURAL_PATH);
    replace_git_metadata_file_byte_identically(
        &target,
        seed_fixture.worktree_modified_times[Path::new(GIT_NATURAL_PATH)],
        seed_fixture.worktree_modified_times[Path::new("")],
    )?;

    assert_eq!(
        git_worktree_entries(workspace.path())?,
        seed_fixture.worktree_entries
    );
    assert_eq!(
        git_worktree_modified_times(workspace.path())?,
        seed_fixture.worktree_modified_times
    );
    assert_ne!(
        git_worktree_entry_identities(workspace.path())?,
        seed_fixture.worktree_entry_identities
    );
    assert!(!git_natural_worktree_entry_identities_match(
        workspace.path(),
        &seed_fixture,
    )?);
    Ok(())
}

#[test]
fn git_natural_state_rejects_skip_worktree_index_drift() -> EvalResult {
    let workspace = tempfile::tempdir()?;
    let (seed, seed_refs, seed_fixture) = seed_git_repository_with_refs(workspace.path())?;
    stage_path(workspace.path(), GIT_NATURAL_PATH)?;
    commit_staged_paths(workspace.path(), GIT_NATURAL_MESSAGE)?;
    let repository = Repository::open(workspace.path())?;
    let mut index = repository.index()?;
    let mut entry = index
        .get_path(Path::new(GIT_SEED_PATH), 0)
        .expect("the seeded index entry exists");
    entry.flags |= GIT_INDEX_EXTENDED_FLAG;
    entry.flags_extended |= GIT_INDEX_SKIP_WORKTREE_FLAG;
    index.add(&entry)?;
    index.write()?;

    assert!(!git_natural_state_passed(
        workspace.path(),
        seed,
        &seed_refs,
        &seed_fixture,
    )?);
    Ok(())
}

#[test]
fn git_natural_state_rejects_unrelated_stat_cache_drift() -> EvalResult {
    let workspace = tempfile::tempdir()?;
    let (seed, seed_refs, seed_fixture) = seed_git_repository_with_refs(workspace.path())?;
    stage_path(workspace.path(), GIT_NATURAL_PATH)?;
    commit_staged_paths(workspace.path(), GIT_NATURAL_MESSAGE)?;
    drift_git_index_ctime(workspace.path(), GIT_SEED_PATH)?;

    assert!(!git_natural_state_passed(
        workspace.path(),
        seed,
        &seed_refs,
        &seed_fixture,
    )?);
    Ok(())
}

#[test]
fn git_natural_state_rejects_index_extension_drift() -> EvalResult {
    let workspace = tempfile::tempdir()?;
    let (seed, seed_refs, seed_fixture) = seed_git_repository_with_refs(workspace.path())?;
    stage_path(workspace.path(), GIT_NATURAL_PATH)?;
    commit_staged_paths(workspace.path(), GIT_NATURAL_MESSAGE)?;
    append_synthetic_git_index_extension(&Repository::open(workspace.path())?)?;

    assert!(!git_natural_state_passed(
        workspace.path(),
        seed,
        &seed_refs,
        &seed_fixture,
    )?);
    Ok(())
}

#[cfg(unix)]
#[test]
fn git_natural_state_rejects_an_executable_committed_fixture() -> EvalResult {
    let workspace = tempfile::tempdir()?;
    let (seed, seed_refs, seed_fixture) = seed_git_repository_with_refs(workspace.path())?;
    let path = workspace.path().join(GIT_NATURAL_PATH);
    let mut permissions = fs::metadata(&path)?.permissions();
    permissions.set_mode(permissions.mode() | USER_EXECUTE_MODE_BIT);
    fs::set_permissions(&path, permissions)?;
    stage_path(workspace.path(), GIT_NATURAL_PATH)?;
    commit_staged_paths(workspace.path(), GIT_NATURAL_MESSAGE)?;

    assert!(!git_natural_state_passed(
        workspace.path(),
        seed,
        &seed_refs,
        &seed_fixture,
    )?);
    Ok(())
}

#[test]
fn git_natural_state_rejects_retained_operation_state() -> EvalResult {
    let workspace = tempfile::tempdir()?;
    let (seed, seed_refs, seed_fixture) = seed_git_repository_with_refs(workspace.path())?;
    stage_path(workspace.path(), GIT_NATURAL_PATH)?;
    commit_staged_paths(workspace.path(), GIT_NATURAL_MESSAGE)?;
    fs::write(
        Repository::open(workspace.path())?
            .path()
            .join(GIT_CHERRY_PICK_HEAD_PATH),
        format!("{seed}\n"),
    )?;

    assert!(!git_natural_state_passed(
        workspace.path(),
        seed,
        &seed_refs,
        &seed_fixture,
    )?);
    Ok(())
}

#[test]
fn git_natural_state_rejects_extra_staging_after_the_target_commit() -> EvalResult {
    let workspace = tempfile::tempdir()?;
    let (seed, seed_refs, seed_fixture) = seed_git_repository_with_refs(workspace.path())?;
    stage_path(workspace.path(), GIT_NATURAL_PATH)?;
    commit_staged_paths(workspace.path(), GIT_NATURAL_MESSAGE)?;
    stage_path(workspace.path(), GIT_STAGE_PATH)?;

    assert!(!git_natural_state_passed(
        workspace.path(),
        seed,
        &seed_refs,
        &seed_fixture,
    )?);
    Ok(())
}

#[test]
fn git_natural_state_rejects_a_missing_branch_reflog_record() -> EvalResult {
    let workspace = tempfile::tempdir()?;
    let (seed, seed_refs, seed_fixture) = seed_git_repository_with_refs(workspace.path())?;
    stage_path(workspace.path(), GIT_NATURAL_PATH)?;
    commit_staged_paths(workspace.path(), GIT_NATURAL_MESSAGE)?;
    let branch_log = Repository::open(workspace.path())?
        .path()
        .join(GIT_LOGS_DIRECTORY)
        .join("refs/heads")
        .join(GIT_BASE_BRANCH);
    let contents = fs::read(&branch_log)?;
    let previous_record_end = contents[..contents.len() - 1]
        .iter()
        .rposition(|byte| *byte == b'\n')
        .expect("the seeded branch reflog has a previous record");
    fs::write(&branch_log, &contents[..=previous_record_end])?;

    assert!(!git_natural_state_passed(
        workspace.path(),
        seed,
        &seed_refs,
        &seed_fixture,
    )?);
    Ok(())
}

#[test]
fn git_natural_state_rejects_branch_reflog_timezone_drift() -> EvalResult {
    let workspace = tempfile::tempdir()?;
    let (seed, seed_refs, seed_fixture) = seed_git_repository_with_refs(workspace.path())?;
    stage_path(workspace.path(), GIT_NATURAL_PATH)?;
    commit_staged_paths(workspace.path(), GIT_NATURAL_MESSAGE)?;
    let repository = Repository::open(workspace.path())?;
    let commit = repository.head()?.peel_to_commit()?;
    let commit_time = commit.committer().when();
    let altered_time = Time::new(
        commit_time.seconds(),
        commit_time.offset_minutes().saturating_add(1),
    );
    let altered_signature = Signature::new(GIT_AUTHOR_NAME, GIT_AUTHOR_EMAIL, &altered_time)?;
    replace_latest_reflog_signature(
        &repository,
        &format!("refs/heads/{GIT_BASE_BRANCH}"),
        &altered_signature,
    )?;

    assert!(!git_natural_state_passed(
        workspace.path(),
        seed,
        &seed_refs,
        &seed_fixture,
    )?);
    Ok(())
}

#[test]
fn git_natural_state_rejects_a_deleted_unrelated_fixture() -> EvalResult {
    let workspace = tempfile::tempdir()?;
    let (seed, seed_refs, seed_fixture) = seed_git_repository_with_refs(workspace.path())?;
    stage_path(workspace.path(), GIT_NATURAL_PATH)?;
    commit_staged_paths(workspace.path(), GIT_NATURAL_MESSAGE)?;
    fs::remove_file(workspace.path().join(GIT_STAGE_PATH))?;

    assert!(!git_natural_state_passed(
        workspace.path(),
        seed,
        &seed_refs,
        &seed_fixture,
    )?);
    Ok(())
}

#[cfg(unix)]
#[test]
fn git_natural_state_rejects_a_symlinked_untracked_fixture() -> EvalResult {
    let workspace = tempfile::tempdir()?;
    let support = tempfile::tempdir()?;
    let (seed, seed_refs, seed_fixture) = seed_git_repository_with_refs(workspace.path())?;
    stage_path(workspace.path(), GIT_NATURAL_PATH)?;
    commit_staged_paths(workspace.path(), GIT_NATURAL_MESSAGE)?;
    let replacement = support.path().join(GIT_STAGE_PATH);
    fs::write(&replacement, GIT_STAGE_CONTENT)?;
    fs::remove_file(workspace.path().join(GIT_STAGE_PATH))?;
    symlink(&replacement, workspace.path().join(GIT_STAGE_PATH))?;

    assert!(!git_natural_state_passed(
        workspace.path(),
        seed,
        &seed_refs,
        &seed_fixture,
    )?);
    Ok(())
}

#[test]
fn git_natural_state_rejects_repository_config_drift() -> EvalResult {
    let workspace = tempfile::tempdir()?;
    let (seed, seed_refs, seed_fixture) = seed_git_repository_with_refs(workspace.path())?;
    stage_path(workspace.path(), GIT_NATURAL_PATH)?;
    commit_staged_paths(workspace.path(), GIT_NATURAL_MESSAGE)?;
    Repository::open(workspace.path())?
        .config()?
        .set_str(SYNTHETIC_GIT_CONFIG_KEY, SYNTHETIC_GIT_CONFIG_VALUE)?;

    assert!(!git_natural_state_passed(
        workspace.path(),
        seed,
        &seed_refs,
        &seed_fixture,
    )?);
    Ok(())
}

#[cfg(unix)]
#[test]
fn git_natural_state_rejects_metadata_root_mode_drift() -> EvalResult {
    let workspace = tempfile::tempdir()?;
    let (seed, seed_refs, seed_fixture) = seed_git_repository_with_refs(workspace.path())?;
    stage_path(workspace.path(), GIT_NATURAL_PATH)?;
    commit_staged_paths(workspace.path(), GIT_NATURAL_MESSAGE)?;
    let metadata_root = Repository::open(workspace.path())?.path().to_path_buf();
    let mut permissions = fs::metadata(&metadata_root)?.permissions();
    permissions.set_mode(permissions.mode() ^ GROUP_WRITE_MODE_BIT);
    fs::set_permissions(&metadata_root, permissions)?;

    assert!(!git_natural_state_passed(
        workspace.path(),
        seed,
        &seed_refs,
        &seed_fixture,
    )?);
    Ok(())
}

#[cfg(unix)]
#[test]
fn git_natural_state_rejects_top_level_metadata_directory_mode_drift() -> EvalResult {
    let workspace = tempfile::tempdir()?;
    let (seed, seed_refs, seed_fixture) = seed_git_repository_with_refs(workspace.path())?;
    stage_path(workspace.path(), GIT_NATURAL_PATH)?;
    commit_staged_paths(workspace.path(), GIT_NATURAL_MESSAGE)?;
    let objects = Repository::open(workspace.path())?
        .path()
        .join(GIT_OBJECTS_DIRECTORY);
    let mut permissions = fs::metadata(&objects)?.permissions();
    permissions.set_mode(permissions.mode() ^ GROUP_WRITE_MODE_BIT);
    fs::set_permissions(&objects, permissions)?;

    assert!(!git_natural_state_passed(
        workspace.path(),
        seed,
        &seed_refs,
        &seed_fixture,
    )?);
    Ok(())
}

#[test]
fn git_natural_state_rejects_a_collateral_untracked_file() -> EvalResult {
    let workspace = tempfile::tempdir()?;
    let (seed, seed_refs, seed_fixture) = seed_git_repository_with_refs(workspace.path())?;
    stage_path(workspace.path(), GIT_NATURAL_PATH)?;
    commit_staged_paths(workspace.path(), GIT_NATURAL_MESSAGE)?;
    fs::write(
        workspace.path().join(GIT_COLLATERAL_PATH),
        GIT_COLLATERAL_CONTENT,
    )?;

    assert!(!git_natural_state_passed(
        workspace.path(),
        seed,
        &seed_refs,
        &seed_fixture,
    )?);
    Ok(())
}

#[test]
fn git_natural_state_rejects_a_collateral_empty_directory() -> EvalResult {
    let workspace = tempfile::tempdir()?;
    let (seed, seed_refs, seed_fixture) = seed_git_repository_with_refs(workspace.path())?;
    stage_path(workspace.path(), GIT_NATURAL_PATH)?;
    commit_staged_paths(workspace.path(), GIT_NATURAL_MESSAGE)?;
    fs::create_dir(workspace.path().join(GIT_COLLATERAL_DIRECTORY))?;

    assert!(!git_natural_state_passed(
        workspace.path(),
        seed,
        &seed_refs,
        &seed_fixture,
    )?);
    Ok(())
}

#[cfg(unix)]
#[test]
fn git_natural_state_rejects_a_collateral_pre_commit_hook() -> EvalResult {
    let workspace = tempfile::tempdir()?;
    let (seed, seed_refs, seed_fixture) = seed_git_repository_with_refs(workspace.path())?;
    stage_path(workspace.path(), GIT_NATURAL_PATH)?;
    commit_staged_paths(workspace.path(), GIT_NATURAL_MESSAGE)?;
    let hook = Repository::open(workspace.path())?
        .path()
        .join(GIT_PRE_COMMIT_HOOK_PATH);
    fs::write(&hook, GIT_PRE_COMMIT_HOOK_CONTENT)?;
    let mut permissions = fs::metadata(&hook)?.permissions();
    permissions.set_mode(permissions.mode() | USER_EXECUTE_MODE_BIT);
    fs::set_permissions(&hook, permissions)?;

    assert!(!git_natural_state_passed(
        workspace.path(),
        seed,
        &seed_refs,
        &seed_fixture,
    )?);
    Ok(())
}

#[test]
fn git_natural_state_rejects_a_collateral_ref_update() -> EvalResult {
    let workspace = tempfile::tempdir()?;
    let (seed, seed_refs, seed_fixture) = seed_git_repository_with_refs(workspace.path())?;
    stage_path(workspace.path(), GIT_NATURAL_PATH)?;
    commit_staged_paths(workspace.path(), GIT_NATURAL_MESSAGE)?;
    let repository = Repository::open(workspace.path())?;
    let head = repository.head()?.peel_to_commit()?;
    repository
        .find_reference("refs/heads/log-target")?
        .set_target(head.id(), "synthetic collateral ref update")?;

    assert!(!git_natural_state_passed(
        workspace.path(),
        seed,
        &seed_refs,
        &seed_fixture,
    )?);
    Ok(())
}

#[test]
fn git_natural_state_rejects_a_collateral_tag() -> EvalResult {
    let workspace = tempfile::tempdir()?;
    let (seed, seed_refs, seed_fixture) = seed_git_repository_with_refs(workspace.path())?;
    stage_path(workspace.path(), GIT_NATURAL_PATH)?;
    commit_staged_paths(workspace.path(), GIT_NATURAL_MESSAGE)?;
    let repository = Repository::open(workspace.path())?;
    let head = repository.head()?.peel_to_commit()?;
    repository.reference(
        "refs/tags/collateral-eval-tag",
        head.id(),
        true,
        "synthetic collateral tag",
    )?;

    assert!(!git_natural_state_passed(
        workspace.path(),
        seed,
        &seed_refs,
        &seed_fixture,
    )?);
    Ok(())
}

#[test]
fn git_natural_state_rejects_a_collateral_object() -> EvalResult {
    let workspace = tempfile::tempdir()?;
    let (seed, seed_refs, seed_fixture) = seed_git_repository_with_refs(workspace.path())?;
    stage_path(workspace.path(), GIT_NATURAL_PATH)?;
    commit_staged_paths(workspace.path(), GIT_NATURAL_MESSAGE)?;
    Repository::open(workspace.path())?.blob(GIT_COLLATERAL_OBJECT_CONTENT)?;

    assert!(!git_natural_state_passed(
        workspace.path(),
        seed,
        &seed_refs,
        &seed_fixture,
    )?);
    Ok(())
}

#[test]
fn git_natural_state_rejects_the_wrong_commit_identity() -> EvalResult {
    let workspace = tempfile::tempdir()?;
    let (seed, seed_refs, seed_fixture) = seed_git_repository_with_refs(workspace.path())?;
    stage_path(workspace.path(), GIT_NATURAL_PATH)?;
    commit_staged_paths_with_identity(
        workspace.path(),
        GIT_NATURAL_MESSAGE,
        SYNTHETIC_OTHER_GIT_AUTHOR_NAME,
        SYNTHETIC_OTHER_GIT_AUTHOR_EMAIL,
    )?;

    assert!(!git_natural_state_passed(
        workspace.path(),
        seed,
        &seed_refs,
        &seed_fixture,
    )?);
    Ok(())
}

#[test]
fn git_natural_state_rejects_an_unrelated_earlier_commit() -> EvalResult {
    let workspace = tempfile::tempdir()?;
    let (seed, seed_refs, seed_fixture) = seed_git_repository_with_refs(workspace.path())?;
    stage_path(workspace.path(), GIT_STAGE_PATH)?;
    commit_staged_paths(workspace.path(), "unrelated eval commit")?;
    stage_path(workspace.path(), GIT_NATURAL_PATH)?;
    commit_staged_paths(workspace.path(), GIT_NATURAL_MESSAGE)?;

    assert!(!git_natural_state_passed(
        workspace.path(),
        seed,
        &seed_refs,
        &seed_fixture,
    )?);
    Ok(())
}

#[test]
fn git_natural_state_rejects_a_parentless_seed_commit() -> EvalResult {
    let workspace = tempfile::tempdir()?;
    let (seed, seed_refs, seed_fixture) = seed_git_repository_with_refs(workspace.path())?;

    assert!(!git_natural_state_passed(
        workspace.path(),
        seed,
        &seed_refs,
        &seed_fixture,
    )?);
    Ok(())
}

#[test]
fn git_natural_state_rejects_a_commit_on_a_switched_branch() -> EvalResult {
    let workspace = tempfile::tempdir()?;
    let (seed, seed_refs, seed_fixture) = seed_git_repository_with_refs(workspace.path())?;
    let repository = Repository::open(workspace.path())?;
    let head = repository.head()?.peel_to_commit()?;
    repository.branch("natural-target", &head, false)?;
    repository.set_head("refs/heads/natural-target")?;
    stage_path(workspace.path(), GIT_NATURAL_PATH)?;
    commit_staged_paths(workspace.path(), GIT_NATURAL_MESSAGE)?;

    assert!(!git_natural_state_passed(
        workspace.path(),
        seed,
        &seed_refs,
        &seed_fixture,
    )?);
    Ok(())
}

#[test]
fn forced_git_stage_verifier_rejects_success_without_the_postcondition() -> EvalResult {
    let suite = FamilySuite::git()?;
    let case = GIT_CASES
        .iter()
        .find(|case| case.name == GIT_STAGE_NAME)
        .expect("the Git stage fixture exists");
    let result = serde_json::json!({
        "staged_paths": 1,
        EVAL_RECEIPT_FIELD: SYNTHETIC_EVAL_RECEIPT,
    })
    .to_string();

    assert!(!suite.forced_case_result_passed(case, &result)?);
    Ok(())
}

#[test]
fn forced_git_stage_verifier_accepts_the_exact_staged_blob() -> EvalResult {
    let suite = FamilySuite::git()?;
    let case = GIT_CASES
        .iter()
        .find(|case| case.name == GIT_STAGE_NAME)
        .expect("the Git stage fixture exists");
    let started = current_filesystem_recorded_time()?;
    stage_path(suite.workspace.path(), GIT_STAGE_PATH)?;
    suite.executor.record_filesystem_execution_window(
        GIT_STAGE_NAME,
        FilesystemExecutionTimeWindow {
            started,
            finished: current_filesystem_recorded_time()?,
        },
    );
    let repository = Repository::open(suite.workspace.path())?;
    let index = repository.index()?;
    let entry = index
        .get_path(Path::new(GIT_STAGE_PATH), 0)
        .expect("the exact staged fixture is indexed");
    let worktree_mode = fs::metadata(suite.workspace.path().join(GIT_STAGE_PATH))?
        .permissions()
        .mode()
        & 0o7777;
    let result = serde_json::json!({
        "staged_paths": 1,
        EVAL_RECEIPT_FIELD: SYNTHETIC_EVAL_RECEIPT,
    })
    .to_string();

    assert_eq!(entry.mode, GIT_REGULAR_INDEX_FILE_MODE);
    assert_eq!(
        Some(worktree_mode),
        suite
            .git_seed_fixture
            .modes
            .get(Path::new(GIT_STAGE_PATH))
            .copied()
            .flatten()
    );
    assert!(suite.forced_case_result_passed(case, &result)?);
    Ok(())
}

#[test]
fn git_object_entry_inventory_accepts_an_exact_pack_publication() -> EvalResult {
    let suite = FamilySuite::git()?;
    let repository = Repository::open(suite.workspace.path())?;
    let started = current_filesystem_recorded_time()?;
    let object_id = repository.blob(GIT_STAGE_CONTENT.as_bytes())?;
    publish_git_object_pack_for_test(
        &repository,
        &[object_id],
        &suite.git_seed_fixture.object_entries,
    )?;
    let execution_window = FilesystemExecutionTimeWindow {
        started,
        finished: current_filesystem_recorded_time()?,
    };

    assert!(git_object_entry_inventory_matches(
        suite.workspace.path(),
        &suite.git_seed_fixture.object_entries,
        &suite.git_seed_fixture.object_modified_times,
        &suite.git_seed_fixture.object_entry_identities,
        &[object_id],
        &suite.git_seed_fixture,
        Some(execution_window),
    )?);
    Ok(())
}

#[test]
fn git_object_entry_inventory_rejects_a_pack_with_a_collateral_object() -> EvalResult {
    let suite = FamilySuite::git()?;
    let repository = Repository::open(suite.workspace.path())?;
    let started = current_filesystem_recorded_time()?;
    let allowed_id = repository.blob(GIT_STAGE_CONTENT.as_bytes())?;
    let collateral_id = repository.blob(GIT_COLLATERAL_OBJECT_CONTENT)?;
    publish_git_object_pack_for_test(
        &repository,
        &[allowed_id, collateral_id],
        &suite.git_seed_fixture.object_entries,
    )?;
    let execution_window = FilesystemExecutionTimeWindow {
        started,
        finished: current_filesystem_recorded_time()?,
    };

    assert!(!git_object_entry_inventory_matches(
        suite.workspace.path(),
        &suite.git_seed_fixture.object_entries,
        &suite.git_seed_fixture.object_modified_times,
        &suite.git_seed_fixture.object_entry_identities,
        &[allowed_id],
        &suite.git_seed_fixture,
        Some(execution_window),
    )?);
    Ok(())
}

#[test]
fn forced_git_stage_verifier_rejects_unrelated_stat_cache_drift() -> EvalResult {
    let suite = FamilySuite::git()?;
    suite.prepare_git_case(GIT_STAGE_NAME)?;
    let case = GIT_CASES
        .iter()
        .find(|case| case.name == GIT_STAGE_NAME)
        .expect("the Git stage fixture exists");
    stage_path(suite.workspace.path(), GIT_STAGE_PATH)?;
    drift_git_index_ctime(suite.workspace.path(), GIT_SEED_PATH)?;
    let result = serde_json::json!({
        "staged_paths": 1,
        EVAL_RECEIPT_FIELD: SYNTHETIC_EVAL_RECEIPT,
    })
    .to_string();

    assert!(!suite.forced_case_result_passed(case, &result)?);
    Ok(())
}

#[test]
fn forced_git_stage_verifier_rejects_a_collateral_object() -> EvalResult {
    let suite = FamilySuite::git()?;
    let case = GIT_CASES
        .iter()
        .find(|case| case.name == GIT_STAGE_NAME)
        .expect("the Git stage fixture exists");
    stage_path(suite.workspace.path(), GIT_STAGE_PATH)?;
    Repository::open(suite.workspace.path())?.blob(GIT_COLLATERAL_OBJECT_CONTENT)?;
    let result = serde_json::json!({
        "staged_paths": 1,
        EVAL_RECEIPT_FIELD: SYNTHETIC_EVAL_RECEIPT,
    })
    .to_string();

    assert!(!suite.forced_case_result_passed(case, &result)?);
    Ok(())
}

#[test]
fn forced_git_stage_verifier_rejects_the_wrong_staged_blob() -> EvalResult {
    let suite = FamilySuite::git()?;
    let case = GIT_CASES
        .iter()
        .find(|case| case.name == GIT_STAGE_NAME)
        .expect("the Git stage fixture exists");
    fs::write(
        suite.workspace.path().join(GIT_STAGE_PATH),
        GIT_WRONG_STAGE_CONTENT,
    )?;
    stage_path(suite.workspace.path(), GIT_STAGE_PATH)?;
    fs::write(
        suite.workspace.path().join(GIT_STAGE_PATH),
        GIT_STAGE_CONTENT,
    )?;
    let result = serde_json::json!({
        "staged_paths": 1,
        EVAL_RECEIPT_FIELD: SYNTHETIC_EVAL_RECEIPT,
    })
    .to_string();

    assert!(!suite.forced_case_result_passed(case, &result)?);
    Ok(())
}

#[test]
fn forced_git_stage_verifier_rejects_an_extra_staged_fixture() -> EvalResult {
    let suite = FamilySuite::git()?;
    let case = GIT_CASES
        .iter()
        .find(|case| case.name == GIT_STAGE_NAME)
        .expect("the Git stage fixture exists");
    stage_path(suite.workspace.path(), GIT_STAGE_PATH)?;
    stage_path(suite.workspace.path(), GIT_COMMIT_PATH)?;
    let result = serde_json::json!({
        "staged_paths": 1,
        EVAL_RECEIPT_FIELD: SYNTHETIC_EVAL_RECEIPT,
    })
    .to_string();

    assert!(!suite.forced_case_result_passed(case, &result)?);
    Ok(())
}

#[test]
fn forced_git_stage_verifier_rejects_a_switched_branch() -> EvalResult {
    let suite = FamilySuite::git()?;
    let case = GIT_CASES
        .iter()
        .find(|case| case.name == GIT_STAGE_NAME)
        .expect("the Git stage fixture exists");
    let repository = Repository::open(suite.workspace.path())?;
    repository.set_head("refs/heads/switch-target")?;
    repository.checkout_head(None)?;
    stage_path(suite.workspace.path(), GIT_STAGE_PATH)?;
    let result = serde_json::json!({
        "staged_paths": 1,
        EVAL_RECEIPT_FIELD: SYNTHETIC_EVAL_RECEIPT,
    })
    .to_string();

    assert!(!suite.forced_case_result_passed(case, &result)?);
    Ok(())
}

#[test]
fn forced_git_stage_verifier_rejects_mutated_unrelated_fixtures() -> EvalResult {
    let suite = FamilySuite::git()?;
    let case = GIT_CASES
        .iter()
        .find(|case| case.name == GIT_STAGE_NAME)
        .expect("the Git stage fixture exists");
    stage_path(suite.workspace.path(), GIT_STAGE_PATH)?;
    fs::write(
        suite.workspace.path().join(GIT_COMMIT_PATH),
        GIT_NATURAL_CONTENT,
    )?;
    fs::write(
        suite.workspace.path().join(GIT_NATURAL_PATH),
        GIT_COMMIT_CONTENT,
    )?;
    let result = serde_json::json!({
        "staged_paths": 1,
        EVAL_RECEIPT_FIELD: SYNTHETIC_EVAL_RECEIPT,
    })
    .to_string();

    assert!(!suite.forced_case_result_passed(case, &result)?);
    Ok(())
}

#[cfg(unix)]
#[test]
fn forced_git_stage_verifier_rejects_mode_drift_in_an_unrelated_fixture() -> EvalResult {
    let suite = FamilySuite::git()?;
    let case = GIT_CASES
        .iter()
        .find(|case| case.name == GIT_STAGE_NAME)
        .expect("the Git stage fixture exists");
    let path = suite.workspace.path().join(GIT_COMMIT_PATH);
    let mut permissions = fs::metadata(&path)?.permissions();
    permissions.set_mode(permissions.mode() | USER_EXECUTE_MODE_BIT);
    fs::set_permissions(&path, permissions)?;
    stage_path(suite.workspace.path(), GIT_STAGE_PATH)?;
    let result = serde_json::json!({
        "staged_paths": 1,
        EVAL_RECEIPT_FIELD: SYNTHETIC_EVAL_RECEIPT,
    })
    .to_string();

    assert!(!suite.forced_case_result_passed(case, &result)?);
    Ok(())
}

#[cfg(unix)]
#[test]
fn forced_git_stage_verifier_rejects_an_executable_staged_file() -> EvalResult {
    let suite = FamilySuite::git()?;
    let case = GIT_CASES
        .iter()
        .find(|case| case.name == GIT_STAGE_NAME)
        .expect("the Git stage fixture exists");
    let path = suite.workspace.path().join(GIT_STAGE_PATH);
    let mut permissions = fs::metadata(&path)?.permissions();
    permissions.set_mode(permissions.mode() | USER_EXECUTE_MODE_BIT);
    fs::set_permissions(&path, permissions)?;
    stage_path(suite.workspace.path(), GIT_STAGE_PATH)?;
    let result = serde_json::json!({
        "staged_paths": 1,
        EVAL_RECEIPT_FIELD: SYNTHETIC_EVAL_RECEIPT,
    })
    .to_string();

    assert!(!suite.forced_case_result_passed(case, &result)?);
    Ok(())
}

#[cfg(unix)]
#[test]
fn forced_git_stage_verifier_rejects_a_symlinked_untracked_fixture() -> EvalResult {
    let suite = FamilySuite::git()?;
    let support = tempfile::tempdir()?;
    let case = GIT_CASES
        .iter()
        .find(|case| case.name == GIT_STAGE_NAME)
        .expect("the Git stage fixture exists");
    let replacement = support.path().join(GIT_COMMIT_PATH);
    fs::write(&replacement, GIT_COMMIT_CONTENT)?;
    fs::remove_file(suite.workspace.path().join(GIT_COMMIT_PATH))?;
    symlink(&replacement, suite.workspace.path().join(GIT_COMMIT_PATH))?;
    stage_path(suite.workspace.path(), GIT_STAGE_PATH)?;
    let result = serde_json::json!({
        "staged_paths": 1,
        EVAL_RECEIPT_FIELD: SYNTHETIC_EVAL_RECEIPT,
    })
    .to_string();

    assert!(!suite.forced_case_result_passed(case, &result)?);
    Ok(())
}

#[test]
fn forced_git_commit_verifier_accepts_the_exact_fixture_tree() -> EvalResult {
    let suite = FamilySuite::git()?;
    let case = GIT_CASES
        .iter()
        .find(|case| case.name == GIT_CREATE_COMMIT_NAME)
        .expect("the Git commit fixture exists");
    let arguments: serde_json::Value = serde_json::from_str(case.expected_arguments)?;
    let message = arguments["message"]
        .as_str()
        .expect("the Git commit fixture has a message");
    suite.prepare_git_case(GIT_CREATE_COMMIT_NAME)?;
    suite.commit_staged_paths_for_test(message)?;
    let head = Repository::open(suite.workspace.path())?
        .head()?
        .peel_to_commit()?
        .id()
        .to_string();
    let result = serde_json::json!({
        "commit": head,
        "state_cleaned": true,
        EVAL_RECEIPT_FIELD: SYNTHETIC_EVAL_RECEIPT,
    })
    .to_string();

    assert!(suite.forced_case_result_passed(case, &result)?);
    Ok(())
}

#[test]
fn forced_git_commit_verifier_rejects_a_collateral_object() -> EvalResult {
    let suite = FamilySuite::git()?;
    let case = GIT_CASES
        .iter()
        .find(|case| case.name == GIT_CREATE_COMMIT_NAME)
        .expect("the Git commit fixture exists");
    let arguments: serde_json::Value = serde_json::from_str(case.expected_arguments)?;
    let message = arguments["message"]
        .as_str()
        .expect("the Git commit fixture has a message");
    suite.prepare_git_case(GIT_CREATE_COMMIT_NAME)?;
    suite.commit_staged_paths_for_test(message)?;
    let repository = Repository::open(suite.workspace.path())?;
    repository.blob(GIT_COLLATERAL_OBJECT_CONTENT)?;
    let head = repository.head()?.peel_to_commit()?.id().to_string();
    let result = serde_json::json!({
        "commit": head,
        "state_cleaned": true,
        EVAL_RECEIPT_FIELD: SYNTHETIC_EVAL_RECEIPT,
    })
    .to_string();

    assert!(!suite.forced_case_result_passed(case, &result)?);
    Ok(())
}

#[test]
fn forced_git_commit_verifier_rejects_a_missing_branch_reflog_record() -> EvalResult {
    let suite = FamilySuite::git()?;
    let case = GIT_CASES
        .iter()
        .find(|case| case.name == GIT_CREATE_COMMIT_NAME)
        .expect("the Git commit fixture exists");
    let arguments: serde_json::Value = serde_json::from_str(case.expected_arguments)?;
    let message = arguments["message"]
        .as_str()
        .expect("the Git commit fixture has a message");
    suite.prepare_git_case(GIT_CREATE_COMMIT_NAME)?;
    suite.commit_staged_paths_for_test(message)?;
    let repository = Repository::open(suite.workspace.path())?;
    let head = repository.head()?.peel_to_commit()?.id();
    let branch_log = repository
        .path()
        .join(GIT_LOGS_DIRECTORY)
        .join("refs/heads")
        .join(GIT_BASE_BRANCH);
    let contents = fs::read(&branch_log)?;
    let previous_record_end = contents[..contents.len() - 1]
        .iter()
        .rposition(|byte| *byte == b'\n')
        .expect("the seeded branch reflog has a previous record");
    fs::write(&branch_log, &contents[..=previous_record_end])?;
    let result = serde_json::json!({
        "commit": head.to_string(),
        "state_cleaned": true,
        EVAL_RECEIPT_FIELD: SYNTHETIC_EVAL_RECEIPT,
    })
    .to_string();

    assert!(!suite.forced_case_result_passed(case, &result)?);
    Ok(())
}

#[test]
fn forced_git_commit_verifier_rejects_reflog_timestamp_drift() -> EvalResult {
    let suite = FamilySuite::git()?;
    let case = GIT_CASES
        .iter()
        .find(|case| case.name == GIT_CREATE_COMMIT_NAME)
        .expect("the Git commit fixture exists");
    let arguments: serde_json::Value = serde_json::from_str(case.expected_arguments)?;
    let message = arguments["message"]
        .as_str()
        .expect("the Git commit fixture has a message");
    suite.prepare_git_case(GIT_CREATE_COMMIT_NAME)?;
    suite.commit_staged_paths_for_test(message)?;
    let repository = Repository::open(suite.workspace.path())?;
    let head = repository.head()?.peel_to_commit()?;
    let commit_time = head.committer().when();
    let altered_time = Time::new(commit_time.seconds() + 1, commit_time.offset_minutes());
    let altered_signature = Signature::new(GIT_AUTHOR_NAME, GIT_AUTHOR_EMAIL, &altered_time)?;
    replace_latest_reflog_signature(&repository, "HEAD", &altered_signature)?;
    let result = serde_json::json!({
        "commit": head.id().to_string(),
        "state_cleaned": true,
        EVAL_RECEIPT_FIELD: SYNTHETIC_EVAL_RECEIPT,
    })
    .to_string();

    assert!(!suite.forced_case_result_passed(case, &result)?);
    Ok(())
}

#[test]
fn forced_git_commit_verifier_rejects_a_mutated_untracked_fixture() -> EvalResult {
    let suite = FamilySuite::git()?;
    let case = GIT_CASES
        .iter()
        .find(|case| case.name == GIT_CREATE_COMMIT_NAME)
        .expect("the Git commit fixture exists");
    let arguments: serde_json::Value = serde_json::from_str(case.expected_arguments)?;
    let message = arguments["message"]
        .as_str()
        .expect("the Git commit fixture has a message");
    suite.prepare_git_case(GIT_CREATE_COMMIT_NAME)?;
    suite.commit_staged_paths_for_test(message)?;
    fs::write(
        suite.workspace.path().join(GIT_STAGE_PATH),
        GIT_NATURAL_CONTENT,
    )?;
    let head = Repository::open(suite.workspace.path())?
        .head()?
        .peel_to_commit()?
        .id()
        .to_string();
    let result = serde_json::json!({
        "commit": head,
        "state_cleaned": true,
        EVAL_RECEIPT_FIELD: SYNTHETIC_EVAL_RECEIPT,
    })
    .to_string();

    assert!(!suite.forced_case_result_passed(case, &result)?);
    Ok(())
}

#[test]
fn forced_git_commit_verifier_rejects_a_detached_head_without_branch_advancement() -> EvalResult {
    let suite = FamilySuite::git()?;
    let case = GIT_CASES
        .iter()
        .find(|case| case.name == GIT_CREATE_COMMIT_NAME)
        .expect("the Git commit fixture exists");
    let arguments: serde_json::Value = serde_json::from_str(case.expected_arguments)?;
    let message = arguments["message"]
        .as_str()
        .expect("the Git commit fixture has a message");
    suite.prepare_git_case(GIT_CREATE_COMMIT_NAME)?;
    suite.commit_staged_paths_for_test(message)?;
    let repository = Repository::open(suite.workspace.path())?;
    let head = repository.head()?.peel_to_commit()?.id();
    let seed = suite
        .git_seed
        .expect("the Git eval suite has a captured seed identity");
    repository.reference(
        &format!("refs/heads/{GIT_BASE_BRANCH}"),
        seed,
        true,
        GIT_RESTORE_BRANCH_REFLOG_MESSAGE,
    )?;
    repository.set_head_detached(head)?;
    let result = serde_json::json!({
        "commit": head.to_string(),
        "state_cleaned": true,
        EVAL_RECEIPT_FIELD: SYNTHETIC_EVAL_RECEIPT,
    })
    .to_string();

    assert!(!suite.forced_case_result_passed(case, &result)?);
    Ok(())
}

#[test]
fn forced_git_commit_verifier_rejects_the_wrong_fixture_tree() -> EvalResult {
    let suite = FamilySuite::git()?;
    let case = GIT_CASES
        .iter()
        .find(|case| case.name == GIT_CREATE_COMMIT_NAME)
        .expect("the Git commit fixture exists");
    let arguments: serde_json::Value = serde_json::from_str(case.expected_arguments)?;
    let message = arguments["message"]
        .as_str()
        .expect("the Git commit fixture has a message");
    suite.prepare_git_case(GIT_CREATE_COMMIT_NAME)?;
    let repository = Repository::open(suite.workspace.path())?;
    let mut index = repository.index()?;
    index.remove_path(Path::new(GIT_COMMIT_PATH))?;
    index.add_path(Path::new(GIT_STAGE_PATH))?;
    index.write()?;
    suite.commit_staged_paths_for_test(message)?;
    let head = Repository::open(suite.workspace.path())?
        .head()?
        .peel_to_commit()?
        .id()
        .to_string();
    let result = serde_json::json!({
        "commit": head,
        "state_cleaned": true,
        EVAL_RECEIPT_FIELD: SYNTHETIC_EVAL_RECEIPT,
    })
    .to_string();

    assert!(!suite.forced_case_result_passed(case, &result)?);
    Ok(())
}

#[test]
fn forced_git_commit_verifier_rejects_the_wrong_identity() -> EvalResult {
    let suite = FamilySuite::git()?;
    let case = GIT_CASES
        .iter()
        .find(|case| case.name == GIT_CREATE_COMMIT_NAME)
        .expect("the Git commit fixture exists");
    let arguments: serde_json::Value = serde_json::from_str(case.expected_arguments)?;
    let message = arguments["message"]
        .as_str()
        .expect("the Git commit fixture has a message");
    suite.prepare_git_case(GIT_CREATE_COMMIT_NAME)?;
    suite.commit_staged_paths_with_identity_for_test(
        message,
        SYNTHETIC_OTHER_GIT_AUTHOR_NAME,
        SYNTHETIC_OTHER_GIT_AUTHOR_EMAIL,
    )?;
    let head = Repository::open(suite.workspace.path())?
        .head()?
        .peel_to_commit()?
        .id()
        .to_string();
    let result = serde_json::json!({
        "commit": head,
        "state_cleaned": true,
        EVAL_RECEIPT_FIELD: SYNTHETIC_EVAL_RECEIPT,
    })
    .to_string();

    assert!(!suite.forced_case_result_passed(case, &result)?);
    Ok(())
}

#[test]
fn forced_git_commit_verifier_rejects_retained_merge_state() -> EvalResult {
    let suite = FamilySuite::git()?;
    let case = GIT_CASES
        .iter()
        .find(|case| case.name == GIT_CREATE_COMMIT_NAME)
        .expect("the Git commit fixture exists");
    let arguments: serde_json::Value = serde_json::from_str(case.expected_arguments)?;
    let message = arguments["message"]
        .as_str()
        .expect("the Git commit fixture has a message");
    suite.prepare_git_case(GIT_CREATE_COMMIT_NAME)?;
    suite.commit_staged_paths_for_test(message)?;
    install_git_merge_state(
        suite.workspace.path(),
        suite
            .git_seed
            .expect("the Git eval suite has a captured seed identity"),
    )?;
    let head = Repository::open(suite.workspace.path())?
        .head()?
        .peel_to_commit()?
        .id()
        .to_string();
    let result = serde_json::json!({
        "commit": head,
        "state_cleaned": true,
        EVAL_RECEIPT_FIELD: SYNTHETIC_EVAL_RECEIPT,
    })
    .to_string();

    assert!(!suite.forced_case_result_passed(case, &result)?);
    Ok(())
}

#[test]
fn forced_git_commit_verifier_rejects_retained_cherry_pick_state() -> EvalResult {
    let suite = FamilySuite::git()?;
    let case = GIT_CASES
        .iter()
        .find(|case| case.name == GIT_CREATE_COMMIT_NAME)
        .expect("the Git commit fixture exists");
    let arguments: serde_json::Value = serde_json::from_str(case.expected_arguments)?;
    let message = arguments["message"]
        .as_str()
        .expect("the Git commit fixture has a message");
    suite.prepare_git_case(GIT_CREATE_COMMIT_NAME)?;
    suite.commit_staged_paths_for_test(message)?;
    let repository = Repository::open(suite.workspace.path())?;
    let head = repository.head()?.peel_to_commit()?.id();
    fs::write(
        repository.path().join(GIT_CHERRY_PICK_HEAD_PATH),
        format!("{head}\n"),
    )?;
    let result = serde_json::json!({
        "commit": head.to_string(),
        "state_cleaned": true,
        EVAL_RECEIPT_FIELD: SYNTHETIC_EVAL_RECEIPT,
    })
    .to_string();

    assert!(!suite.forced_case_result_passed(case, &result)?);
    Ok(())
}

#[test]
fn forced_git_branch_create_verifier_accepts_the_exact_new_reference() -> EvalResult {
    let suite = FamilySuite::git()?;
    let case = GIT_CASES
        .iter()
        .find(|case| case.name == GIT_BRANCH_CREATE_NAME)
        .expect("the Git branch-create fixture exists");
    let arguments: serde_json::Value = serde_json::from_str(case.expected_arguments)?;
    let branch_name = arguments["name"]
        .as_str()
        .expect("the Git branch-create fixture has a name");
    let start = arguments["start"]
        .as_str()
        .expect("the Git branch-create fixture has a start reference");
    let started = current_filesystem_recorded_time()?;
    let repository = Repository::open(suite.workspace.path())?;
    let target = repository.find_reference(start)?.peel_to_commit()?;
    fs::write(
        repository.path().join("refs/heads").join(branch_name),
        format!("{}\n", target.id()),
    )?;
    suite.executor.record_filesystem_execution_window(
        GIT_BRANCH_CREATE_NAME,
        FilesystemExecutionTimeWindow {
            started,
            finished: current_filesystem_recorded_time()?,
        },
    );
    let result = serde_json::json!({
        "branch": branch_name,
        "head": target.id().to_string(),
        EVAL_RECEIPT_FIELD: SYNTHETIC_EVAL_RECEIPT,
    })
    .to_string();

    assert!(suite.forced_case_result_passed(case, &result)?);
    Ok(())
}

#[test]
fn forced_git_branch_create_verifier_rejects_the_default_head() -> EvalResult {
    let suite = FamilySuite::git()?;
    let case = GIT_CASES
        .iter()
        .find(|case| case.name == GIT_BRANCH_CREATE_NAME)
        .expect("the Git branch-create fixture exists");
    let repository = Repository::open(suite.workspace.path())?;
    let head = repository.head()?.peel_to_commit()?;
    repository.branch("created-by-eval", &head, false)?;
    let result = serde_json::json!({
        "branch": "created-by-eval",
        "head": head.id().to_string(),
        EVAL_RECEIPT_FIELD: SYNTHETIC_EVAL_RECEIPT,
    })
    .to_string();

    assert!(!suite.forced_case_result_passed(case, &result)?);
    Ok(())
}

#[test]
fn forced_git_branch_create_verifier_rejects_switching_to_the_created_branch() -> EvalResult {
    let suite = FamilySuite::git()?;
    let case = GIT_CASES
        .iter()
        .find(|case| case.name == GIT_BRANCH_CREATE_NAME)
        .expect("the Git branch-create fixture exists");
    let repository = Repository::open(suite.workspace.path())?;
    let target = repository
        .find_branch("log-target", BranchType::Local)?
        .into_reference()
        .peel_to_commit()?;
    repository.branch("created-by-eval", &target, false)?;
    repository.set_head("refs/heads/created-by-eval")?;
    let result = serde_json::json!({
        "branch": "created-by-eval",
        "head": target.id().to_string(),
        EVAL_RECEIPT_FIELD: SYNTHETIC_EVAL_RECEIPT,
    })
    .to_string();

    assert!(!suite.forced_case_result_passed(case, &result)?);
    Ok(())
}

#[test]
fn forced_git_branch_create_verifier_rejects_an_extra_branch() -> EvalResult {
    let suite = FamilySuite::git()?;
    let case = GIT_CASES
        .iter()
        .find(|case| case.name == GIT_BRANCH_CREATE_NAME)
        .expect("the Git branch-create fixture exists");
    let repository = Repository::open(suite.workspace.path())?;
    let target = repository
        .find_branch("log-target", BranchType::Local)?
        .into_reference()
        .peel_to_commit()?;
    repository.branch("created-by-eval", &target, false)?;
    repository.branch("collateral-branch", &target, false)?;
    let result = serde_json::json!({
        "branch": "created-by-eval",
        "head": target.id().to_string(),
        EVAL_RECEIPT_FIELD: SYNTHETIC_EVAL_RECEIPT,
    })
    .to_string();

    assert!(!suite.forced_case_result_passed(case, &result)?);
    Ok(())
}

#[test]
fn forced_git_branch_create_verifier_rejects_an_untracked_fixture_change() -> EvalResult {
    let suite = FamilySuite::git()?;
    let case = GIT_CASES
        .iter()
        .find(|case| case.name == GIT_BRANCH_CREATE_NAME)
        .expect("the Git branch-create fixture exists");
    let repository = Repository::open(suite.workspace.path())?;
    let target = repository
        .find_branch("log-target", BranchType::Local)?
        .into_reference()
        .peel_to_commit()?;
    repository.branch("created-by-eval", &target, false)?;
    fs::write(suite.workspace.path().join(GIT_STAGE_PATH), b"collateral\n")?;
    let result = serde_json::json!({
        "branch": "created-by-eval",
        "head": target.id().to_string(),
        EVAL_RECEIPT_FIELD: SYNTHETIC_EVAL_RECEIPT,
    })
    .to_string();

    assert!(!suite.forced_case_result_passed(case, &result)?);
    Ok(())
}

#[test]
fn forced_git_branch_switch_verifier_rejects_a_head_only_update() -> EvalResult {
    let suite = FamilySuite::git()?;
    let case = GIT_CASES
        .iter()
        .find(|case| case.name == GIT_BRANCH_SWITCH_NAME)
        .expect("the Git branch-switch fixture exists");
    let repository = Repository::open(suite.workspace.path())?;
    let target = repository
        .find_branch("switch-target", BranchType::Local)?
        .get()
        .target()
        .expect("the Git branch-switch fixture has a target");
    repository.set_head("refs/heads/switch-target")?;
    let result = serde_json::json!({
        "branch": "switch-target",
        "head": target.to_string(),
        EVAL_RECEIPT_FIELD: SYNTHETIC_EVAL_RECEIPT,
    })
    .to_string();

    assert!(!suite.forced_case_result_passed(case, &result)?);
    Ok(())
}

#[cfg(unix)]
#[test]
fn forced_git_branch_switch_timestamp_gates_accept_the_exact_checkout() -> EvalResult {
    let fixture = git_branch_switch_timestamp_fixture()?;
    let actual_worktree_modified_times =
        git_worktree_modified_times(fixture.suite.workspace.path())?;

    assert_eq!(
        fs::read_to_string(fixture.suite.workspace.path().join(GIT_SEED_PATH))?,
        GIT_SWITCH_CONTENT,
    );
    assert!(git_forced_metadata_root_modified_time_matches(
        fixture.suite.workspace.path(),
        GIT_BRANCH_SWITCH_NAME,
        &fixture.suite.git_seed_fixture,
        Some(fixture.pre_metadata_root_modified_time),
        Some(fixture.pre_metadata_root_identity),
        Some(fixture.execution_window),
    )?);
    assert!(
        git_forced_worktree_modified_times_match(
            fixture.suite.workspace.path(),
            GIT_BRANCH_SWITCH_NAME,
            &fixture.suite.git_seed_fixture,
            Some(&fixture.pre_worktree_modified_times),
            Some(fixture.execution_window),
        )?,
        "actual target time: {:?}; execution window: {:?}",
        actual_worktree_modified_times[Path::new(GIT_SEED_PATH)],
        fixture.execution_window,
    );
    assert!(git_forced_worktree_entry_identities_match(
        fixture.suite.workspace.path(),
        GIT_BRANCH_SWITCH_NAME,
        &fixture.suite.git_seed_fixture,
        Some(&fixture.pre_worktree_entry_identities),
        Some(fixture.execution_window),
    )?);
    Ok(())
}

#[cfg(unix)]
#[test]
fn forced_git_metadata_root_gate_rejects_an_epoch_mtime_after_switch() -> EvalResult {
    let fixture = git_branch_switch_timestamp_fixture()?;
    let repository = Repository::open(fixture.suite.workspace.path())?;
    fs::File::open(repository.path())?.set_times(fs::FileTimes::new().set_modified(UNIX_EPOCH))?;

    assert!(!git_forced_metadata_root_modified_time_matches(
        fixture.suite.workspace.path(),
        GIT_BRANCH_SWITCH_NAME,
        &fixture.suite.git_seed_fixture,
        Some(fixture.pre_metadata_root_modified_time),
        Some(fixture.pre_metadata_root_identity),
        Some(fixture.execution_window),
    )?);
    Ok(())
}

#[cfg(unix)]
#[test]
fn forced_git_metadata_root_gate_rejects_an_epoch_change_time() -> EvalResult {
    let fixture = git_branch_switch_timestamp_fixture()?;
    let mut actual_identity = git_metadata_root_identity(fixture.suite.workspace.path())?
        .expect("the switched Git fixture has a metadata-root identity");
    actual_identity.change_time_seconds = 0;
    actual_identity.change_time_nanoseconds = 0;

    assert!(!git_mutated_metadata_root_times_match(
        git_metadata_root_modified_time(fixture.suite.workspace.path())?,
        Some(actual_identity),
        Some(fixture.pre_metadata_root_identity),
        Some(fixture.execution_window),
    ));
    Ok(())
}

#[cfg(unix)]
#[test]
fn forced_git_branch_switch_gate_rejects_an_epoch_target_mtime() -> EvalResult {
    let fixture = git_branch_switch_timestamp_fixture()?;
    fs::File::open(fixture.suite.workspace.path().join(GIT_SEED_PATH))?
        .set_times(fs::FileTimes::new().set_modified(UNIX_EPOCH))?;

    assert!(!git_forced_worktree_modified_times_match(
        fixture.suite.workspace.path(),
        GIT_BRANCH_SWITCH_NAME,
        &fixture.suite.git_seed_fixture,
        Some(&fixture.pre_worktree_modified_times),
        Some(fixture.execution_window),
    )?);
    Ok(())
}

#[cfg(unix)]
#[test]
fn forced_git_branch_switch_gate_rejects_an_epoch_target_change_time() -> EvalResult {
    let fixture = git_branch_switch_timestamp_fixture()?;
    let target = Path::new(GIT_SEED_PATH);
    let mut actual_identity =
        git_worktree_entry_identities(fixture.suite.workspace.path())?[target];
    actual_identity.change_time_seconds = 0;
    actual_identity.change_time_nanoseconds = 0;

    assert!(!git_branch_switch_target_identity_matches(
        Some(&actual_identity),
        fixture.pre_worktree_entry_identities.get(target),
        Some(fixture.execution_window),
    ));
    Ok(())
}

#[cfg(target_os = "linux")]
#[test]
fn forced_git_branch_switch_attribute_gate_rejects_target_drift() -> EvalResult {
    let suite = FamilySuite::git()?;
    suite.prepare_git_case(GIT_BRANCH_SWITCH_NAME)?;
    let pre_execution = suite
        .git_pre_execution_worktree_extended_attributes
        .lock()
        .expect("Git pre-execution worktree-attribute lock is available");
    let relative = Path::new(GIT_SEED_PATH);
    let target = suite.workspace.path().join(relative);
    assert!(git_forced_worktree_extended_attributes_match(
        suite.workspace.path(),
        &suite.git_seed_fixture,
        pre_execution.as_ref(),
    )?);
    rustix::fs::setxattr(
        &target,
        SYNTHETIC_UNEXPECTED_XATTR_NAME,
        SYNTHETIC_UNEXPECTED_XATTR_VALUE,
        rustix::fs::XattrFlags::CREATE,
    )?;

    assert_ne!(
        git_worktree_extended_attributes(suite.workspace.path())?[relative],
        pre_execution
            .as_ref()
            .expect("the Git branch-switch fixture has a captured attribute inventory")[relative]
    );
    assert!(!git_forced_worktree_extended_attributes_match(
        suite.workspace.path(),
        &suite.git_seed_fixture,
        pre_execution.as_ref(),
    )?);
    Ok(())
}

#[cfg(target_os = "linux")]
#[test]
fn forced_git_metadata_attribute_gate_rejects_index_drift() -> EvalResult {
    let suite = FamilySuite::git()?;
    suite.prepare_git_case(GIT_STAGE_NAME)?;
    let pre_execution = suite
        .git_pre_execution_metadata_extended_attributes
        .lock()
        .expect("Git pre-execution metadata-attribute lock is available");
    let repository = Repository::open(suite.workspace.path())?;
    let relative = Path::new(GIT_INDEX_PATH);
    let target = repository.path().join(relative);
    assert!(git_metadata_extended_attributes_match(
        suite.workspace.path(),
        &suite.git_seed_fixture,
        pre_execution.as_ref(),
    )?);
    rustix::fs::setxattr(
        &target,
        SYNTHETIC_UNEXPECTED_XATTR_NAME,
        SYNTHETIC_UNEXPECTED_XATTR_VALUE,
        rustix::fs::XattrFlags::CREATE,
    )?;

    assert_ne!(
        git_metadata_extended_attributes(suite.workspace.path())?[relative],
        pre_execution
            .as_ref()
            .expect("the Git stage fixture has a captured metadata-attribute inventory")[relative]
    );
    assert!(!git_metadata_extended_attributes_match(
        suite.workspace.path(),
        &suite.git_seed_fixture,
        pre_execution.as_ref(),
    )?);
    Ok(())
}

#[cfg(unix)]
#[test]
fn forced_git_metadata_mutation_gate_rejects_index_ownership_drift() -> EvalResult {
    let suite = FamilySuite::git()?;
    let mut actual = git_metadata_top_level(suite.workspace.path())?;
    let mut expected = actual.clone();
    let index = Path::new(GIT_INDEX_PATH);
    let changed_group_id = actual[index]
        .identity
        .expect("the Git index has a filesystem identity")
        .group_id
        .wrapping_add(1);
    actual
        .get_mut(index)
        .expect("the Git index has a metadata snapshot")
        .identity
        .as_mut()
        .expect("the Git index has a mutable filesystem identity")
        .group_id = changed_group_id;

    assert!(!admit_git_metadata_file_mutation(
        &actual,
        &mut expected,
        index,
        None,
    ));
    Ok(())
}

#[cfg(unix)]
#[test]
fn forced_git_metadata_mutation_gate_rejects_an_out_of_window_index_mtime() -> EvalResult {
    let suite = FamilySuite::git()?;
    let index = Path::new(GIT_INDEX_PATH);
    let mut expected = git_metadata_top_level(suite.workspace.path())?;
    let started = current_filesystem_recorded_time()?;
    stage_path(suite.workspace.path(), GIT_STAGE_PATH)?;
    let window = FilesystemExecutionTimeWindow {
        started,
        finished: current_filesystem_recorded_time()?,
    };
    let mut actual = git_metadata_top_level(suite.workspace.path())?;
    actual
        .get_mut(index)
        .expect("the Git index has a metadata snapshot")
        .modified = Some(UNIX_EPOCH);

    assert!(!admit_git_metadata_file_mutation(
        &actual,
        &mut expected,
        index,
        Some(window),
    ));
    Ok(())
}

#[cfg(unix)]
#[test]
fn rewritten_git_identity_gate_rejects_changed_ownership() -> EvalResult {
    let suite = FamilySuite::git()?;
    let target = Path::new("heads").join(GIT_BASE_BRANCH);
    let mut actual = suite.git_seed_fixture.reference_entry_identities.clone();
    let mut expected = actual.clone();
    let target_identity = actual[&target];
    let recorded = UNIX_EPOCH
        + Duration::new(
            target_identity
                .change_time_seconds
                .try_into()
                .expect("the captured change time is nonnegative"),
            target_identity
                .change_time_nanoseconds
                .try_into()
                .expect("the captured change-time fraction is valid"),
        );
    let execution_window = FilesystemExecutionTimeWindow {
        started: recorded,
        finished: recorded,
    };
    let mut unchanged_expected = expected.clone();

    assert!(admit_filesystem_identity_path(
        &actual,
        &mut unchanged_expected,
        &target,
        Some(execution_window),
    ));

    let changed_group_id = target_identity.group_id.wrapping_add(1);
    actual
        .get_mut(&target)
        .expect("the seeded branch has a filesystem identity")
        .group_id = changed_group_id;

    assert!(!admit_filesystem_identity_path(
        &actual,
        &mut expected,
        &target,
        Some(execution_window),
    ));
    Ok(())
}

#[test]
fn forced_git_branch_switch_verifier_rejects_rewriting_the_base_branch() -> EvalResult {
    let suite = FamilySuite::git()?;
    let case = GIT_CASES
        .iter()
        .find(|case| case.name == GIT_BRANCH_SWITCH_NAME)
        .expect("the Git branch-switch fixture exists");
    let repository = Repository::open(suite.workspace.path())?;
    let target = repository
        .find_branch("switch-target", BranchType::Local)?
        .into_reference()
        .peel_to_commit()?;
    repository.set_head("refs/heads/switch-target")?;
    repository.checkout_head(None)?;
    repository.reference(
        &format!("refs/heads/{GIT_BASE_BRANCH}"),
        target.id(),
        true,
        GIT_RESTORE_BRANCH_REFLOG_MESSAGE,
    )?;
    let result = serde_json::json!({
        "branch": "switch-target",
        "head": target.id().to_string(),
        EVAL_RECEIPT_FIELD: SYNTHETIC_EVAL_RECEIPT,
    })
    .to_string();

    assert!(!suite.forced_case_result_passed(case, &result)?);
    Ok(())
}

#[test]
fn forced_git_branch_switch_verifier_rejects_an_extra_branch() -> EvalResult {
    let suite = FamilySuite::git()?;
    let case = GIT_CASES
        .iter()
        .find(|case| case.name == GIT_BRANCH_SWITCH_NAME)
        .expect("the Git branch-switch fixture exists");
    let repository = Repository::open(suite.workspace.path())?;
    let target = repository
        .find_branch("switch-target", BranchType::Local)?
        .into_reference()
        .peel_to_commit()?;
    repository.set_head("refs/heads/switch-target")?;
    repository.checkout_head(None)?;
    repository.branch("collateral-branch", &target, false)?;
    let result = serde_json::json!({
        "branch": "switch-target",
        "head": target.id().to_string(),
        EVAL_RECEIPT_FIELD: SYNTHETIC_EVAL_RECEIPT,
    })
    .to_string();

    assert!(!suite.forced_case_result_passed(case, &result)?);
    Ok(())
}

#[test]
fn forced_git_diff_verifier_accepts_the_seeded_worktree_patch() -> EvalResult {
    let suite = FamilySuite::git()?;
    let case = GIT_CASES
        .iter()
        .find(|case| case.name == GIT_DIFF_NAME)
        .expect("the Git diff fixture exists");
    suite.prepare_git_case(GIT_DIFF_NAME)?;
    let repository = Repository::open(suite.workspace.path())?;
    let result = serde_json::json!({
        "patch": expected_bounded_git_worktree_patch(suite.workspace.path())?,
        "truncated": true,
        EVAL_RECEIPT_FIELD: SYNTHETIC_EVAL_RECEIPT,
    })
    .to_string();

    assert_eq!(
        repository.status_file(Path::new(GIT_STAGE_PATH))?,
        Status::INDEX_NEW
    );
    assert!(suite.forced_case_result_passed(case, &result)?);
    Ok(())
}

#[cfg(unix)]
#[test]
fn forced_git_diff_verifier_rejects_post_seed_fixture_mode_drift() -> EvalResult {
    let suite = FamilySuite::git()?;
    let case = GIT_CASES
        .iter()
        .find(|case| case.name == GIT_DIFF_NAME)
        .expect("the Git diff fixture exists");
    suite.prepare_git_case(GIT_DIFF_NAME)?;
    let path = suite.workspace.path().join(GIT_DIFF_OVERFLOW_PATH);
    let mut permissions = fs::metadata(&path)?.permissions();
    permissions.set_mode(permissions.mode() ^ USER_EXECUTE_MODE_BIT);
    fs::set_permissions(&path, permissions)?;
    let result = serde_json::json!({
        "patch": expected_bounded_git_worktree_patch(suite.workspace.path())?,
        "truncated": true,
        EVAL_RECEIPT_FIELD: SYNTHETIC_EVAL_RECEIPT,
    })
    .to_string();

    assert!(!suite.forced_case_result_passed(case, &result)?);
    Ok(())
}

#[test]
fn forced_git_diff_verifier_rejects_an_empty_patch() -> EvalResult {
    let suite = FamilySuite::git()?;
    let case = GIT_CASES
        .iter()
        .find(|case| case.name == GIT_DIFF_NAME)
        .expect("the Git diff fixture exists");
    suite.prepare_git_case(GIT_DIFF_NAME)?;
    let result = serde_json::json!({
        "patch": "",
        "truncated": true,
        EVAL_RECEIPT_FIELD: SYNTHETIC_EVAL_RECEIPT,
    })
    .to_string();

    assert!(!suite.forced_case_result_passed(case, &result)?);
    Ok(())
}

#[test]
fn forced_git_diff_verifier_rejects_an_unstaged_fixture() -> EvalResult {
    let suite = FamilySuite::git()?;
    let case = GIT_CASES
        .iter()
        .find(|case| case.name == GIT_DIFF_NAME)
        .expect("the Git diff fixture exists");
    suite.prepare_git_case(GIT_DIFF_NAME)?;
    let result = serde_json::json!({
        "patch": expected_bounded_git_worktree_patch(suite.workspace.path())?,
        "truncated": true,
        EVAL_RECEIPT_FIELD: SYNTHETIC_EVAL_RECEIPT,
    })
    .to_string();
    let mut index = Repository::open(suite.workspace.path())?.index()?;
    index.remove_path(Path::new(GIT_STAGE_PATH))?;
    index.write()?;

    assert!(!suite.forced_case_result_passed(case, &result)?);
    Ok(())
}

#[test]
fn forced_git_diff_verifier_rejects_a_staged_overflow_fixture() -> EvalResult {
    let suite = FamilySuite::git()?;
    let case = GIT_CASES
        .iter()
        .find(|case| case.name == GIT_DIFF_NAME)
        .expect("the Git diff fixture exists");
    suite.prepare_git_case(GIT_DIFF_NAME)?;
    let result = serde_json::json!({
        "patch": expected_bounded_git_worktree_patch(suite.workspace.path())?,
        "truncated": true,
        EVAL_RECEIPT_FIELD: SYNTHETIC_EVAL_RECEIPT,
    })
    .to_string();
    stage_path(suite.workspace.path(), GIT_DIFF_OVERFLOW_PATH)?;

    assert!(!suite.forced_case_result_passed(case, &result)?);
    Ok(())
}

#[test]
fn forced_git_diff_verifier_rejects_an_unbounded_patch() -> EvalResult {
    let suite = FamilySuite::git()?;
    let case = GIT_CASES
        .iter()
        .find(|case| case.name == GIT_DIFF_NAME)
        .expect("the Git diff fixture exists");
    suite.prepare_git_case(GIT_DIFF_NAME)?;
    let result = serde_json::json!({
        "patch": expected_git_worktree_patch(suite.workspace.path())?,
        "truncated": false,
        EVAL_RECEIPT_FIELD: SYNTHETIC_EVAL_RECEIPT,
    })
    .to_string();

    assert!(!suite.forced_case_result_passed(case, &result)?);
    Ok(())
}

#[test]
fn forced_git_log_verifier_rejects_the_default_head() -> EvalResult {
    let suite = FamilySuite::git()?;
    let case = GIT_CASES
        .iter()
        .find(|case| case.name == GIT_LOG_NAME)
        .expect("the Git log fixture exists");
    let head = Repository::open(suite.workspace.path())?
        .head()?
        .peel_to_commit()?
        .id()
        .to_string();
    let result = serde_json::json!({
        "commits": [{"commit": head}],
        "truncated": true,
        EVAL_RECEIPT_FIELD: SYNTHETIC_EVAL_RECEIPT,
    })
    .to_string();

    assert!(!suite.forced_case_result_passed(case, &result)?);
    Ok(())
}

#[test]
fn forced_git_log_verifier_accepts_the_bounded_target() -> EvalResult {
    let suite = FamilySuite::git()?;
    suite.prepare_git_case(GIT_LOG_NAME)?;
    let case = GIT_CASES
        .iter()
        .find(|case| case.name == GIT_LOG_NAME)
        .expect("the Git log fixture exists");
    let repository = Repository::open(suite.workspace.path())?;
    let target = repository
        .find_branch("log-target", BranchType::Local)?
        .into_reference()
        .peel_to_commit()?;
    let result = serde_json::json!({
        "commits": [{
            "commit": target.id().to_string(),
            "author_name": target.author().name().unwrap_or_default(),
            "author_name_truncated": false,
            "author_email": target.author().email().unwrap_or_default(),
            "author_email_truncated": false,
            "message": target.message().unwrap_or_default(),
            "message_truncated": false,
        }],
        "truncated": true,
        EVAL_RECEIPT_FIELD: SYNTHETIC_EVAL_RECEIPT,
    })
    .to_string();

    assert!(suite.forced_case_result_passed(case, &result)?);
    Ok(())
}

#[test]
fn forced_git_log_verifier_rejects_worktree_mtime_drift() -> EvalResult {
    let suite = FamilySuite::git()?;
    suite.prepare_git_case(GIT_LOG_NAME)?;
    let case = GIT_CASES
        .iter()
        .find(|case| case.name == GIT_LOG_NAME)
        .expect("the Git log fixture exists");
    let repository = Repository::open(suite.workspace.path())?;
    let target = repository
        .find_branch("log-target", BranchType::Local)?
        .into_reference()
        .peel_to_commit()?;
    let collateral = suite.workspace.path().join(GIT_STAGE_PATH);
    fs::File::open(collateral)?.set_times(fs::FileTimes::new().set_modified(UNIX_EPOCH))?;
    let result = serde_json::json!({
        "commits": [{
            "commit": target.id().to_string(),
            "author_name": target.author().name().unwrap_or_default(),
            "author_name_truncated": false,
            "author_email": target.author().email().unwrap_or_default(),
            "author_email_truncated": false,
            "message": target.message().unwrap_or_default(),
            "message_truncated": false,
        }],
        "truncated": true,
        EVAL_RECEIPT_FIELD: SYNTHETIC_EVAL_RECEIPT,
    })
    .to_string();

    assert!(!suite.forced_case_result_passed(case, &result)?);
    Ok(())
}

#[cfg(unix)]
#[test]
fn forced_git_log_verifier_rejects_byte_identical_worktree_replacement() -> EvalResult {
    let suite = FamilySuite::git()?;
    suite.prepare_git_case(GIT_LOG_NAME)?;
    let case = GIT_CASES
        .iter()
        .find(|case| case.name == GIT_LOG_NAME)
        .expect("the Git log fixture exists");
    let repository = Repository::open(suite.workspace.path())?;
    let target_commit = repository
        .find_branch("log-target", BranchType::Local)?
        .into_reference()
        .peel_to_commit()?;
    let target = suite.workspace.path().join(GIT_STAGE_PATH);
    let replacement = suite.workspace.path().join("replacement-fixture");
    let target_modified = *suite
        .git_seed_fixture
        .worktree_modified_times
        .get(Path::new(GIT_STAGE_PATH))
        .expect("the Git fixture has a captured target modified time");
    let root_modified = *suite
        .git_seed_fixture
        .worktree_modified_times
        .get(Path::new(""))
        .expect("the Git fixture has a captured root modified time");
    let permissions = fs::metadata(&target)?.permissions();
    fs::write(&replacement, GIT_STAGE_CONTENT)?;
    fs::set_permissions(&replacement, permissions)?;
    fs::rename(&replacement, &target)?;
    fs::File::open(&target)?.set_times(fs::FileTimes::new().set_modified(target_modified))?;
    fs::File::open(suite.workspace.path())?
        .set_times(fs::FileTimes::new().set_modified(root_modified))?;
    let result = serde_json::json!({
        "commits": [{
            "commit": target_commit.id().to_string(),
            "author_name": target_commit.author().name().unwrap_or_default(),
            "author_name_truncated": false,
            "author_email": target_commit.author().email().unwrap_or_default(),
            "author_email_truncated": false,
            "message": target_commit.message().unwrap_or_default(),
            "message_truncated": false,
        }],
        "truncated": true,
        EVAL_RECEIPT_FIELD: SYNTHETIC_EVAL_RECEIPT,
    })
    .to_string();

    assert_eq!(
        git_worktree_entries(suite.workspace.path())?,
        suite.git_seed_fixture.worktree_entries
    );
    assert_eq!(
        git_worktree_modified_times(suite.workspace.path())?,
        suite.git_seed_fixture.worktree_modified_times
    );
    assert_ne!(
        git_worktree_entry_identities(suite.workspace.path())?,
        suite.git_seed_fixture.worktree_entry_identities
    );
    assert!(!suite.forced_case_result_passed(case, &result)?);
    Ok(())
}

#[cfg(unix)]
#[test]
fn forced_git_log_verifier_rejects_byte_identical_nested_reference_replacement() -> EvalResult {
    let suite = FamilySuite::git()?;
    suite.prepare_git_case(GIT_LOG_NAME)?;
    let case = GIT_CASES
        .iter()
        .find(|case| case.name == GIT_LOG_NAME)
        .expect("the Git log fixture exists");
    let relative = Path::new("heads/log-target");
    let parent = Path::new("heads");
    let repository = Repository::open(suite.workspace.path())?;
    let target = repository.path().join(GIT_REFS_DIRECTORY).join(relative);
    let target_modified = suite.git_seed_fixture.reference_modified_times[relative];
    let parent_modified = suite.git_seed_fixture.reference_modified_times[parent];
    replace_git_metadata_file_byte_identically(&target, target_modified, parent_modified)?;
    let result = forced_git_log_result(&suite)?;

    assert_ne!(
        git_reference_entry_identities(suite.workspace.path())?,
        suite.git_seed_fixture.reference_entry_identities
    );
    assert!(!suite.forced_case_result_passed(case, &result)?);
    Ok(())
}

#[cfg(unix)]
#[test]
fn forced_git_log_verifier_rejects_byte_identical_reflog_replacement() -> EvalResult {
    let suite = FamilySuite::git()?;
    suite.prepare_git_case(GIT_LOG_NAME)?;
    let case = GIT_CASES
        .iter()
        .find(|case| case.name == GIT_LOG_NAME)
        .expect("the Git log fixture exists");
    let relative = Path::new("HEAD");
    let repository = Repository::open(suite.workspace.path())?;
    let target = repository.path().join(GIT_LOGS_DIRECTORY).join(relative);
    let target_modified = suite.git_seed_fixture.reflog_modified_times[relative];
    let parent_modified = suite.git_seed_fixture.reflog_modified_times[Path::new("")];
    replace_git_metadata_file_byte_identically(&target, target_modified, parent_modified)?;
    let result = forced_git_log_result(&suite)?;

    assert_ne!(
        git_reflog_entry_identities(suite.workspace.path())?,
        suite.git_seed_fixture.reflog_entry_identities
    );
    assert!(!suite.forced_case_result_passed(case, &result)?);
    Ok(())
}

#[cfg(unix)]
#[test]
fn forced_git_log_verifier_rejects_byte_identical_loose_object_replacement() -> EvalResult {
    let suite = FamilySuite::git()?;
    suite.prepare_git_case(GIT_LOG_NAME)?;
    let case = GIT_CASES
        .iter()
        .find(|case| case.name == GIT_LOG_NAME)
        .expect("the Git log fixture exists");
    let repository = Repository::open(suite.workspace.path())?;
    let target_commit = repository
        .find_branch("log-target", BranchType::Local)?
        .into_reference()
        .peel_to_commit()?;
    let relative = git_loose_object_relative_path(target_commit.id());
    let parent = relative
        .parent()
        .expect("the loose object fixture has a parent");
    let target = repository
        .path()
        .join(GIT_OBJECTS_DIRECTORY)
        .join(&relative);
    let target_modified = suite.git_seed_fixture.object_modified_times[&relative];
    let parent_modified = suite.git_seed_fixture.object_modified_times[parent];
    replace_git_metadata_file_byte_identically(&target, target_modified, parent_modified)?;
    let result = forced_git_log_result(&suite)?;

    assert_ne!(
        git_object_entry_identities(suite.workspace.path())?,
        suite.git_seed_fixture.object_entry_identities
    );
    assert!(!suite.forced_case_result_passed(case, &result)?);
    Ok(())
}

#[cfg(unix)]
#[test]
fn forced_git_log_verifier_rejects_byte_identical_static_metadata_replacement() -> EvalResult {
    let suite = FamilySuite::git()?;
    suite.prepare_git_case(GIT_LOG_NAME)?;
    let case = GIT_CASES
        .iter()
        .find(|case| case.name == GIT_LOG_NAME)
        .expect("the Git log fixture exists");
    let relative = Path::new(GIT_DESCRIPTION_PATH);
    let repository = Repository::open(suite.workspace.path())?;
    let target = repository.path().join(relative);
    let target_modified = suite.git_seed_fixture.static_metadata_modified_times[relative];
    let parent_modified = suite
        .git_seed_fixture
        .metadata_root_modified_time
        .expect("the Git fixture has a metadata-root modified time");
    replace_git_metadata_file_byte_identically(&target, target_modified, parent_modified)?;
    let result = forced_git_log_result(&suite)?;

    assert_ne!(
        git_static_metadata_entry_identities(suite.workspace.path())?,
        suite.git_seed_fixture.static_metadata_entry_identities
    );
    assert!(!suite.forced_case_result_passed(case, &result)?);
    Ok(())
}

#[test]
fn forced_git_log_verifier_rejects_content_in_a_seeded_metadata_directory() -> EvalResult {
    let suite = FamilySuite::git()?;
    suite.prepare_git_case(GIT_LOG_NAME)?;
    let case = GIT_CASES
        .iter()
        .find(|case| case.name == GIT_LOG_NAME)
        .expect("the Git log fixture exists");
    let repository = Repository::open(suite.workspace.path())?;
    let branches = repository.path().join(GIT_BRANCHES_DIRECTORY);
    let modified =
        suite.git_seed_fixture.static_metadata_modified_times[Path::new(GIT_BRANCHES_DIRECTORY)];
    fs::write(branches.join("collateral"), "synthetic metadata fixture\n")?;
    fs::File::open(branches)?.set_times(fs::FileTimes::new().set_modified(modified))?;
    let result = forced_git_log_result(&suite)?;

    assert!(!suite.forced_case_result_passed(case, &result)?);
    Ok(())
}

#[test]
fn forced_git_log_verifier_rejects_metadata_root_mtime_drift() -> EvalResult {
    let suite = FamilySuite::git()?;
    suite.prepare_git_case(GIT_LOG_NAME)?;
    let case = GIT_CASES
        .iter()
        .find(|case| case.name == GIT_LOG_NAME)
        .expect("the Git log fixture exists");
    let repository = Repository::open(suite.workspace.path())?;
    let target = repository
        .find_branch("log-target", BranchType::Local)?
        .into_reference()
        .peel_to_commit()?;
    fs::File::open(repository.path())?.set_times(fs::FileTimes::new().set_modified(UNIX_EPOCH))?;
    let result = serde_json::json!({
        "commits": [{
            "commit": target.id().to_string(),
            "author_name": target.author().name().unwrap_or_default(),
            "author_name_truncated": false,
            "author_email": target.author().email().unwrap_or_default(),
            "author_email_truncated": false,
            "message": target.message().unwrap_or_default(),
            "message_truncated": false,
        }],
        "truncated": true,
        EVAL_RECEIPT_FIELD: SYNTHETIC_EVAL_RECEIPT,
    })
    .to_string();

    assert!(!suite.forced_case_result_passed(case, &result)?);
    Ok(())
}

#[test]
fn forced_git_log_verifier_rejects_loose_object_mtime_drift() -> EvalResult {
    let suite = FamilySuite::git()?;
    suite.prepare_git_case(GIT_LOG_NAME)?;
    let case = GIT_CASES
        .iter()
        .find(|case| case.name == GIT_LOG_NAME)
        .expect("the Git log fixture exists");
    let repository = Repository::open(suite.workspace.path())?;
    let target = repository
        .find_branch("log-target", BranchType::Local)?
        .into_reference()
        .peel_to_commit()?;
    let object_path = repository
        .path()
        .join(GIT_OBJECTS_DIRECTORY)
        .join(git_loose_object_relative_path(target.id()));
    fs::File::open(object_path)?.set_times(fs::FileTimes::new().set_modified(UNIX_EPOCH))?;
    let result = serde_json::json!({
        "commits": [{
            "commit": target.id().to_string(),
            "author_name": target.author().name().unwrap_or_default(),
            "author_name_truncated": false,
            "author_email": target.author().email().unwrap_or_default(),
            "author_email_truncated": false,
            "message": target.message().unwrap_or_default(),
            "message_truncated": false,
        }],
        "truncated": true,
        EVAL_RECEIPT_FIELD: SYNTHETIC_EVAL_RECEIPT,
    })
    .to_string();

    assert!(!suite.forced_case_result_passed(case, &result)?);
    Ok(())
}

#[test]
fn forced_git_log_verifier_rejects_top_level_metadata_byte_drift() -> EvalResult {
    let suite = FamilySuite::git()?;
    suite.prepare_git_case(GIT_LOG_NAME)?;
    let case = GIT_CASES
        .iter()
        .find(|case| case.name == GIT_LOG_NAME)
        .expect("the Git log fixture exists");
    let repository = Repository::open(suite.workspace.path())?;
    let target = repository
        .find_branch("log-target", BranchType::Local)?
        .into_reference()
        .peel_to_commit()?;
    fs::write(
        repository.path().join(GIT_HEAD_PATH),
        format!("ref: refs/heads/{GIT_BASE_BRANCH}"),
    )?;
    let result = serde_json::json!({
        "commits": [{
            "commit": target.id().to_string(),
            "author_name": target.author().name().unwrap_or_default(),
            "author_name_truncated": false,
            "author_email": target.author().email().unwrap_or_default(),
            "author_email_truncated": false,
            "message": target.message().unwrap_or_default(),
            "message_truncated": false,
        }],
        "truncated": true,
        EVAL_RECEIPT_FIELD: SYNTHETIC_EVAL_RECEIPT,
    })
    .to_string();

    assert!(!suite.forced_case_result_passed(case, &result)?);
    Ok(())
}

#[cfg(unix)]
#[test]
fn forced_git_log_verifier_rejects_byte_identical_metadata_file_replacement() -> EvalResult {
    let suite = FamilySuite::git()?;
    suite.prepare_git_case(GIT_LOG_NAME)?;
    let case = GIT_CASES
        .iter()
        .find(|case| case.name == GIT_LOG_NAME)
        .expect("the Git log fixture exists");
    let repository = Repository::open(suite.workspace.path())?;
    let target_commit = repository
        .find_branch("log-target", BranchType::Local)?
        .into_reference()
        .peel_to_commit()?;
    let target = repository.path().join(GIT_CONFIG_PATH);
    let replacement = repository.path().join("config-replacement-fixture");
    let expected_config = suite
        .git_seed_fixture
        .metadata_top_level
        .get(Path::new(GIT_CONFIG_PATH))
        .expect("the Git fixture has a captured config entry");
    let target_modified = expected_config
        .modified
        .expect("the Git fixture has a captured config modified time");
    let root_modified = suite
        .git_seed_fixture
        .metadata_root_modified_time
        .expect("the Git fixture has a captured metadata-root modified time");
    let permissions = fs::metadata(&target)?.permissions();
    fs::write(&replacement, &suite.git_seed_fixture.config)?;
    fs::set_permissions(&replacement, permissions)?;
    fs::rename(&replacement, &target)?;
    fs::File::open(&target)?.set_times(fs::FileTimes::new().set_modified(target_modified))?;
    fs::File::open(repository.path())?
        .set_times(fs::FileTimes::new().set_modified(root_modified))?;
    let actual_metadata = git_metadata_top_level(suite.workspace.path())?;
    let actual_config = actual_metadata
        .get(Path::new(GIT_CONFIG_PATH))
        .expect("the replaced config remains in the metadata inventory");
    let result = serde_json::json!({
        "commits": [{
            "commit": target_commit.id().to_string(),
            "author_name": target_commit.author().name().unwrap_or_default(),
            "author_name_truncated": false,
            "author_email": target_commit.author().email().unwrap_or_default(),
            "author_email_truncated": false,
            "message": target_commit.message().unwrap_or_default(),
            "message_truncated": false,
        }],
        "truncated": true,
        EVAL_RECEIPT_FIELD: SYNTHETIC_EVAL_RECEIPT,
    })
    .to_string();

    assert_eq!(actual_config.content, expected_config.content);
    assert_eq!(actual_config.modified, expected_config.modified);
    assert_ne!(actual_config.identity, expected_config.identity);
    assert!(!suite.forced_case_result_passed(case, &result)?);
    Ok(())
}

#[test]
fn forced_git_log_verifier_rejects_top_level_metadata_mtime_drift() -> EvalResult {
    let suite = FamilySuite::git()?;
    suite.prepare_git_case(GIT_LOG_NAME)?;
    let case = GIT_CASES
        .iter()
        .find(|case| case.name == GIT_LOG_NAME)
        .expect("the Git log fixture exists");
    let repository = Repository::open(suite.workspace.path())?;
    let target = repository
        .find_branch("log-target", BranchType::Local)?
        .into_reference()
        .peel_to_commit()?;
    fs::File::open(repository.path().join(GIT_CONFIG_PATH))?
        .set_times(fs::FileTimes::new().set_modified(UNIX_EPOCH))?;
    let result = serde_json::json!({
        "commits": [{
            "commit": target.id().to_string(),
            "author_name": target.author().name().unwrap_or_default(),
            "author_name_truncated": false,
            "author_email": target.author().email().unwrap_or_default(),
            "author_email_truncated": false,
            "message": target.message().unwrap_or_default(),
            "message_truncated": false,
        }],
        "truncated": true,
        EVAL_RECEIPT_FIELD: SYNTHETIC_EVAL_RECEIPT,
    })
    .to_string();

    assert!(!suite.forced_case_result_passed(case, &result)?);
    Ok(())
}

#[test]
fn forced_git_log_verifier_rejects_metadata_directory_mtime_drift() -> EvalResult {
    let suite = FamilySuite::git()?;
    suite.prepare_git_case(GIT_LOG_NAME)?;
    let case = GIT_CASES
        .iter()
        .find(|case| case.name == GIT_LOG_NAME)
        .expect("the Git log fixture exists");
    let repository = Repository::open(suite.workspace.path())?;
    let target = repository
        .find_branch("log-target", BranchType::Local)?
        .into_reference()
        .peel_to_commit()?;
    fs::File::open(repository.path().join(GIT_HOOKS_DIRECTORY))?
        .set_times(fs::FileTimes::new().set_modified(UNIX_EPOCH))?;
    let result = serde_json::json!({
        "commits": [{
            "commit": target.id().to_string(),
            "author_name": target.author().name().unwrap_or_default(),
            "author_name_truncated": false,
            "author_email": target.author().email().unwrap_or_default(),
            "author_email_truncated": false,
            "message": target.message().unwrap_or_default(),
            "message_truncated": false,
        }],
        "truncated": true,
        EVAL_RECEIPT_FIELD: SYNTHETIC_EVAL_RECEIPT,
    })
    .to_string();

    assert!(!suite.forced_case_result_passed(case, &result)?);
    Ok(())
}

#[test]
fn forced_git_log_verifier_rejects_static_metadata_file_mtime_drift() -> EvalResult {
    let suite = FamilySuite::git()?;
    suite.prepare_git_case(GIT_LOG_NAME)?;
    let case = GIT_CASES
        .iter()
        .find(|case| case.name == GIT_LOG_NAME)
        .expect("the Git log fixture exists");
    let repository = Repository::open(suite.workspace.path())?;
    let target = repository
        .find_branch("log-target", BranchType::Local)?
        .into_reference()
        .peel_to_commit()?;
    fs::File::open(repository.path().join(GIT_DESCRIPTION_PATH))?
        .set_times(fs::FileTimes::new().set_modified(UNIX_EPOCH))?;
    let result = serde_json::json!({
        "commits": [{
            "commit": target.id().to_string(),
            "author_name": target.author().name().unwrap_or_default(),
            "author_name_truncated": false,
            "author_email": target.author().email().unwrap_or_default(),
            "author_email_truncated": false,
            "message": target.message().unwrap_or_default(),
            "message_truncated": false,
        }],
        "truncated": true,
        EVAL_RECEIPT_FIELD: SYNTHETIC_EVAL_RECEIPT,
    })
    .to_string();

    assert!(!suite.forced_case_result_passed(case, &result)?);
    Ok(())
}

#[test]
fn forced_git_log_verifier_rejects_nested_reference_mtime_drift() -> EvalResult {
    let suite = FamilySuite::git()?;
    suite.prepare_git_case(GIT_LOG_NAME)?;
    let case = GIT_CASES
        .iter()
        .find(|case| case.name == GIT_LOG_NAME)
        .expect("the Git log fixture exists");
    let repository = Repository::open(suite.workspace.path())?;
    let target = repository
        .find_branch("log-target", BranchType::Local)?
        .into_reference()
        .peel_to_commit()?;
    fs::File::open(
        repository
            .path()
            .join(GIT_REFS_DIRECTORY)
            .join("heads")
            .join("log-target"),
    )?
    .set_times(fs::FileTimes::new().set_modified(UNIX_EPOCH))?;
    let result = serde_json::json!({
        "commits": [{
            "commit": target.id().to_string(),
            "author_name": target.author().name().unwrap_or_default(),
            "author_name_truncated": false,
            "author_email": target.author().email().unwrap_or_default(),
            "author_email_truncated": false,
            "message": target.message().unwrap_or_default(),
            "message_truncated": false,
        }],
        "truncated": true,
        EVAL_RECEIPT_FIELD: SYNTHETIC_EVAL_RECEIPT,
    })
    .to_string();

    assert!(!suite.forced_case_result_passed(case, &result)?);
    Ok(())
}

#[test]
fn forced_git_log_verifier_rejects_nested_reference_directory_mtime_drift() -> EvalResult {
    let suite = FamilySuite::git()?;
    suite.prepare_git_case(GIT_LOG_NAME)?;
    let case = GIT_CASES
        .iter()
        .find(|case| case.name == GIT_LOG_NAME)
        .expect("the Git log fixture exists");
    let repository = Repository::open(suite.workspace.path())?;
    let target = repository
        .find_branch("log-target", BranchType::Local)?
        .into_reference()
        .peel_to_commit()?;
    fs::File::open(repository.path().join(GIT_REFS_DIRECTORY).join("heads"))?
        .set_times(fs::FileTimes::new().set_modified(UNIX_EPOCH))?;
    let result = serde_json::json!({
        "commits": [{
            "commit": target.id().to_string(),
            "author_name": target.author().name().unwrap_or_default(),
            "author_name_truncated": false,
            "author_email": target.author().email().unwrap_or_default(),
            "author_email_truncated": false,
            "message": target.message().unwrap_or_default(),
            "message_truncated": false,
        }],
        "truncated": true,
        EVAL_RECEIPT_FIELD: SYNTHETIC_EVAL_RECEIPT,
    })
    .to_string();

    assert!(!suite.forced_case_result_passed(case, &result)?);
    Ok(())
}

#[test]
fn forced_git_log_verifier_rejects_reflog_file_mtime_drift() -> EvalResult {
    let suite = FamilySuite::git()?;
    suite.prepare_git_case(GIT_LOG_NAME)?;
    let case = GIT_CASES
        .iter()
        .find(|case| case.name == GIT_LOG_NAME)
        .expect("the Git log fixture exists");
    let repository = Repository::open(suite.workspace.path())?;
    let target = repository
        .find_branch("log-target", BranchType::Local)?
        .into_reference()
        .peel_to_commit()?;
    fs::File::open(repository.path().join(GIT_LOGS_DIRECTORY).join("HEAD"))?
        .set_times(fs::FileTimes::new().set_modified(UNIX_EPOCH))?;
    let result = serde_json::json!({
        "commits": [{
            "commit": target.id().to_string(),
            "author_name": target.author().name().unwrap_or_default(),
            "author_name_truncated": false,
            "author_email": target.author().email().unwrap_or_default(),
            "author_email_truncated": false,
            "message": target.message().unwrap_or_default(),
            "message_truncated": false,
        }],
        "truncated": true,
        EVAL_RECEIPT_FIELD: SYNTHETIC_EVAL_RECEIPT,
    })
    .to_string();

    assert!(!suite.forced_case_result_passed(case, &result)?);
    Ok(())
}

#[test]
fn forced_git_log_verifier_rejects_reflog_drift() -> EvalResult {
    let suite = FamilySuite::git()?;
    let case = GIT_CASES
        .iter()
        .find(|case| case.name == GIT_LOG_NAME)
        .expect("the Git log fixture exists");
    let repository = Repository::open(suite.workspace.path())?;
    let target = repository
        .find_branch("log-target", BranchType::Local)?
        .into_reference()
        .peel_to_commit()?;
    let head_log = repository.path().join(GIT_LOGS_DIRECTORY).join("HEAD");
    let mut contents = fs::read(&head_log)?;
    contents.extend_from_slice(b"synthetic collateral reflog record\n");
    fs::write(head_log, contents)?;
    let result = serde_json::json!({
        "commits": [{
            "commit": target.id().to_string(),
            "author_name": target.author().name().unwrap_or_default(),
            "author_name_truncated": false,
            "author_email": target.author().email().unwrap_or_default(),
            "author_email_truncated": false,
            "message": target.message().unwrap_or_default(),
            "message_truncated": false,
        }],
        "truncated": true,
        EVAL_RECEIPT_FIELD: SYNTHETIC_EVAL_RECEIPT,
    })
    .to_string();

    assert!(!suite.forced_case_result_passed(case, &result)?);
    Ok(())
}

#[test]
fn forced_git_log_verifier_rejects_a_collateral_empty_directory() -> EvalResult {
    let suite = FamilySuite::git()?;
    let case = GIT_CASES
        .iter()
        .find(|case| case.name == GIT_LOG_NAME)
        .expect("the Git log fixture exists");
    let repository = Repository::open(suite.workspace.path())?;
    let target = repository
        .find_branch("log-target", BranchType::Local)?
        .into_reference()
        .peel_to_commit()?;
    let result = serde_json::json!({
        "commits": [{
            "commit": target.id().to_string(),
            "author_name": target.author().name().unwrap_or_default(),
            "author_name_truncated": false,
            "author_email": target.author().email().unwrap_or_default(),
            "author_email_truncated": false,
            "message": target.message().unwrap_or_default(),
            "message_truncated": false,
        }],
        "truncated": true,
        EVAL_RECEIPT_FIELD: SYNTHETIC_EVAL_RECEIPT,
    })
    .to_string();
    fs::create_dir(suite.workspace.path().join(GIT_COLLATERAL_DIRECTORY))?;

    assert!(!suite.forced_case_result_passed(case, &result)?);
    Ok(())
}

#[cfg(unix)]
#[test]
fn forced_git_log_verifier_rejects_a_collateral_pre_commit_hook() -> EvalResult {
    let suite = FamilySuite::git()?;
    let case = GIT_CASES
        .iter()
        .find(|case| case.name == GIT_LOG_NAME)
        .expect("the Git log fixture exists");
    let repository = Repository::open(suite.workspace.path())?;
    let target = repository
        .find_branch("log-target", BranchType::Local)?
        .into_reference()
        .peel_to_commit()?;
    let result = serde_json::json!({
        "commits": [{
            "commit": target.id().to_string(),
            "author_name": target.author().name().unwrap_or_default(),
            "author_name_truncated": false,
            "author_email": target.author().email().unwrap_or_default(),
            "author_email_truncated": false,
            "message": target.message().unwrap_or_default(),
            "message_truncated": false,
        }],
        "truncated": true,
        EVAL_RECEIPT_FIELD: SYNTHETIC_EVAL_RECEIPT,
    })
    .to_string();
    let hook = repository.path().join(GIT_PRE_COMMIT_HOOK_PATH);
    fs::write(&hook, GIT_PRE_COMMIT_HOOK_CONTENT)?;
    let mut permissions = fs::metadata(&hook)?.permissions();
    permissions.set_mode(permissions.mode() | USER_EXECUTE_MODE_BIT);
    fs::set_permissions(&hook, permissions)?;

    assert!(!suite.forced_case_result_passed(case, &result)?);
    Ok(())
}

#[test]
fn forced_git_log_verifier_rejects_a_moved_target_reference() -> EvalResult {
    let suite = FamilySuite::git()?;
    let case = GIT_CASES
        .iter()
        .find(|case| case.name == GIT_LOG_NAME)
        .expect("the Git log fixture exists");
    let repository = Repository::open(suite.workspace.path())?;
    let target = repository
        .find_branch("log-target", BranchType::Local)?
        .into_reference()
        .peel_to_commit()?;
    let result = serde_json::json!({
        "commits": [{
            "commit": target.id().to_string(),
            "author_name": target.author().name().unwrap_or_default(),
            "author_name_truncated": false,
            "author_email": target.author().email().unwrap_or_default(),
            "author_email_truncated": false,
            "message": target.message().unwrap_or_default(),
            "message_truncated": false,
        }],
        "truncated": true,
        EVAL_RECEIPT_FIELD: SYNTHETIC_EVAL_RECEIPT,
    })
    .to_string();
    repository
        .find_reference("refs/heads/log-target")?
        .set_target(
            suite
                .git_seed
                .expect("the Git eval suite has a captured seed identity"),
            "synthetic moved log target",
        )?;

    assert!(!suite.forced_case_result_passed(case, &result)?);
    Ok(())
}

#[test]
fn forced_git_log_verifier_rejects_a_moved_non_target_reference() -> EvalResult {
    let suite = FamilySuite::git()?;
    let case = GIT_CASES
        .iter()
        .find(|case| case.name == GIT_LOG_NAME)
        .expect("the Git log fixture exists");
    let repository = Repository::open(suite.workspace.path())?;
    let target = repository
        .find_branch("log-target", BranchType::Local)?
        .into_reference()
        .peel_to_commit()?;
    let result = serde_json::json!({
        "commits": [{
            "commit": target.id().to_string(),
            "author_name": target.author().name().unwrap_or_default(),
            "author_name_truncated": false,
            "author_email": target.author().email().unwrap_or_default(),
            "author_email_truncated": false,
            "message": target.message().unwrap_or_default(),
            "message_truncated": false,
        }],
        "truncated": true,
        EVAL_RECEIPT_FIELD: SYNTHETIC_EVAL_RECEIPT,
    })
    .to_string();
    repository
        .find_reference("refs/heads/switch-target")?
        .set_target(
            suite
                .git_seed
                .expect("the Git eval suite has a captured seed identity"),
            "synthetic moved non-target branch",
        )?;

    assert!(!suite.forced_case_result_passed(case, &result)?);
    Ok(())
}

#[test]
fn forced_git_log_verifier_rejects_more_than_the_requested_limit() -> EvalResult {
    let suite = FamilySuite::git()?;
    let case = GIT_CASES
        .iter()
        .find(|case| case.name == GIT_LOG_NAME)
        .expect("the Git log fixture exists");
    let repository = Repository::open(suite.workspace.path())?;
    let target = repository
        .find_branch("log-target", BranchType::Local)?
        .into_reference()
        .peel_to_commit()?;
    let parent = target.parent(0)?;
    let result = serde_json::json!({
        "commits": [
            {"commit": target.id().to_string()},
            {"commit": parent.id().to_string()},
        ],
        "truncated": true,
        EVAL_RECEIPT_FIELD: SYNTHETIC_EVAL_RECEIPT,
    })
    .to_string();

    assert!(!suite.forced_case_result_passed(case, &result)?);
    Ok(())
}

#[test]
fn forced_git_status_verifier_accepts_the_bounded_prefix() -> EvalResult {
    let suite = FamilySuite::git()?;
    suite.prepare_git_case(GIT_STATUS_NAME)?;
    let case = GIT_CASES
        .iter()
        .find(|case| case.name == GIT_STATUS_NAME)
        .expect("the Git status fixture exists");
    let seed = suite
        .git_seed
        .expect("the Git eval suite has a captured seed identity");
    let result = serde_json::json!({
        "branch": GIT_BASE_BRANCH,
        "branch_truncated": false,
        "head": seed.to_string(),
        "entries": git_status_entries_json(),
        "truncated": true,
        EVAL_RECEIPT_FIELD: SYNTHETIC_EVAL_RECEIPT,
    })
    .to_string();

    assert!(suite.forced_case_result_passed(case, &result)?);
    Ok(())
}

#[test]
fn forced_git_status_verifier_rejects_a_collateral_object() -> EvalResult {
    let suite = FamilySuite::git()?;
    suite.prepare_git_case(GIT_STATUS_NAME)?;
    let case = GIT_CASES
        .iter()
        .find(|case| case.name == GIT_STATUS_NAME)
        .expect("the Git status fixture exists");
    let seed = suite
        .git_seed
        .expect("the Git eval suite has a captured seed identity");
    Repository::open(suite.workspace.path())?.blob(GIT_COLLATERAL_OBJECT_CONTENT)?;
    let result = serde_json::json!({
        "branch": GIT_BASE_BRANCH,
        "branch_truncated": false,
        "head": seed.to_string(),
        "entries": git_status_entries_json(),
        "truncated": true,
        EVAL_RECEIPT_FIELD: SYNTHETIC_EVAL_RECEIPT,
    })
    .to_string();

    assert!(!suite.forced_case_result_passed(case, &result)?);
    Ok(())
}

#[cfg(unix)]
#[test]
fn forced_git_status_verifier_rejects_seed_object_mode_drift() -> EvalResult {
    let suite = FamilySuite::git()?;
    suite.prepare_git_case(GIT_STATUS_NAME)?;
    let case = GIT_CASES
        .iter()
        .find(|case| case.name == GIT_STATUS_NAME)
        .expect("the Git status fixture exists");
    let seed = suite
        .git_seed
        .expect("the Git eval suite has a captured seed identity");
    let object_id = Oid::hash_object(ObjectType::Blob, GIT_BASE_CONTENT.as_bytes())?;
    let object_path = Repository::open(suite.workspace.path())?
        .path()
        .join(GIT_OBJECTS_DIRECTORY)
        .join(git_loose_object_relative_path(object_id));
    let mut permissions = fs::metadata(&object_path)?.permissions();
    permissions.set_mode(permissions.mode() ^ GROUP_WRITE_MODE_BIT);
    fs::set_permissions(object_path, permissions)?;
    let result = serde_json::json!({
        "branch": GIT_BASE_BRANCH,
        "branch_truncated": false,
        "head": seed.to_string(),
        "entries": git_status_entries_json(),
        "truncated": true,
        EVAL_RECEIPT_FIELD: SYNTHETIC_EVAL_RECEIPT,
    })
    .to_string();

    assert!(!suite.forced_case_result_passed(case, &result)?);
    Ok(())
}

#[cfg(unix)]
#[test]
fn forced_git_status_verifier_rejects_seed_reference_mode_drift() -> EvalResult {
    let suite = FamilySuite::git()?;
    suite.prepare_git_case(GIT_STATUS_NAME)?;
    let case = GIT_CASES
        .iter()
        .find(|case| case.name == GIT_STATUS_NAME)
        .expect("the Git status fixture exists");
    let seed = suite
        .git_seed
        .expect("the Git eval suite has a captured seed identity");
    let reference_path = Repository::open(suite.workspace.path())?
        .path()
        .join(GIT_REFS_DIRECTORY)
        .join("heads")
        .join(GIT_BASE_BRANCH);
    let mut permissions = fs::metadata(&reference_path)?.permissions();
    permissions.set_mode(permissions.mode() ^ GROUP_WRITE_MODE_BIT);
    fs::set_permissions(reference_path, permissions)?;
    let result = serde_json::json!({
        "branch": GIT_BASE_BRANCH,
        "branch_truncated": false,
        "head": seed.to_string(),
        "entries": git_status_entries_json(),
        "truncated": true,
        EVAL_RECEIPT_FIELD: SYNTHETIC_EVAL_RECEIPT,
    })
    .to_string();

    assert!(!suite.forced_case_result_passed(case, &result)?);
    Ok(())
}

#[cfg(unix)]
#[test]
fn forced_git_status_verifier_rejects_corrupted_seed_object_bytes() -> EvalResult {
    let suite = FamilySuite::git()?;
    suite.prepare_git_case(GIT_STATUS_NAME)?;
    let case = GIT_CASES
        .iter()
        .find(|case| case.name == GIT_STATUS_NAME)
        .expect("the Git status fixture exists");
    let seed = suite
        .git_seed
        .expect("the Git eval suite has a captured seed identity");
    let object_id = Oid::hash_object(ObjectType::Blob, GIT_BASE_CONTENT.as_bytes())?;
    let object_path = Repository::open(suite.workspace.path())?
        .path()
        .join(GIT_OBJECTS_DIRECTORY)
        .join(git_loose_object_relative_path(object_id));
    fs::set_permissions(&object_path, fs::Permissions::from_mode(0o600))?;
    fs::write(object_path, b"synthetic corrupt object bytes")?;
    let result = serde_json::json!({
        "branch": GIT_BASE_BRANCH,
        "branch_truncated": false,
        "head": seed.to_string(),
        "entries": git_status_entries_json(),
        "truncated": true,
        EVAL_RECEIPT_FIELD: SYNTHETIC_EVAL_RECEIPT,
    })
    .to_string();

    assert!(!suite.forced_case_result_passed(case, &result)?);
    Ok(())
}

#[cfg(unix)]
#[test]
fn forced_git_status_verifier_rejects_post_seed_fixture_mode_drift() -> EvalResult {
    let suite = FamilySuite::git()?;
    suite.prepare_git_case(GIT_STATUS_NAME)?;
    let case = GIT_CASES
        .iter()
        .find(|case| case.name == GIT_STATUS_NAME)
        .expect("the Git status fixture exists");
    let seed = suite
        .git_seed
        .expect("the Git eval suite has a captured seed identity");
    let path = suite.workspace.path().join(git_status_overflow_path(0));
    let mut permissions = fs::metadata(&path)?.permissions();
    permissions.set_mode(permissions.mode() ^ USER_EXECUTE_MODE_BIT);
    fs::set_permissions(&path, permissions)?;
    let result = serde_json::json!({
        "branch": GIT_BASE_BRANCH,
        "branch_truncated": false,
        "head": seed.to_string(),
        "entries": git_status_entries_json(),
        "truncated": true,
        EVAL_RECEIPT_FIELD: SYNTHETIC_EVAL_RECEIPT,
    })
    .to_string();

    assert!(!suite.forced_case_result_passed(case, &result)?);
    Ok(())
}

#[cfg(unix)]
#[test]
fn forced_git_status_verifier_rejects_metadata_root_mode_drift() -> EvalResult {
    let suite = FamilySuite::git()?;
    suite.prepare_git_case(GIT_STATUS_NAME)?;
    let case = GIT_CASES
        .iter()
        .find(|case| case.name == GIT_STATUS_NAME)
        .expect("the Git status fixture exists");
    let seed = suite
        .git_seed
        .expect("the Git eval suite has a captured seed identity");
    let metadata_root = Repository::open(suite.workspace.path())?
        .path()
        .to_path_buf();
    let mut permissions = fs::metadata(&metadata_root)?.permissions();
    permissions.set_mode(permissions.mode() ^ GROUP_WRITE_MODE_BIT);
    fs::set_permissions(&metadata_root, permissions)?;
    let result = serde_json::json!({
        "branch": GIT_BASE_BRANCH,
        "branch_truncated": false,
        "head": seed.to_string(),
        "entries": git_status_entries_json(),
        "truncated": true,
        EVAL_RECEIPT_FIELD: SYNTHETIC_EVAL_RECEIPT,
    })
    .to_string();

    assert!(!suite.forced_case_result_passed(case, &result)?);
    Ok(())
}

#[cfg(unix)]
#[test]
fn forced_git_status_verifier_rejects_top_level_metadata_file_mode_drift() -> EvalResult {
    let suite = FamilySuite::git()?;
    suite.prepare_git_case(GIT_STATUS_NAME)?;
    let case = GIT_CASES
        .iter()
        .find(|case| case.name == GIT_STATUS_NAME)
        .expect("the Git status fixture exists");
    let seed = suite
        .git_seed
        .expect("the Git eval suite has a captured seed identity");
    let config = Repository::open(suite.workspace.path())?
        .path()
        .join(GIT_CONFIG_PATH);
    let mut permissions = fs::metadata(&config)?.permissions();
    permissions.set_mode(permissions.mode() ^ GROUP_WRITE_MODE_BIT);
    fs::set_permissions(&config, permissions)?;
    let result = serde_json::json!({
        "branch": GIT_BASE_BRANCH,
        "branch_truncated": false,
        "head": seed.to_string(),
        "entries": git_status_entries_json(),
        "truncated": true,
        EVAL_RECEIPT_FIELD: SYNTHETIC_EVAL_RECEIPT,
    })
    .to_string();

    assert!(!suite.forced_case_result_passed(case, &result)?);
    Ok(())
}

#[cfg(unix)]
#[test]
fn forced_git_status_verifier_rejects_a_top_level_metadata_hard_link() -> EvalResult {
    let suite = FamilySuite::git()?;
    suite.prepare_git_case(GIT_STATUS_NAME)?;
    let case = GIT_CASES
        .iter()
        .find(|case| case.name == GIT_STATUS_NAME)
        .expect("the Git status fixture exists");
    let seed = suite
        .git_seed
        .expect("the Git eval suite has a captured seed identity");
    let external = tempfile::tempdir()?;
    let head = Repository::open(suite.workspace.path())?
        .path()
        .join("HEAD");
    let alias = external.path().join("head-alias");
    fs::write(&alias, fs::read(&head)?)?;
    fs::remove_file(&head)?;
    fs::hard_link(&alias, &head)?;
    let result = serde_json::json!({
        "branch": GIT_BASE_BRANCH,
        "branch_truncated": false,
        "head": seed.to_string(),
        "entries": git_status_entries_json(),
        "truncated": true,
        EVAL_RECEIPT_FIELD: SYNTHETIC_EVAL_RECEIPT,
    })
    .to_string();

    assert!(!suite.forced_case_result_passed(case, &result)?);
    Ok(())
}

#[test]
fn forced_git_status_verifier_rejects_an_unknown_result_field() -> EvalResult {
    let suite = FamilySuite::git()?;
    suite.prepare_git_case(GIT_STATUS_NAME)?;
    let case = GIT_CASES
        .iter()
        .find(|case| case.name == GIT_STATUS_NAME)
        .expect("the Git status fixture exists");
    let seed = suite
        .git_seed
        .expect("the Git eval suite has a captured seed identity");
    let result = serde_json::json!({
        "branch": GIT_BASE_BRANCH,
        "branch_truncated": false,
        "head": seed.to_string(),
        "entries": git_status_entries_json(),
        "truncated": true,
        "error": "synthetic contradictory field",
        EVAL_RECEIPT_FIELD: SYNTHETIC_EVAL_RECEIPT,
    })
    .to_string();

    assert!(!suite.forced_case_result_passed(case, &result)?);
    Ok(())
}

#[test]
fn forced_git_status_verifier_rejects_an_unknown_entry_field() -> EvalResult {
    let suite = FamilySuite::git()?;
    suite.prepare_git_case(GIT_STATUS_NAME)?;
    let case = GIT_CASES
        .iter()
        .find(|case| case.name == GIT_STATUS_NAME)
        .expect("the Git status fixture exists");
    let seed = suite
        .git_seed
        .expect("the Git eval suite has a captured seed identity");
    let mut entries = git_status_entries_json();
    entries[0]["error"] = serde_json::json!("synthetic contradictory field");
    let result = serde_json::json!({
        "branch": GIT_BASE_BRANCH,
        "branch_truncated": false,
        "head": seed.to_string(),
        "entries": entries,
        "truncated": true,
        EVAL_RECEIPT_FIELD: SYNTHETIC_EVAL_RECEIPT,
    })
    .to_string();

    assert!(!suite.forced_case_result_passed(case, &result)?);
    Ok(())
}

#[test]
fn forced_git_status_verifier_rejects_skip_worktree_index_drift() -> EvalResult {
    let suite = FamilySuite::git()?;
    suite.prepare_git_case(GIT_STATUS_NAME)?;
    let case = GIT_CASES
        .iter()
        .find(|case| case.name == GIT_STATUS_NAME)
        .expect("the Git status fixture exists");
    let seed = suite
        .git_seed
        .expect("the Git eval suite has a captured seed identity");
    let repository = Repository::open(suite.workspace.path())?;
    let mut index = repository.index()?;
    let mut entry = index
        .get_path(Path::new(GIT_SEED_PATH), 0)
        .expect("the seeded index entry exists");
    entry.flags |= GIT_INDEX_EXTENDED_FLAG;
    entry.flags_extended |= GIT_INDEX_SKIP_WORKTREE_FLAG;
    index.add(&entry)?;
    index.write()?;
    let result = serde_json::json!({
        "branch": GIT_BASE_BRANCH,
        "branch_truncated": false,
        "head": seed.to_string(),
        "entries": git_status_entries_json(),
        "truncated": true,
        EVAL_RECEIPT_FIELD: SYNTHETIC_EVAL_RECEIPT,
    })
    .to_string();

    assert!(!suite.forced_case_result_passed(case, &result)?);
    Ok(())
}

#[test]
fn forced_git_status_verifier_rejects_stat_cache_drift() -> EvalResult {
    let suite = FamilySuite::git()?;
    suite.prepare_git_case(GIT_STATUS_NAME)?;
    let case = GIT_CASES
        .iter()
        .find(|case| case.name == GIT_STATUS_NAME)
        .expect("the Git status fixture exists");
    let seed = suite
        .git_seed
        .expect("the Git eval suite has a captured seed identity");
    let repository = Repository::open(suite.workspace.path())?;
    let mut index = repository.index()?;
    let mut entry = index
        .get_path(Path::new(GIT_SEED_PATH), 0)
        .expect("the seeded index entry exists");
    entry.ctime = IndexTime::new(
        entry.ctime.seconds().wrapping_add(1),
        entry.ctime.nanoseconds(),
    );
    index.add(&entry)?;
    index.write()?;
    let result = serde_json::json!({
        "branch": GIT_BASE_BRANCH,
        "branch_truncated": false,
        "head": seed.to_string(),
        "entries": git_status_entries_json(),
        "truncated": true,
        EVAL_RECEIPT_FIELD: SYNTHETIC_EVAL_RECEIPT,
    })
    .to_string();

    assert!(!suite.forced_case_result_passed(case, &result)?);
    Ok(())
}

#[test]
fn forced_git_status_verifier_rejects_index_extension_drift() -> EvalResult {
    let suite = FamilySuite::git()?;
    suite.prepare_git_case(GIT_STATUS_NAME)?;
    let case = GIT_CASES
        .iter()
        .find(|case| case.name == GIT_STATUS_NAME)
        .expect("the Git status fixture exists");
    let seed = suite
        .git_seed
        .expect("the Git eval suite has a captured seed identity");
    append_synthetic_git_index_extension(&Repository::open(suite.workspace.path())?)?;
    let result = serde_json::json!({
        "branch": GIT_BASE_BRANCH,
        "branch_truncated": false,
        "head": seed.to_string(),
        "entries": git_status_entries_json(),
        "truncated": true,
        EVAL_RECEIPT_FIELD: SYNTHETIC_EVAL_RECEIPT,
    })
    .to_string();

    assert!(!suite.forced_case_result_passed(case, &result)?);
    Ok(())
}

#[cfg(unix)]
#[test]
fn forced_git_status_verifier_rejects_a_symlinked_metadata_root() -> EvalResult {
    let suite = FamilySuite::git()?;
    suite.prepare_git_case(GIT_STATUS_NAME)?;
    let case = GIT_CASES
        .iter()
        .find(|case| case.name == GIT_STATUS_NAME)
        .expect("the Git status fixture exists");
    let seed = suite
        .git_seed
        .expect("the Git eval suite has a captured seed identity");
    let external = tempfile::tempdir()?;
    let metadata_root = suite.workspace.path().join(".git");
    let relocated = external.path().join("repository-metadata");
    fs::rename(&metadata_root, &relocated)?;
    std::os::unix::fs::symlink(&relocated, &metadata_root)?;
    let result = serde_json::json!({
        "branch": GIT_BASE_BRANCH,
        "branch_truncated": false,
        "head": seed.to_string(),
        "entries": git_status_entries_json(),
        "truncated": true,
        EVAL_RECEIPT_FIELD: SYNTHETIC_EVAL_RECEIPT,
    })
    .to_string();

    assert!(!suite.forced_case_result_passed(case, &result)?);
    Ok(())
}

#[test]
fn forced_git_status_verifier_rejects_incorrect_entry_metadata() -> EvalResult {
    let suite = FamilySuite::git()?;
    suite.prepare_git_case(GIT_STATUS_NAME)?;
    let case = GIT_CASES
        .iter()
        .find(|case| case.name == GIT_STATUS_NAME)
        .expect("the Git status fixture exists");
    let seed = suite
        .git_seed
        .expect("the Git eval suite has a captured seed identity");
    let mut entries = git_status_entries_json();
    entries[1]["worktree"] = serde_json::json!("modified");
    let result = serde_json::json!({
        "branch": GIT_BASE_BRANCH,
        "branch_truncated": false,
        "head": seed.to_string(),
        "entries": entries,
        "truncated": true,
        EVAL_RECEIPT_FIELD: SYNTHETIC_EVAL_RECEIPT,
    })
    .to_string();

    assert!(!suite.forced_case_result_passed(case, &result)?);
    Ok(())
}

#[test]
fn forced_git_status_verifier_rejects_a_repository_state_change() -> EvalResult {
    let suite = FamilySuite::git()?;
    suite.prepare_git_case(GIT_STATUS_NAME)?;
    let case = GIT_CASES
        .iter()
        .find(|case| case.name == GIT_STATUS_NAME)
        .expect("the Git status fixture exists");
    let seed = suite
        .git_seed
        .expect("the Git eval suite has a captured seed identity");
    let result = serde_json::json!({
        "branch": GIT_BASE_BRANCH,
        "branch_truncated": false,
        "head": seed.to_string(),
        "entries": git_status_entries_json(),
        "truncated": true,
        EVAL_RECEIPT_FIELD: SYNTHETIC_EVAL_RECEIPT,
    })
    .to_string();
    let repository = Repository::open(suite.workspace.path())?;
    let target = repository
        .find_branch("switch-target", BranchType::Local)?
        .into_reference()
        .peel_to_commit()?;
    repository.checkout_tree(target.as_object(), None)?;
    repository.set_head("refs/heads/switch-target")?;

    assert!(!suite.forced_case_result_passed(case, &result)?);
    Ok(())
}

#[test]
fn forced_git_status_verifier_rejects_repository_config_drift() -> EvalResult {
    let suite = FamilySuite::git()?;
    suite.prepare_git_case(GIT_STATUS_NAME)?;
    let case = GIT_CASES
        .iter()
        .find(|case| case.name == GIT_STATUS_NAME)
        .expect("the Git status fixture exists");
    let seed = suite
        .git_seed
        .expect("the Git eval suite has a captured seed identity");
    let result = serde_json::json!({
        "branch": GIT_BASE_BRANCH,
        "branch_truncated": false,
        "head": seed.to_string(),
        "entries": git_status_entries_json(),
        "truncated": true,
        EVAL_RECEIPT_FIELD: SYNTHETIC_EVAL_RECEIPT,
    })
    .to_string();
    Repository::open(suite.workspace.path())?
        .config()?
        .set_str(SYNTHETIC_GIT_CONFIG_KEY, SYNTHETIC_GIT_CONFIG_VALUE)?;

    assert!(!suite.forced_case_result_passed(case, &result)?);
    Ok(())
}

#[test]
fn forced_git_status_verifier_rejects_a_collateral_path() -> EvalResult {
    let suite = FamilySuite::git()?;
    suite.prepare_git_case(GIT_STATUS_NAME)?;
    let case = GIT_CASES
        .iter()
        .find(|case| case.name == GIT_STATUS_NAME)
        .expect("the Git status fixture exists");
    let seed = suite
        .git_seed
        .expect("the Git eval suite has a captured seed identity");
    let result = serde_json::json!({
        "branch": GIT_BASE_BRANCH,
        "branch_truncated": false,
        "head": seed.to_string(),
        "entries": git_status_entries_json(),
        "truncated": true,
        EVAL_RECEIPT_FIELD: SYNTHETIC_EVAL_RECEIPT,
    })
    .to_string();
    fs::write(
        suite.workspace.path().join("zz-collateral.txt"),
        b"collateral\n",
    )?;

    assert!(!suite.forced_case_result_passed(case, &result)?);
    Ok(())
}

#[test]
fn forced_git_status_verifier_rejects_an_unbounded_result() -> EvalResult {
    let suite = FamilySuite::git()?;
    suite.prepare_git_case(GIT_STATUS_NAME)?;
    let case = GIT_CASES
        .iter()
        .find(|case| case.name == GIT_STATUS_NAME)
        .expect("the Git status fixture exists");
    let seed = suite
        .git_seed
        .expect("the Git eval suite has a captured seed identity");
    let mut entries = git_status_entries_json();
    entries.push(serde_json::json!({
        "path": git_status_overflow_path(GIT_STATUS_OVERFLOW_ENTRY_COUNT - 1),
        "previous_path": null,
        "index": "unchanged",
        "worktree": "untracked",
    }));
    let result = serde_json::json!({
        "branch": GIT_BASE_BRANCH,
        "branch_truncated": false,
        "head": seed.to_string(),
        "entries": entries,
        "truncated": false,
        EVAL_RECEIPT_FIELD: SYNTHETIC_EVAL_RECEIPT,
    })
    .to_string();

    assert!(!suite.forced_case_result_passed(case, &result)?);
    Ok(())
}

#[test]
fn git_natural_result_payloads_accept_the_exact_results() -> EvalResult {
    let (suite, snapshot, head) = prepared_git_natural_result_case()?;
    let tracker = OperationTracker::default();
    record_git_natural_results(
        &tracker,
        GitNaturalResultFixture {
            staged_paths: GIT_NATURAL_STAGED_PATH_COUNT,
            commit: &head,
            state_cleaned: true,
        },
    );

    assert!(git_natural_result_payloads_passed(
        suite.workspace.path(),
        &snapshot,
        &tracker,
    )?);
    Ok(())
}

#[test]
fn git_natural_result_payloads_reject_an_unknown_stage_field() -> EvalResult {
    let (suite, snapshot, head) = prepared_git_natural_result_case()?;
    let tracker = OperationTracker::default();
    tracker.observe_result(
        Uuid::from_u128(ARBITRARY_EVAL_REQUEST_ID),
        &serde_json::json!({
            "staged_paths": GIT_NATURAL_STAGED_PATH_COUNT,
            "error": "synthetic contradictory field",
            EVAL_RECEIPT_FIELD: SYNTHETIC_EVAL_RECEIPT,
        })
        .to_string(),
    );
    tracker.observe_result(
        Uuid::from_u128(ARBITRARY_SECOND_EVAL_REQUEST_ID),
        &serde_json::json!({
            "commit": head,
            "state_cleaned": true,
            EVAL_RECEIPT_FIELD: SYNTHETIC_EVAL_RECEIPT,
        })
        .to_string(),
    );

    assert!(!git_natural_result_payloads_passed(
        suite.workspace.path(),
        &snapshot,
        &tracker,
    )?);
    Ok(())
}

#[test]
fn git_natural_result_payloads_reject_the_wrong_staged_count() -> EvalResult {
    let (suite, snapshot, head) = prepared_git_natural_result_case()?;
    let tracker = OperationTracker::default();
    record_git_natural_results(
        &tracker,
        GitNaturalResultFixture {
            staged_paths: SYNTHETIC_WRONG_STAGED_PATH_COUNT,
            commit: &head,
            state_cleaned: true,
        },
    );

    assert!(!git_natural_result_payloads_passed(
        suite.workspace.path(),
        &snapshot,
        &tracker,
    )?);
    Ok(())
}

#[test]
fn git_natural_result_payloads_reject_the_wrong_commit() -> EvalResult {
    let (suite, snapshot, _head) = prepared_git_natural_result_case()?;
    let tracker = OperationTracker::default();
    record_git_natural_results(
        &tracker,
        GitNaturalResultFixture {
            staged_paths: GIT_NATURAL_STAGED_PATH_COUNT,
            commit: SYNTHETIC_WRONG_COMMIT_ID,
            state_cleaned: true,
        },
    );

    assert!(!git_natural_result_payloads_passed(
        suite.workspace.path(),
        &snapshot,
        &tracker,
    )?);
    Ok(())
}

#[test]
fn git_natural_result_payloads_require_cleanup() -> EvalResult {
    let (suite, snapshot, head) = prepared_git_natural_result_case()?;
    let tracker = OperationTracker::default();
    record_git_natural_results(
        &tracker,
        GitNaturalResultFixture {
            staged_paths: GIT_NATURAL_STAGED_PATH_COUNT,
            commit: &head,
            state_cleaned: false,
        },
    );

    assert!(!git_natural_result_payloads_passed(
        suite.workspace.path(),
        &snapshot,
        &tracker,
    )?);
    Ok(())
}

#[test]
fn git_natural_state_requires_a_later_model_call_for_the_commit() -> EvalResult {
    let snapshot = CaseSnapshot {
        turn_disposition: SnapshotTurnDisposition::Completed,
        requests: vec![
            RequestSnapshot {
                request_id: Uuid::from_u128(ARBITRARY_EVAL_REQUEST_ID),
                producing_model_call_id: Uuid::from_u128(ARBITRARY_EVAL_MODEL_CALL_ID),
                name: String::from(GIT_STAGE_NAME),
                arguments_text: normalized_arguments_text(
                    &serde_json::json!({"paths": [GIT_NATURAL_PATH]}).to_string(),
                )?,
                entry_index: ARBITRARY_REQUEST_ENTRY_INDEX,
                completed_result_entry_index: Some(ARBITRARY_COMPLETED_RESULT_ENTRY_INDEX),
                attempt_succeeded: true,
                attempt_denied: false,
            },
            RequestSnapshot {
                request_id: Uuid::from_u128(ARBITRARY_EVAL_REQUEST_ID),
                producing_model_call_id: Uuid::from_u128(ARBITRARY_EVAL_MODEL_CALL_ID),
                name: String::from(GIT_CREATE_COMMIT_NAME),
                arguments_text: normalized_arguments_text(
                    &serde_json::json!({"message": GIT_NATURAL_MESSAGE}).to_string(),
                )?,
                entry_index: ARBITRARY_REQUEST_ENTRY_INDEX,
                completed_result_entry_index: Some(ARBITRARY_COMPLETED_RESULT_ENTRY_INDEX),
                attempt_succeeded: true,
                attempt_denied: false,
            },
        ],
        model_calls: MINIMUM_MODEL_CALLS_FOR_RESULT_ROUND_TRIP,
    };

    assert!(!snapshot.git_natural_requests_passed()?);
    Ok(())
}

#[test]
fn git_natural_state_requires_the_stage_result_before_the_commit_call() -> EvalResult {
    let snapshot = CaseSnapshot {
        turn_disposition: SnapshotTurnDisposition::Completed,
        requests: vec![
            RequestSnapshot {
                request_id: Uuid::from_u128(ARBITRARY_EVAL_REQUEST_ID),
                producing_model_call_id: Uuid::from_u128(ARBITRARY_EVAL_MODEL_CALL_ID),
                name: String::from(GIT_STAGE_NAME),
                arguments_text: normalized_arguments_text(
                    &serde_json::json!({"paths": [GIT_NATURAL_PATH]}).to_string(),
                )?,
                entry_index: ARBITRARY_REQUEST_ENTRY_INDEX,
                completed_result_entry_index: Some(ARBITRARY_LATE_RESULT_ENTRY_INDEX),
                attempt_succeeded: true,
                attempt_denied: false,
            },
            RequestSnapshot {
                request_id: Uuid::from_u128(ARBITRARY_SECOND_EVAL_REQUEST_ID),
                producing_model_call_id: Uuid::from_u128(ARBITRARY_SECOND_EVAL_MODEL_CALL_ID),
                name: String::from(GIT_CREATE_COMMIT_NAME),
                arguments_text: normalized_arguments_text(
                    &serde_json::json!({"message": GIT_NATURAL_MESSAGE}).to_string(),
                )?,
                entry_index: ARBITRARY_REQUEST_ENTRY_INDEX,
                completed_result_entry_index: Some(ARBITRARY_COMPLETED_RESULT_ENTRY_INDEX),
                attempt_succeeded: true,
                attempt_denied: false,
            },
        ],
        model_calls: MINIMUM_MODEL_CALLS_FOR_RESULT_ROUND_TRIP,
    };

    assert!(!snapshot.git_natural_requests_passed()?);
    Ok(())
}

#[test]
fn git_natural_requests_reject_extra_staging_after_the_target_commit() -> EvalResult {
    let snapshot = CaseSnapshot {
        turn_disposition: SnapshotTurnDisposition::Completed,
        requests: vec![
            RequestSnapshot {
                request_id: Uuid::from_u128(ARBITRARY_EVAL_REQUEST_ID),
                producing_model_call_id: Uuid::from_u128(ARBITRARY_EVAL_MODEL_CALL_ID),
                name: String::from(GIT_STAGE_NAME),
                arguments_text: normalized_arguments_text(
                    &serde_json::json!({"paths": [GIT_NATURAL_PATH]}).to_string(),
                )?,
                entry_index: ARBITRARY_REQUEST_ENTRY_INDEX,
                completed_result_entry_index: Some(ARBITRARY_COMPLETED_RESULT_ENTRY_INDEX),
                attempt_succeeded: true,
                attempt_denied: false,
            },
            RequestSnapshot {
                request_id: Uuid::from_u128(ARBITRARY_SECOND_EVAL_REQUEST_ID),
                producing_model_call_id: Uuid::from_u128(ARBITRARY_SECOND_EVAL_MODEL_CALL_ID),
                name: String::from(GIT_CREATE_COMMIT_NAME),
                arguments_text: normalized_arguments_text(
                    &serde_json::json!({"message": GIT_NATURAL_MESSAGE}).to_string(),
                )?,
                entry_index: ARBITRARY_REQUEST_ENTRY_INDEX,
                completed_result_entry_index: Some(ARBITRARY_COMPLETED_RESULT_ENTRY_INDEX),
                attempt_succeeded: true,
                attempt_denied: false,
            },
            RequestSnapshot {
                request_id: Uuid::from_u128(ARBITRARY_EVAL_REQUEST_ID),
                producing_model_call_id: Uuid::from_u128(ARBITRARY_SECOND_EVAL_MODEL_CALL_ID),
                name: String::from(GIT_STAGE_NAME),
                arguments_text: normalized_arguments_text(
                    &serde_json::json!({"paths": [GIT_STAGE_PATH]}).to_string(),
                )?,
                entry_index: ARBITRARY_REQUEST_ENTRY_INDEX,
                completed_result_entry_index: Some(ARBITRARY_COMPLETED_RESULT_ENTRY_INDEX),
                attempt_succeeded: true,
                attempt_denied: false,
            },
        ],
        model_calls: MINIMUM_MODEL_CALLS_FOR_RESULT_ROUND_TRIP,
    };

    assert!(!snapshot.git_natural_requests_passed()?);
    Ok(())
}
