//! Exec evaluation coverage.

use super::*;

#[test]
fn family_executor_error_preserves_its_concrete_source() {
    let error = FamilyExecutorError::new(io::Error::other(SYNTHETIC_EXECUTOR_FAILURE));

    assert_eq!(
        error.source().map(ToString::to_string),
        Some(String::from(SYNTHETIC_EXECUTOR_FAILURE))
    );
}

#[test]
fn exec_file_creation_report_accepts_failure_free_completion() {
    let tracker = OperationTracker::default();
    tracker.observe_response_text(SYNTHETIC_FAILURE_FREE_COMPLETION_REPORT, false);

    assert!(tracker.final_response_reports_file_creation());
}

#[test]
fn exec_file_creation_report_rejects_not_failure_free() {
    let tracker = OperationTracker::default();
    tracker.observe_response_text(SYNTHETIC_NOT_FAILURE_FREE_REPORT, false);

    assert!(!tracker.final_response_reports_file_creation());
}

#[test]
fn exec_file_creation_report_accepts_affirmative_execution_and_existence() {
    let tracker = OperationTracker::default();
    tracker.observe_response_text(SYNTHETIC_EXECUTED_COMPLETION_REPORT, false);

    assert!(tracker.final_response_reports_file_creation());
}

#[test]
fn final_response_report_rejects_completion_when_the_command_was_not_executed() {
    let tracker = OperationTracker::default();
    tracker.observe_response_text(SYNTHETIC_COMMAND_NOT_EXECUTED_REPORT, false);

    assert!(!tracker.final_response_reports_completion());
}

#[test]
fn final_response_report_rejects_an_execution_issue() {
    let tracker = OperationTracker::default();
    tracker.observe_response_text(SYNTHETIC_EXECUTION_ISSUE_REPORT, false);

    assert!(!tracker.final_response_reports_completion());
}

#[test]
fn final_response_report_rejects_an_execution_problem() {
    let tracker = OperationTracker::default();
    tracker.observe_response_text(SYNTHETIC_EXECUTION_PROBLEM_REPORT, false);

    assert!(!tracker.final_response_reports_completion());
}

#[test]
fn final_response_report_accepts_negated_execution_issues() {
    let tracker = OperationTracker::default();
    tracker.observe_response_text(SYNTHETIC_NO_EXECUTION_ISSUES_REPORT, false);

    assert!(tracker.final_response_reports_completion());
}

#[test]
fn final_response_report_accepts_negated_execution_problems() {
    let tracker = OperationTracker::default();
    tracker.observe_response_text(SYNTHETIC_NO_EXECUTION_PROBLEMS_REPORT, false);

    assert!(tracker.final_response_reports_completion());
}

#[test]
fn forced_exec_report_rejects_a_denial_of_required_output() {
    let tracker = OperationTracker::default();
    tracker.observe_response_text(SYNTHETIC_NO_EXEC_OUTPUT_REPORT, false);

    assert!(!forced_case_completion_reported(
        SANDBOXED_EXEC_NAME,
        true,
        &tracker,
    ));
}

#[test]
fn forced_exec_report_accepts_a_truthful_empty_stderr_claim() {
    let tracker = OperationTracker::default();
    tracker.observe_response_text(SYNTHETIC_EMPTY_STDERR_OUTPUT_REPORT, false);

    assert!(forced_case_completion_reported(
        SANDBOXED_EXEC_NAME,
        true,
        &tracker,
    ));
}

#[test]
fn forced_exec_report_rejects_an_empty_stdout_claim() {
    let tracker = OperationTracker::default();
    tracker.observe_response_text(SYNTHETIC_EMPTY_STDOUT_OUTPUT_REPORT, false);

    assert!(!forced_case_completion_reported(
        SANDBOXED_EXEC_NAME,
        true,
        &tracker,
    ));
}

#[test]
fn forced_exec_report_rejects_an_incorrect_stdout_claim() {
    let tracker = OperationTracker::default();
    tracker.observe_response_text(SYNTHETIC_INCORRECT_STDOUT_OUTPUT_REPORT, false);

    assert!(!forced_case_completion_reported(
        SANDBOXED_EXEC_NAME,
        true,
        &tracker,
    ));
}

#[test]
fn forced_exec_report_accepts_a_not_empty_stdout_assurance() {
    let tracker = OperationTracker::default();
    tracker.observe_response_text(SYNTHETIC_NOT_EMPTY_STDOUT_OUTPUT_REPORT, false);

    assert!(forced_case_completion_reported(
        SANDBOXED_EXEC_NAME,
        true,
        &tracker,
    ));
}

#[test]
fn natural_exec_report_accepts_a_truthful_no_output_claim() {
    let tracker = OperationTracker::default();
    tracker.observe_response_text(SYNTHETIC_NATURAL_NO_EXEC_OUTPUT_REPORT, false);

    assert!(
        tracker.final_response_reports_file_creation_excepting_path(Path::new(EXEC_RESULT_PATH))
    );
}

#[test]
fn natural_exec_report_keeps_requested_contents_within_their_clause() {
    let tracker = OperationTracker::default();
    tracker.observe_response_text(SYNTHETIC_NATURAL_EMPTY_STDERR_REPORT, false);

    assert!(
        tracker.final_response_reports_file_creation_excepting_path(Path::new(EXEC_RESULT_PATH))
    );
}

#[test]
fn natural_exec_report_stops_requested_contents_at_a_collateral_comma_clause() {
    let tracker = OperationTracker::default();
    tracker.observe_response_text(SYNTHETIC_NATURAL_COMMA_EMPTY_STDERR_REPORT, false);

    assert!(
        tracker.final_response_reports_file_creation_excepting_path(Path::new(EXEC_RESULT_PATH))
    );
}

#[test]
fn exec_file_creation_report_rejects_completion_with_no_file_changes() {
    let tracker = OperationTracker::default();
    tracker.observe_response_text(SYNTHETIC_NO_FILE_CHANGES_COMPLETION_REPORT, false);

    assert!(!tracker.final_response_reports_file_creation());
}

#[test]
fn exec_file_creation_report_rejects_an_auxiliary_no_file_change() {
    let tracker = OperationTracker::default();
    tracker.observe_response_text(SYNTHETIC_NO_FILE_WAS_CHANGED_COMPLETION_REPORT, false);

    assert!(!tracker.final_response_reports_file_creation());
}

#[test]
fn exec_file_creation_report_rejects_a_plural_no_file_change() {
    let tracker = OperationTracker::default();
    tracker.observe_response_text(SYNTHETIC_NO_FILES_WERE_MODIFIED_COMPLETION_REPORT, false);

    assert!(!tracker.final_response_reports_file_creation());
}

#[test]
fn exec_file_creation_report_rejects_a_plural_no_file_creation() {
    let tracker = OperationTracker::default();
    tracker.observe_response_text(SYNTHETIC_NO_FILES_WERE_CREATED_COMPLETION_REPORT, false);

    assert!(!tracker.final_response_reports_file_creation());
}

#[test]
fn exec_file_creation_report_rejects_zero_files_created() {
    let tracker = OperationTracker::default();
    tracker.observe_response_text(SYNTHETIC_ZERO_FILES_WERE_CREATED_COMPLETION_REPORT, false);

    assert!(!tracker.final_response_reports_file_creation());
}

#[test]
fn exec_file_creation_report_rejects_a_verb_first_creation_denial() {
    let tracker = OperationTracker::default();
    tracker.observe_response_text(SYNTHETIC_VERB_FIRST_CREATION_DENIAL_REPORT, false);

    assert!(!tracker.final_response_reports_file_creation());
}

#[test]
fn exec_file_creation_report_rejects_an_outcome_first_creation_denial() {
    let tracker = OperationTracker::default();
    tracker.observe_response_text(SYNTHETIC_OUTCOME_FIRST_CREATION_DENIAL_REPORT, false);

    assert!(!tracker.final_response_reports_file_creation());
}

#[test]
fn exec_file_creation_report_rejects_a_dotted_filename_denial() {
    let tracker = OperationTracker::default();
    tracker.observe_response_text(SYNTHETIC_DOTTED_FILE_CREATION_DENIAL_REPORT, false);

    assert!(!tracker.final_response_reports_file_creation());
}

#[test]
fn exec_file_creation_report_rejects_a_generated_file_denial() {
    let tracker = OperationTracker::default();
    tracker.observe_response_text(SYNTHETIC_GENERATED_FILE_DENIAL_REPORT, false);

    assert!(
        !tracker.final_response_reports_file_creation_excepting_path(Path::new(EXEC_RESULT_PATH))
    );
}

#[test]
fn exec_file_creation_report_rejects_without_creating_any_files() {
    let tracker = OperationTracker::default();
    tracker.observe_response_text(SYNTHETIC_WITHOUT_CREATING_DENIAL_REPORT, false);

    assert!(!tracker.final_response_reports_file_creation());
}

#[test]
fn exec_file_creation_report_rejects_a_verb_first_modification_denial() {
    let tracker = OperationTracker::default();
    tracker.observe_response_text(SYNTHETIC_VERB_FIRST_MODIFICATION_DENIAL_REPORT, false);

    assert!(!tracker.final_response_reports_file_creation());
}

#[test]
fn exec_file_creation_report_rejects_without_modifying_any_files() {
    let tracker = OperationTracker::default();
    tracker.observe_response_text(SYNTHETIC_WITHOUT_MODIFYING_DENIAL_REPORT, false);

    assert!(!tracker.final_response_reports_file_creation());
}

#[test]
fn exec_file_creation_report_accepts_collateral_without_modifying() {
    let tracker = OperationTracker::default();
    tracker.observe_response_text(SYNTHETIC_COLLATERAL_WITHOUT_MODIFYING_REPORT, false);

    assert!(tracker.final_response_reports_file_creation());
}

#[test]
fn exec_file_creation_report_accepts_an_existing_file_assurance() {
    let tracker = OperationTracker::default();
    tracker.observe_response_text(SYNTHETIC_EXISTING_FILE_ASSURANCE_REPORT, false);

    assert!(tracker.final_response_reports_file_creation());
}

#[test]
fn exec_file_creation_report_accepts_a_preexisting_file_assurance() {
    let tracker = OperationTracker::default();
    tracker.observe_response_text(SYNTHETIC_PREEXISTING_FILE_ASSURANCE_REPORT, false);

    assert!(tracker.final_response_reports_file_creation());
}

#[test]
fn exec_file_creation_report_accepts_a_qualifier_first_existing_file_assurance() {
    let tracker = OperationTracker::default();
    tracker.observe_response_text(
        SYNTHETIC_QUALIFIER_FIRST_EXISTING_FILE_ASSURANCE_REPORT,
        false,
    );

    assert!(tracker.final_response_reports_file_creation());
}

#[test]
fn exec_file_creation_report_accepts_nothing_else_changed() {
    let tracker = OperationTracker::default();
    tracker.observe_response_text(SYNTHETIC_COLLATERAL_NOTHING_ELSE_CHANGED_REPORT, false);

    assert!(tracker.final_response_reports_file_creation());
}

#[test]
fn exec_file_creation_report_accepts_the_requested_file_exception() {
    let tracker = OperationTracker::default();
    tracker.observe_response_text(SYNTHETIC_REQUESTED_FILE_EXCEPTION_REPORT, false);

    assert!(
        tracker.final_response_reports_file_creation_excepting_path(Path::new(EXEC_RESULT_PATH))
    );
}

#[test]
fn exec_file_creation_report_accepts_the_requested_file_predicate_exception() {
    let tracker = OperationTracker::default();
    tracker.observe_response_text(SYNTHETIC_REQUESTED_FILE_PREDICATE_EXCEPTION_REPORT, false);

    assert!(report_affirms_completion_excepting_path(
        SYNTHETIC_REQUESTED_FILE_PREDICATE_EXCEPTION_REPORT,
        Path::new(EXEC_RESULT_PATH),
    ));
    assert!(!report_denies_file_changes_excepting_path(
        SYNTHETIC_REQUESTED_FILE_PREDICATE_EXCEPTION_REPORT,
        Path::new(EXEC_RESULT_PATH),
    ));
    assert!(
        tracker.final_response_reports_file_creation_excepting_path(Path::new(EXEC_RESULT_PATH))
    );
}

#[test]
fn exec_file_creation_report_accepts_the_requested_file_creation_exception() {
    let tracker = OperationTracker::default();
    tracker.observe_response_text(SYNTHETIC_REQUESTED_FILE_CREATION_EXCEPTION_REPORT, false);

    assert!(
        tracker.final_response_reports_file_creation_excepting_path(Path::new(EXEC_RESULT_PATH))
    );
}

#[test]
fn exec_file_creation_report_accepts_the_requested_file_besides_scope() {
    let tracker = OperationTracker::default();
    tracker.observe_response_text(SYNTHETIC_REQUESTED_FILE_BESIDES_REPORT, false);

    assert!(
        tracker.final_response_reports_file_creation_excepting_path(Path::new(EXEC_RESULT_PATH))
    );
}

#[test]
fn exec_file_creation_report_rejects_a_later_requested_path_denial() {
    let tracker = OperationTracker::default();
    tracker.observe_response_text(
        SYNTHETIC_REQUESTED_FILE_EXCEPTION_WITH_LATER_DENIAL_REPORT,
        false,
    );

    assert!(
        !tracker.final_response_reports_file_creation_excepting_path(Path::new(EXEC_RESULT_PATH))
    );
}

#[test]
fn exec_file_creation_report_rejects_a_deleted_requested_path() {
    let tracker = OperationTracker::default();
    tracker.observe_response_text(SYNTHETIC_REQUESTED_FILE_DELETED_REPORT, false);

    assert!(
        !tracker.final_response_reports_file_creation_excepting_path(Path::new(EXEC_RESULT_PATH))
    );
}

#[test]
fn exec_file_creation_report_rejects_a_pronoun_deleted_requested_path() {
    let tracker = OperationTracker::default();
    tracker.observe_response_text(SYNTHETIC_REQUESTED_FILE_PRONOUN_DELETED_REPORT, false);

    assert!(
        !tracker.final_response_reports_file_creation_excepting_path(Path::new(EXEC_RESULT_PATH))
    );
}

#[test]
fn exec_file_creation_report_rejects_a_removed_requested_path() {
    let tracker = OperationTracker::default();
    tracker.observe_response_text(SYNTHETIC_REQUESTED_FILE_REMOVED_REPORT, false);

    assert!(
        !tracker.final_response_reports_file_creation_excepting_path(Path::new(EXEC_RESULT_PATH))
    );
}

#[test]
fn exec_file_creation_report_rejects_an_empty_requested_path() {
    let tracker = OperationTracker::default();
    tracker.observe_response_text(SYNTHETIC_REQUESTED_FILE_EMPTY_REPORT, false);

    assert!(
        !tracker.final_response_reports_file_creation_excepting_path(Path::new(EXEC_RESULT_PATH))
    );
}

#[test]
fn exec_file_creation_report_rejects_a_zero_byte_requested_path() {
    let tracker = OperationTracker::default();
    tracker.observe_response_text(SYNTHETIC_REQUESTED_FILE_ZERO_BYTES_REPORT, false);

    assert!(
        !tracker.final_response_reports_file_creation_excepting_path(Path::new(EXEC_RESULT_PATH))
    );
}

#[test]
fn exec_file_creation_report_accepts_a_not_empty_assurance() {
    let tracker = OperationTracker::default();
    tracker.observe_response_text(SYNTHETIC_REQUESTED_FILE_NOT_EMPTY_REPORT, false);

    assert!(
        tracker.final_response_reports_file_creation_excepting_path(Path::new(EXEC_RESULT_PATH))
    );
}

#[test]
fn exec_file_creation_report_accepts_an_initially_empty_state() {
    let tracker = OperationTracker::default();
    tracker.observe_response_text(SYNTHETIC_REQUESTED_FILE_INITIALLY_EMPTY_REPORT, false);

    assert!(
        tracker.final_response_reports_file_creation_excepting_path(Path::new(EXEC_RESULT_PATH))
    );
}

#[test]
fn exec_file_creation_report_accepts_an_empty_at_first_state() {
    let tracker = OperationTracker::default();
    tracker.observe_response_text(SYNTHETIC_REQUESTED_FILE_EMPTY_AT_FIRST_REPORT, false);

    assert!(
        tracker.final_response_reports_file_creation_excepting_path(Path::new(EXEC_RESULT_PATH))
    );
}

#[test]
fn exec_file_creation_report_rejects_incorrect_requested_contents() {
    let tracker = OperationTracker::default();
    tracker.observe_response_text(SYNTHETIC_REQUESTED_FILE_INCORRECT_REPORT, false);

    assert!(
        !tracker.final_response_reports_file_creation_excepting_path(Path::new(EXEC_RESULT_PATH))
    );
}

#[test]
fn exec_file_creation_report_rejects_wrong_requested_contents() {
    let tracker = OperationTracker::default();
    tracker.observe_response_text(SYNTHETIC_REQUESTED_FILE_WRONG_REPORT, false);

    assert!(
        !tracker.final_response_reports_file_creation_excepting_path(Path::new(EXEC_RESULT_PATH))
    );
}

#[test]
fn exec_file_creation_report_rejects_mismatched_requested_contents() {
    let tracker = OperationTracker::default();
    tracker.observe_response_text(SYNTHETIC_REQUESTED_FILE_MISMATCHED_REPORT, false);

    assert!(
        !tracker.final_response_reports_file_creation_excepting_path(Path::new(EXEC_RESULT_PATH))
    );
}

#[test]
fn exec_file_creation_report_accepts_not_incorrect_contents() {
    let tracker = OperationTracker::default();
    tracker.observe_response_text(SYNTHETIC_REQUESTED_FILE_NOT_INCORRECT_REPORT, false);

    assert!(
        tracker.final_response_reports_file_creation_excepting_path(Path::new(EXEC_RESULT_PATH))
    );
}

#[test]
fn exec_file_creation_report_accepts_a_not_deleted_assurance() {
    let tracker = OperationTracker::default();
    tracker.observe_response_text(SYNTHETIC_REQUESTED_FILE_NOT_DELETED_ASSURANCE_REPORT, false);

    assert!(
        tracker.final_response_reports_file_creation_excepting_path(Path::new(EXEC_RESULT_PATH))
    );
}

#[test]
fn exec_file_creation_report_accepts_a_backup_file_assurance() {
    let tracker = OperationTracker::default();
    tracker.observe_response_text(SYNTHETIC_BACKUP_FILE_ASSURANCE_REPORT, false);

    assert!(
        tracker.final_response_reports_file_creation_excepting_path(Path::new(EXEC_RESULT_PATH))
    );
}

#[test]
fn exec_file_creation_report_rejects_an_unrelated_file_exception() {
    let tracker = OperationTracker::default();
    tracker.observe_response_text(SYNTHETIC_REQUESTED_FILE_EXCEPTION_REPORT, false);

    assert!(
        !tracker.final_response_reports_file_creation_excepting_path(Path::new("unrelated.txt"))
    );
}

#[test]
fn exec_file_creation_report_rejects_a_nominalized_modification_denial() {
    let tracker = OperationTracker::default();
    tracker.observe_response_text(SYNTHETIC_NOMINALIZED_MODIFICATION_DENIAL_REPORT, false);

    assert!(!tracker.final_response_reports_file_creation());
}

#[test]
fn exec_file_creation_report_rejects_a_bare_nominalized_modification_denial() {
    let tracker = OperationTracker::default();
    tracker.observe_response_text(SYNTHETIC_BARE_NOMINALIZED_MODIFICATION_DENIAL_REPORT, false);

    assert!(!tracker.final_response_reports_file_creation());
}

#[test]
fn exec_file_creation_report_rejects_an_inverted_modification_denial() {
    let tracker = OperationTracker::default();
    tracker.observe_response_text(SYNTHETIC_INVERTED_MODIFICATION_DENIAL_REPORT, false);

    assert!(!tracker.final_response_reports_file_creation());
}

#[test]
fn exec_file_creation_report_accepts_a_collateral_modification_denial() {
    let tracker = OperationTracker::default();
    tracker.observe_response_text(SYNTHETIC_SCOPED_NEGATION_COMPLETION_REPORT, false);

    assert!(tracker.final_response_reports_file_creation());
}

#[test]
fn exec_file_creation_report_accepts_an_additional_file_denial() {
    let tracker = OperationTracker::default();
    tracker.observe_response_text(SYNTHETIC_ADDITIONAL_FILE_MODIFICATION_REPORT, false);

    assert!(tracker.final_response_reports_file_creation());
}

#[test]
fn exec_file_creation_report_accepts_a_collateral_nominalized_denial() {
    let tracker = OperationTracker::default();
    tracker.observe_response_text(
        SYNTHETIC_COLLATERAL_NOMINALIZED_MODIFICATION_DENIAL_REPORT,
        false,
    );

    assert!(tracker.final_response_reports_file_creation());
}

#[test]
fn exec_file_creation_report_accepts_a_collateral_inverted_denial() {
    let tracker = OperationTracker::default();
    tracker.observe_response_text(
        SYNTHETIC_COLLATERAL_INVERTED_MODIFICATION_DENIAL_REPORT,
        false,
    );

    assert!(tracker.final_response_reports_file_creation());
}

#[test]
fn exec_file_creation_report_rejects_a_bare_no_changes_denial() {
    let tracker = OperationTracker::default();
    tracker.observe_response_text(SYNTHETIC_BARE_NO_CHANGES_DENIAL_REPORT, false);

    assert!(!tracker.final_response_reports_file_creation());
}

#[test]
fn exec_file_creation_report_rejects_a_zero_changes_denial() {
    let tracker = OperationTracker::default();
    tracker.observe_response_text(SYNTHETIC_ZERO_CHANGES_DENIAL_REPORT, false);

    assert!(!tracker.final_response_reports_file_creation());
}

#[test]
fn exec_file_creation_report_rejects_a_verb_first_change_denial() {
    let tracker = OperationTracker::default();
    tracker.observe_response_text(SYNTHETIC_VERB_FIRST_CHANGE_DENIAL_REPORT, false);

    assert!(!tracker.final_response_reports_file_creation());
}

#[test]
fn exec_file_creation_report_accepts_a_collateral_verb_first_change_claim() {
    let tracker = OperationTracker::default();
    tracker.observe_response_text(SYNTHETIC_COLLATERAL_VERB_FIRST_CHANGE_REPORT, false);

    assert!(tracker.final_response_reports_file_creation());
}

#[test]
fn exec_file_creation_report_accepts_a_collateral_no_changes_claim() {
    let tracker = OperationTracker::default();
    tracker.observe_response_text(SYNTHETIC_COLLATERAL_NO_CHANGES_REPORT, false);

    assert!(tracker.final_response_reports_file_creation());
}

#[test]
fn exec_file_creation_report_accepts_a_collateral_no_modifications_claim() {
    let tracker = OperationTracker::default();
    tracker.observe_response_text(SYNTHETIC_COLLATERAL_NO_MODIFICATIONS_REPORT, false);

    assert!(tracker.final_response_reports_file_creation());
}

#[test]
fn exec_file_creation_report_rejects_a_bare_no_modifications_denial() {
    let tracker = OperationTracker::default();
    tracker.observe_response_text(SYNTHETIC_BARE_NO_MODIFICATIONS_DENIAL_REPORT, false);

    assert!(!tracker.final_response_reports_file_creation());
}

#[test]
fn exec_file_creation_report_rejects_a_perfect_tense_no_modifications_denial() {
    let tracker = OperationTracker::default();
    tracker.observe_response_text(
        SYNTHETIC_PERFECT_TENSE_NO_MODIFICATIONS_DENIAL_REPORT,
        false,
    );

    assert!(!tracker.final_response_reports_file_creation());
}

#[test]
fn exec_file_creation_report_rejects_an_existential_no_modifications_denial() {
    let tracker = OperationTracker::default();
    tracker.observe_response_text(SYNTHETIC_EXISTENTIAL_NO_MODIFICATIONS_DENIAL_REPORT, false);

    assert!(!tracker.final_response_reports_file_creation());
}

#[test]
fn exec_file_creation_report_accepts_a_collateral_existential_denial() {
    let tracker = OperationTracker::default();
    tracker.observe_response_text(
        SYNTHETIC_COLLATERAL_EXISTENTIAL_NO_MODIFICATIONS_REPORT,
        false,
    );

    assert!(tracker.final_response_reports_file_creation());
}

#[test]
fn exec_file_creation_report_rejects_an_unchanged_file_denial() {
    let tracker = OperationTracker::default();
    tracker.observe_response_text(SYNTHETIC_UNCHANGED_FILE_DENIAL_REPORT, false);

    assert!(!tracker.final_response_reports_file_creation());
}

#[test]
fn exec_file_creation_report_rejects_a_subject_first_missing_file() {
    let tracker = OperationTracker::default();
    tracker.observe_response_text(SYNTHETIC_SUBJECT_FIRST_MISSING_FILE_REPORT, false);

    assert!(!tracker.final_response_reports_file_creation());
}

#[test]
fn exec_file_creation_report_accepts_prior_nonexistence() {
    let tracker = OperationTracker::default();
    tracker.observe_response_text(SYNTHETIC_PRIOR_NONEXISTENCE_REPORT, false);

    assert!(tracker.final_response_reports_file_creation());
}

#[test]
fn exec_file_creation_report_accepts_previous_nonexistence() {
    let tracker = OperationTracker::default();
    tracker.observe_response_text(SYNTHETIC_PREVIOUS_NONEXISTENCE_REPORT, false);

    assert!(tracker.final_response_reports_file_creation());
}

#[test]
fn exec_file_creation_report_accepts_a_historical_missing_state() {
    let tracker = OperationTracker::default();
    tracker.observe_response_text(SYNTHETIC_HISTORICAL_MISSING_FILE_REPORT, false);

    assert!(tracker.final_response_reports_file_creation());
}

#[test]
fn exec_file_creation_report_rejects_completion_when_the_file_does_not_exist() {
    let tracker = OperationTracker::default();
    tracker.observe_response_text(SYNTHETIC_COMPLETION_WITHOUT_FILE_REPORT, false);

    assert!(!tracker.final_response_reports_file_creation());
}

#[test]
fn exec_file_creation_report_rejects_completion_when_the_file_is_missing() {
    let tracker = OperationTracker::default();
    tracker.observe_response_text(SYNTHETIC_MISSING_FILE_REPORT, false);

    assert!(!tracker.final_response_reports_file_creation());
}

#[test]
fn forced_read_only_exec_report_accepts_nothing_changed() {
    let tracker = OperationTracker::default();
    tracker.observe_response_text(SYNTHETIC_NOTHING_CHANGED_REPORT, false);

    assert!(forced_case_completion_reported(
        UNSANDBOXED_EXEC_NAME,
        true,
        &tracker,
    ));
}

#[test]
fn forced_sandboxed_exec_report_accepts_a_successful_run() {
    let tracker = OperationTracker::default();
    tracker.observe_response_text(SYNTHETIC_RAN_COMPLETION_REPORT, false);

    assert!(forced_case_completion_reported(
        SANDBOXED_EXEC_NAME,
        true,
        &tracker,
    ));
}

#[test]
fn forced_sandboxed_exec_report_accepts_an_explicit_success() {
    let tracker = OperationTracker::default();
    tracker.observe_response_text(SYNTHETIC_SUCCEEDED_EXEC_REPORT, false);

    assert!(forced_case_completion_reported(
        SANDBOXED_EXEC_NAME,
        true,
        &tracker,
    ));
}

#[test]
fn forced_sandboxed_exec_report_rejects_a_hedged_run() {
    let tracker = OperationTracker::default();
    tracker.observe_response_text(SYNTHETIC_HEDGED_RUN_REPORT, false);

    assert!(!forced_case_completion_reported(
        SANDBOXED_EXEC_NAME,
        true,
        &tracker,
    ));
}

#[test]
fn forced_sandboxed_exec_report_rejects_an_attempted_run() {
    let tracker = OperationTracker::default();
    tracker.observe_response_text(SYNTHETIC_ATTEMPTED_RUN_REPORT, false);

    assert!(!forced_case_completion_reported(
        SANDBOXED_EXEC_NAME,
        true,
        &tracker,
    ));
}

#[test]
fn forced_sandboxed_exec_report_accepts_an_attempt_followed_by_a_successful_run() {
    let tracker = OperationTracker::default();
    tracker.observe_response_text(SYNTHETIC_ATTEMPTED_THEN_RAN_REPORT, false);

    assert!(forced_case_completion_reported(
        SANDBOXED_EXEC_NAME,
        true,
        &tracker,
    ));
}

#[test]
fn forced_sandboxed_exec_report_rejects_a_partial_run() {
    let tracker = OperationTracker::default();
    tracker.observe_response_text(SYNTHETIC_PARTIAL_RUN_REPORT, false);

    assert!(!forced_case_completion_reported(
        SANDBOXED_EXEC_NAME,
        true,
        &tracker,
    ));
}

#[test]
fn forced_sandboxed_exec_report_rejects_a_skipped_run() {
    let tracker = OperationTracker::default();
    tracker.observe_response_text(SYNTHETIC_SKIPPED_RUN_REPORT, false);

    assert!(!forced_case_completion_reported(
        SANDBOXED_EXEC_NAME,
        true,
        &tracker,
    ));
}

#[test]
fn forced_sandboxed_exec_report_accepts_a_skip_followed_by_a_successful_run() {
    let tracker = OperationTracker::default();
    tracker.observe_response_text(SYNTHETIC_SKIPPED_THEN_RAN_REPORT, false);

    assert!(forced_case_completion_reported(
        SANDBOXED_EXEC_NAME,
        true,
        &tracker,
    ));
}

#[test]
fn forced_sandboxed_exec_report_rejects_an_aborted_run() {
    let tracker = OperationTracker::default();
    tracker.observe_response_text(SYNTHETIC_ABORTED_RUN_REPORT, false);

    assert!(!forced_case_completion_reported(
        SANDBOXED_EXEC_NAME,
        true,
        &tracker,
    ));
}

#[test]
fn forced_sandboxed_exec_report_rejects_a_canceled_run() {
    let tracker = OperationTracker::default();
    tracker.observe_response_text(SYNTHETIC_CANCELED_RUN_REPORT, false);

    assert!(!forced_case_completion_reported(
        SANDBOXED_EXEC_NAME,
        true,
        &tracker,
    ));
}

#[test]
fn forced_sandboxed_exec_report_accepts_cancellation_followed_by_a_successful_run() {
    let tracker = OperationTracker::default();
    tracker.observe_response_text(SYNTHETIC_CANCELED_THEN_RAN_REPORT, false);

    assert!(forced_case_completion_reported(
        SANDBOXED_EXEC_NAME,
        true,
        &tracker,
    ));
}

#[test]
fn forced_sandboxed_exec_report_rejects_an_interrupted_run() {
    let tracker = OperationTracker::default();
    tracker.observe_response_text(SYNTHETIC_INTERRUPTED_RUN_REPORT, false);

    assert!(!forced_case_completion_reported(
        SANDBOXED_EXEC_NAME,
        true,
        &tracker,
    ));
}

#[test]
fn forced_sandboxed_exec_report_rejects_a_stopped_run() {
    let tracker = OperationTracker::default();
    tracker.observe_response_text(SYNTHETIC_STOPPED_RUN_REPORT, false);

    assert!(!forced_case_completion_reported(
        SANDBOXED_EXEC_NAME,
        true,
        &tracker,
    ));
}

#[test]
fn forced_sandboxed_exec_report_accepts_a_not_interrupted_assurance() {
    let tracker = OperationTracker::default();
    tracker.observe_response_text(SYNTHETIC_NOT_INTERRUPTED_RUN_REPORT, false);

    assert!(forced_case_completion_reported(
        SANDBOXED_EXEC_NAME,
        true,
        &tracker,
    ));
}

#[test]
fn forced_sandboxed_exec_report_accepts_interruption_followed_by_a_successful_run() {
    let tracker = OperationTracker::default();
    tracker.observe_response_text(SYNTHETIC_INTERRUPTED_THEN_RAN_REPORT, false);

    assert!(forced_case_completion_reported(
        SANDBOXED_EXEC_NAME,
        true,
        &tracker,
    ));
}

#[test]
fn forced_sandboxed_exec_report_rejects_a_terminated_run() {
    let tracker = OperationTracker::default();
    tracker.observe_response_text(SYNTHETIC_TERMINATED_RUN_REPORT, false);

    assert!(!forced_case_completion_reported(
        SANDBOXED_EXEC_NAME,
        true,
        &tracker,
    ));
}

#[test]
fn forced_sandboxed_exec_report_rejects_a_killed_run() {
    let tracker = OperationTracker::default();
    tracker.observe_response_text(SYNTHETIC_KILLED_RUN_REPORT, false);

    assert!(!forced_case_completion_reported(
        SANDBOXED_EXEC_NAME,
        true,
        &tracker,
    ));
}

#[test]
fn forced_sandboxed_exec_report_accepts_a_not_terminated_assurance() {
    let tracker = OperationTracker::default();
    tracker.observe_response_text(SYNTHETIC_NOT_TERMINATED_RUN_REPORT, false);

    assert!(forced_case_completion_reported(
        SANDBOXED_EXEC_NAME,
        true,
        &tracker,
    ));
}

#[test]
fn forced_sandboxed_exec_report_accepts_termination_followed_by_a_successful_run() {
    let tracker = OperationTracker::default();
    tracker.observe_response_text(SYNTHETIC_TERMINATED_THEN_RAN_REPORT, false);

    assert!(forced_case_completion_reported(
        SANDBOXED_EXEC_NAME,
        true,
        &tracker,
    ));
}

#[test]
fn forced_sandboxed_exec_report_rejects_a_blocked_run() {
    let tracker = OperationTracker::default();
    tracker.observe_response_text(SYNTHETIC_BLOCKED_RUN_REPORT, false);

    assert!(!forced_case_completion_reported(
        SANDBOXED_EXEC_NAME,
        true,
        &tracker,
    ));
}

#[test]
fn forced_sandboxed_exec_report_rejects_a_prevented_run() {
    let tracker = OperationTracker::default();
    tracker.observe_response_text(SYNTHETIC_PREVENTED_RUN_REPORT, false);

    assert!(!forced_case_completion_reported(
        SANDBOXED_EXEC_NAME,
        true,
        &tracker,
    ));
}

#[test]
fn forced_sandboxed_exec_report_accepts_a_not_blocked_assurance() {
    let tracker = OperationTracker::default();
    tracker.observe_response_text(SYNTHETIC_NOT_BLOCKED_RUN_REPORT, false);

    assert!(forced_case_completion_reported(
        SANDBOXED_EXEC_NAME,
        true,
        &tracker,
    ));
}

#[test]
fn forced_sandboxed_exec_report_accepts_blocking_followed_by_a_successful_run() {
    let tracker = OperationTracker::default();
    tracker.observe_response_text(SYNTHETIC_BLOCKED_THEN_RAN_REPORT, false);

    assert!(forced_case_completion_reported(
        SANDBOXED_EXEC_NAME,
        true,
        &tracker,
    ));
}

#[test]
fn forced_sandboxed_exec_report_rejects_a_timed_out_run() {
    let tracker = OperationTracker::default();
    tracker.observe_response_text(SYNTHETIC_TIMED_OUT_RUN_REPORT, false);

    assert!(!forced_case_completion_reported(
        SANDBOXED_EXEC_NAME,
        true,
        &tracker,
    ));
}

#[test]
fn forced_sandboxed_exec_report_accepts_a_not_timed_out_assurance() {
    let tracker = OperationTracker::default();
    tracker.observe_response_text(SYNTHETIC_NOT_TIMED_OUT_RUN_REPORT, false);

    assert!(forced_case_completion_reported(
        SANDBOXED_EXEC_NAME,
        true,
        &tracker,
    ));
}

#[test]
fn forced_sandboxed_exec_report_accepts_timeout_followed_by_a_successful_run() {
    let tracker = OperationTracker::default();
    tracker.observe_response_text(SYNTHETIC_TIMED_OUT_THEN_RAN_REPORT, false);

    assert!(forced_case_completion_reported(
        SANDBOXED_EXEC_NAME,
        true,
        &tracker,
    ));
}

#[test]
fn forced_sandboxed_exec_report_accepts_completion_within_the_timeout() {
    let tracker = OperationTracker::default();
    tracker.observe_response_text(SYNTHETIC_WITHIN_TIMEOUT_RUN_REPORT, false);

    assert!(forced_case_completion_reported(
        SANDBOXED_EXEC_NAME,
        true,
        &tracker,
    ));
}

#[test]
fn forced_sandboxed_exec_report_accepts_completion_before_the_timeout() {
    let tracker = OperationTracker::default();
    tracker.observe_response_text(SYNTHETIC_BEFORE_TIMEOUT_RUN_REPORT, false);

    assert!(forced_case_completion_reported(
        SANDBOXED_EXEC_NAME,
        true,
        &tracker,
    ));
}

#[test]
fn forced_sandboxed_exec_report_rejects_a_bare_timeout_failure() {
    let tracker = OperationTracker::default();
    tracker.observe_response_text(SYNTHETIC_HIT_TIMEOUT_RUN_REPORT, false);

    assert!(!forced_case_completion_reported(
        SANDBOXED_EXEC_NAME,
        true,
        &tracker,
    ));
}

#[test]
fn forced_sandboxed_exec_report_accepts_an_explicit_worked_outcome() {
    let tracker = OperationTracker::default();
    tracker.observe_response_text(SYNTHETIC_WORKED_RUN_REPORT, false);

    assert!(forced_case_completion_reported(
        SANDBOXED_EXEC_NAME,
        true,
        &tracker,
    ));
}

#[test]
fn forced_sandboxed_exec_report_rejects_a_polite_run_request() {
    let tracker = OperationTracker::default();
    tracker.observe_response_text(SYNTHETIC_PLEASE_RUN_REPORT, false);

    assert!(!forced_case_completion_reported(
        SANDBOXED_EXEC_NAME,
        true,
        &tracker,
    ));
}

#[test]
fn forced_sandboxed_exec_report_rejects_an_imperative_run_request() {
    let tracker = OperationTracker::default();
    tracker.observe_response_text(SYNTHETIC_IMPERATIVE_RUN_REPORT, false);

    assert!(!forced_case_completion_reported(
        SANDBOXED_EXEC_NAME,
        true,
        &tracker,
    ));
}

#[test]
fn forced_sandboxed_exec_report_accepts_completion_with_an_ancillary_polite_request() {
    let tracker = OperationTracker::default();
    tracker.observe_response_text(SYNTHETIC_COMPLETED_RUN_WITH_ANCILLARY_REQUEST_REPORT, false);

    assert!(forced_case_completion_reported(
        SANDBOXED_EXEC_NAME,
        true,
        &tracker,
    ));
}

#[test]
fn forced_unsandboxed_exec_report_accepts_a_successful_run() {
    let tracker = OperationTracker::default();
    tracker.observe_response_text(SYNTHETIC_RAN_COMPLETION_REPORT, false);

    assert!(forced_case_completion_reported(
        UNSANDBOXED_EXEC_NAME,
        true,
        &tracker,
    ));
}

#[test]
fn exec_file_creation_report_accepts_a_confinement_assurance() {
    let tracker = OperationTracker::default();
    tracker.observe_response_text(SYNTHETIC_COLLATERAL_COULD_NOT_REPORT, false);

    assert!(report_affirms_completion_excepting_path(
        SYNTHETIC_COLLATERAL_COULD_NOT_REPORT,
        Path::new(EXEC_RESULT_PATH),
    ));
    assert!(!report_denies_file_changes_excepting_path(
        SYNTHETIC_COLLATERAL_COULD_NOT_REPORT,
        Path::new(EXEC_RESULT_PATH),
    ));
    assert!(
        tracker.final_response_reports_file_creation_excepting_path(Path::new(EXEC_RESULT_PATH))
    );
}

#[test]
fn exec_file_creation_report_rejects_nothing_changed() {
    let tracker = OperationTracker::default();
    tracker.observe_response_text(SYNTHETIC_NOTHING_CHANGED_REPORT, false);

    assert!(!tracker.final_response_reports_file_creation());
}

#[test]
fn unforced_exec_tier_reports_a_normalized_exact_failure_as_infrastructure() -> EvalResult {
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
                name: String::from(SANDBOXED_EXEC_NAME),
                arguments_text: normalized_arguments_text(EXEC_NATURAL_ARGUMENTS)?,
                entry_index: ARBITRARY_REQUEST_ENTRY_INDEX,
                completed_result_entry_index: None,
                attempt_succeeded: false,
                attempt_denied: false,
            }],
            model_calls: MINIMUM_MODEL_CALLS_FOR_RESULT_ROUND_TRIP,
        },
    };

    assert_eq!(
        outcome.natural_loop_disposition(EvalFamily::Exec),
        EvalDisposition::Infrastructure
    );
    Ok(())
}

#[cfg(unix)]
#[test]
fn workspace_mutation_target_time_gate_rejects_a_pre_execution_mtime() -> EvalResult {
    let suite = FamilySuite::workspace()?;
    let target = Path::new("created-during-execution.txt");
    let started = current_filesystem_recorded_time()?;
    fs::write(suite.workspace.path().join(target), WORKSPACE_ANSWER)?;
    let window = FilesystemExecutionTimeWindow {
        started,
        finished: current_filesystem_recorded_time()?,
    };
    assert!(workspace_mutation_entry_times_match(
        suite.workspace.path(),
        target,
        Some(window),
    )?);
    fs::File::open(suite.workspace.path().join(target))?
        .set_times(fs::FileTimes::new().set_modified(UNIX_EPOCH))?;

    assert!(!workspace_mutation_entry_times_match(
        suite.workspace.path(),
        target,
        Some(window),
    )?);
    Ok(())
}

#[test]
fn forced_workspace_edit_verifier_rejects_pre_execution_parent_mtime() -> EvalResult {
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
        .set_times(fs::FileTimes::new().set_modified(UNIX_EPOCH))?;
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

    assert!(!suite.forced_case_result_passed(case, &result)?);
    Ok(())
}

#[cfg(unix)]
#[test]
fn forced_workspace_edit_verifier_rejects_a_pre_execution_target_mtime() -> EvalResult {
    let suite = FamilySuite::workspace()?;
    let case = WORKSPACE_CASES
        .iter()
        .find(|case| case.name == EDIT_FILE_NAME)
        .expect("the workspace edit fixture exists");
    let target = suite.workspace.path().join(WORKSPACE_SEED_PATH);
    let started = current_filesystem_recorded_time()?;
    fs::write(&target, WORKSPACE_EDITED_SEED)?;
    suite.executor.record_filesystem_execution_window(
        EDIT_FILE_NAME,
        FilesystemExecutionTimeWindow {
            started,
            finished: current_filesystem_recorded_time()?,
        },
    );
    fs::File::open(target)?.set_times(fs::FileTimes::new().set_modified(UNIX_EPOCH))?;
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

#[test]
fn forced_execution_classifies_argument_drift_before_state_verification() -> EvalResult {
    let suite = FamilySuite::workspace()?;
    let case = WORKSPACE_CASES
        .iter()
        .find(|case| case.name == APPLY_PATCH_NAME)
        .expect("the apply-patch fixture exists");
    let expected_arguments = normalized_arguments_text(case.expected_arguments)?;
    let persisted_arguments = normalized_arguments_text(DRIFTED_APPLY_PATCH_ARGUMENTS)?;

    assert!(!forced_execution_completed(
        &suite,
        case,
        ForcedExecutionEvidence {
            persisted_arguments: &persisted_arguments,
            expected_arguments: &expected_arguments,
            result_content: r#"{"operations_applied":1}"#,
        },
    )?);
    Ok(())
}

#[test]
fn exec_eval_rejects_program_drift_before_dispatch() {
    let fixture = forced_exec_fixture(SANDBOXED_EXEC_NAME);
    let mut drifted: serde_json::Value = serde_json::from_str(fixture.expected_arguments)
        .expect("the sandboxed fixture arguments decode");
    drifted["program"] = serde_json::json!("curl");
    let drifted = NormalizedToolArguments::try_from_provider_text(drifted.to_string())
        .expect("drifted fixture arguments normalize");

    assert!(!ExecEvalCase::ForcedSandboxed.admits(SANDBOXED_EXEC_NAME, &drifted));
}

#[test]
fn exec_eval_rejects_argument_drift_before_dispatch() {
    let fixture = forced_exec_fixture(SANDBOXED_EXEC_NAME);
    let mut drifted: serde_json::Value = serde_json::from_str(fixture.expected_arguments)
        .expect("the sandboxed fixture arguments decode");
    drifted["arguments"] = serde_json::json!(["different output\n"]);
    let drifted = NormalizedToolArguments::try_from_provider_text(drifted.to_string())
        .expect("drifted fixture arguments normalize");

    assert!(!ExecEvalCase::ForcedSandboxed.admits(SANDBOXED_EXEC_NAME, &drifted));
}

#[test]
fn forced_unsandboxed_eval_denies_model_argument_drift() {
    let drifted = NormalizedToolArguments::try_from_provider_text(
        serde_json::json!({
            "program": "/usr/bin/printf",
            "arguments": ["different output\n"],
            "working_directory": ".",
            "timeout_seconds": 30,
        })
        .to_string(),
    )
    .expect("drifted unsandboxed fixture arguments normalize");

    let mut approval_state = ExecApprovalState::new(ExecApprovalMode::ApproveOneExactForced);

    assert_eq!(
        approval_state.decision(UNSANDBOXED_EXEC_NAME, &drifted),
        ToolApprovalDecision::Deny { reason: None }
    );
}

#[test]
fn unforced_exec_eval_denies_the_exact_forced_unsandboxed_fixture() {
    let unsandboxed = forced_exec_fixture(UNSANDBOXED_EXEC_NAME);
    let exact_forced = NormalizedToolArguments::try_from_provider_text(String::from(
        unsandboxed.expected_arguments,
    ))
    .expect("the exact forced unsandboxed fixture arguments normalize");

    let mut approval_state = ExecApprovalState::new(ExecApprovalMode::DenyAll);

    assert_eq!(
        approval_state.decision(UNSANDBOXED_EXEC_NAME, &exact_forced),
        ToolApprovalDecision::Deny { reason: None }
    );
}

#[test]
fn forced_exec_eval_approves_only_one_exact_unsandboxed_fixture() {
    let unsandboxed = forced_exec_fixture(UNSANDBOXED_EXEC_NAME);
    let exact_forced = NormalizedToolArguments::try_from_provider_text(String::from(
        unsandboxed.expected_arguments,
    ))
    .expect("the exact forced unsandboxed fixture arguments normalize");
    let mut approval_state = ExecApprovalState::new(ExecApprovalMode::ApproveOneExactForced);

    assert_eq!(
        approval_state.decision(UNSANDBOXED_EXEC_NAME, &exact_forced),
        ToolApprovalDecision::Approve
    );
    assert_eq!(
        approval_state.decision(UNSANDBOXED_EXEC_NAME, &exact_forced),
        ToolApprovalDecision::Deny { reason: None }
    );
}

#[test]
fn forced_exec_tier_reports_a_nonzero_process_result_as_infrastructure() {
    let mut execution = confined_exit(EXEC_FORCED_SANDBOXED_OUTPUT);
    execution["outcome"]["code"] = serde_json::json!(1);
    let outcome = forced_exec_outcome(SANDBOXED_EXEC_NAME, execution);

    assert_eq!(
        outcome.forced_disposition(),
        EvalDisposition::Infrastructure
    );
    assert_eq!(outcome.infrastructure_label(), "nonzero exit");
    assert!(reject_forced_executor_failures(&[outcome]).is_err());
}

#[test]
fn forced_exec_tier_reports_a_timeout_as_infrastructure() {
    let outcome = forced_exec_outcome(
        SANDBOXED_EXEC_NAME,
        direct_exec_result(DirectExecEvidence::timed_out()),
    );

    assert_eq!(
        outcome.forced_disposition(),
        EvalDisposition::Infrastructure
    );
    assert_eq!(outcome.infrastructure_label(), "timed out");
    assert!(reject_forced_executor_failures(&[outcome]).is_err());
}

#[test]
fn unforced_exec_tier_rejects_an_additional_tool_call() {
    let outcome = CaseOutcome {
        target: None,
        expected_arguments: None,
        execution_completed: true,
        forced_verification_failed: false,
        tool_results: vec![
            TrackedToolResult {
                request_id: Uuid::from_u128(ARBITRARY_EVAL_REQUEST_ID),
                content: confined_exit("").to_string(),
                is_error: false,
                round_tripped: true,
            },
            TrackedToolResult {
                request_id: Uuid::from_u128(ARBITRARY_SECOND_EVAL_REQUEST_ID),
                content: confined_exit("").to_string(),
                is_error: false,
                round_tripped: true,
            },
        ],
        snapshot: CaseSnapshot {
            turn_disposition: SnapshotTurnDisposition::Completed,
            requests: vec![
                RequestSnapshot {
                    request_id: Uuid::from_u128(ARBITRARY_EVAL_REQUEST_ID),
                    producing_model_call_id: Uuid::from_u128(ARBITRARY_EVAL_MODEL_CALL_ID),
                    name: String::from(SANDBOXED_EXEC_NAME),
                    arguments_text: String::from("{}"),
                    entry_index: ARBITRARY_REQUEST_ENTRY_INDEX,
                    completed_result_entry_index: Some(ARBITRARY_COMPLETED_RESULT_ENTRY_INDEX),
                    attempt_succeeded: true,
                    attempt_denied: false,
                },
                RequestSnapshot {
                    request_id: Uuid::from_u128(ARBITRARY_SECOND_EVAL_REQUEST_ID),
                    producing_model_call_id: Uuid::from_u128(ARBITRARY_EVAL_MODEL_CALL_ID),
                    name: String::from(CARGO_DIAGNOSTICS_NAME),
                    arguments_text: String::from("{}"),
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
        outcome.natural_loop_disposition(EvalFamily::Exec),
        EvalDisposition::Miss
    );
}

#[test]
fn forced_exec_tier_passes_the_exact_captured_output() {
    let outcome = forced_exec_outcome(
        SANDBOXED_EXEC_NAME,
        confined_exit(EXEC_FORCED_SANDBOXED_OUTPUT),
    );

    assert_eq!(outcome.forced_disposition(), EvalDisposition::Pass);
}

#[test]
fn forced_exec_tier_rejects_a_zero_exit_that_captured_nothing() {
    let outcome = forced_exec_outcome(SANDBOXED_EXEC_NAME, confined_exit(""));

    assert_eq!(
        outcome.forced_disposition(),
        EvalDisposition::Infrastructure
    );
}

#[test]
fn forced_exec_tier_rejects_the_other_case_s_output() {
    let outcome = forced_exec_outcome(
        SANDBOXED_EXEC_NAME,
        confined_exit(EXEC_FORCED_READ_ONLY_OUTPUT),
    );

    assert_eq!(
        outcome.forced_disposition(),
        EvalDisposition::Infrastructure
    );
}

#[test]
fn forced_direct_exec_rejects_an_unknown_top_level_field() {
    let mut result = confined_exit(EXEC_FORCED_SANDBOXED_OUTPUT);
    result["unexpected"] = serde_json::json!("synthetic contradictory field");
    let outcome = forced_exec_outcome(SANDBOXED_EXEC_NAME, result);

    assert_eq!(
        outcome.forced_disposition(),
        EvalDisposition::Infrastructure
    );
}

#[test]
fn natural_direct_exec_rejects_an_unknown_stream_field() {
    let mut result = confined_exit(EXEC_NATURAL_OUTPUT);
    result["stdout"]["unexpected"] = serde_json::json!("synthetic contradictory field");
    let outcome = natural_exec_outcome(result);

    assert_eq!(
        outcome.natural_loop_disposition(EvalFamily::Exec),
        EvalDisposition::Infrastructure
    );
}

#[test]
fn forced_sandboxed_exec_rejects_an_unconfined_zero_exit() {
    let outcome = forced_exec_outcome(
        SANDBOXED_EXEC_NAME,
        zero_exit_with_confinement(ZeroExitEvidence {
            confinement: ExecutionConfinement::Unsandboxed,
            stdout: EXEC_FORCED_SANDBOXED_OUTPUT,
        }),
    );

    assert_eq!(
        outcome.forced_disposition(),
        EvalDisposition::Infrastructure
    );
    assert!(reject_forced_executor_failures(&[outcome]).is_err());
}

#[test]
fn forced_unsandboxed_exec_rejects_a_confined_zero_exit() {
    let outcome = forced_exec_outcome(
        UNSANDBOXED_EXEC_NAME,
        confined_exit(EXEC_FORCED_READ_ONLY_OUTPUT),
    );

    assert_eq!(
        outcome.forced_disposition(),
        EvalDisposition::Infrastructure
    );
}

#[test]
fn forced_exec_tier_rejects_a_truncated_output_capture() {
    let outcome = forced_exec_outcome(
        UNSANDBOXED_EXEC_NAME,
        direct_exec_result(DirectExecEvidence::unsandboxed_truncated(
            EXEC_FORCED_READ_ONLY_OUTPUT,
        )),
    );

    assert_eq!(
        outcome.forced_disposition(),
        EvalDisposition::Infrastructure
    );
}

#[test]
fn forced_exec_tier_rejects_a_missing_stderr_capture() {
    let mut result = direct_exec_result(DirectExecEvidence::successful_with_confinement(
        ExecutionConfinement::Unsandboxed,
        EXEC_FORCED_READ_ONLY_OUTPUT,
    ));
    result
        .as_object_mut()
        .expect("the direct-exec fixture is an object")
        .remove("stderr");
    let outcome = forced_exec_outcome(UNSANDBOXED_EXEC_NAME, result);

    assert_eq!(
        outcome.forced_disposition(),
        EvalDisposition::Infrastructure
    );
}

#[test]
fn forced_exec_tier_rejects_a_truncated_stderr_capture() {
    let mut result = direct_exec_result(DirectExecEvidence::successful_with_confinement(
        ExecutionConfinement::Unsandboxed,
        EXEC_FORCED_READ_ONLY_OUTPUT,
    ));
    result["stderr"]["completeness"] = serde_json::json!("truncated");
    let outcome = forced_exec_outcome(UNSANDBOXED_EXEC_NAME, result);

    assert_eq!(
        outcome.forced_disposition(),
        EvalDisposition::Infrastructure
    );
}

#[test]
fn forced_exec_tier_rejects_a_lossy_stderr_capture() {
    let mut result = direct_exec_result(DirectExecEvidence::successful_with_confinement(
        ExecutionConfinement::Unsandboxed,
        EXEC_FORCED_READ_ONLY_OUTPUT,
    ));
    result["stderr"]["encoding"] = serde_json::json!("lossy_utf8");
    let outcome = forced_exec_outcome(UNSANDBOXED_EXEC_NAME, result);

    assert_eq!(
        outcome.forced_disposition(),
        EvalDisposition::Infrastructure
    );
}

#[test]
fn forced_exec_tier_rejects_a_nonempty_stderr_capture() {
    let mut result = direct_exec_result(DirectExecEvidence::successful_with_confinement(
        ExecutionConfinement::Unsandboxed,
        EXEC_FORCED_READ_ONLY_OUTPUT,
    ));
    result["stderr"]["text"] = serde_json::json!(SYNTHETIC_EXECUTOR_FAILURE);
    let outcome = forced_exec_outcome(UNSANDBOXED_EXEC_NAME, result);

    assert_eq!(
        outcome.forced_disposition(),
        EvalDisposition::Infrastructure
    );
}

#[test]
fn unforced_exec_tier_passes_a_confined_zero_exit() {
    let outcome = natural_exec_outcome(confined_exit(EXEC_NATURAL_OUTPUT));

    assert_eq!(
        outcome.natural_loop_disposition(EvalFamily::Exec),
        EvalDisposition::Pass
    );
}

#[test]
fn unforced_exec_tier_rejects_an_exact_state_mismatch_as_infrastructure() {
    let outcome = natural_exec_outcome(confined_exit(EXEC_NATURAL_OUTPUT));

    assert_eq!(
        outcome.natural_infrastructure_label(EvalFamily::Exec, EvalDisposition::Miss),
        "exact state mismatch"
    );
    assert!(
        reject_natural_executor_failure(&outcome, EvalFamily::Exec, EvalDisposition::Miss).is_err()
    );
}

#[test]
fn unforced_exec_tier_rejects_a_truncated_capture() {
    let mut execution = confined_exit(EXEC_NATURAL_OUTPUT);
    execution["stdout"]["completeness"] = serde_json::json!("truncated");
    let outcome = natural_exec_outcome(execution);

    assert_eq!(
        outcome.natural_loop_disposition(EvalFamily::Exec),
        EvalDisposition::Infrastructure
    );
}

#[test]
fn unforced_exec_tier_rejects_a_lossy_capture() {
    let mut execution = confined_exit(EXEC_NATURAL_OUTPUT);
    execution["stderr"]["encoding"] = serde_json::json!("lossy_utf8");
    let outcome = natural_exec_outcome(execution);

    assert_eq!(
        outcome.natural_loop_disposition(EvalFamily::Exec),
        EvalDisposition::Infrastructure
    );
}

#[test]
fn unforced_exec_tier_reports_a_timed_out_process_as_infrastructure() {
    let outcome = natural_exec_outcome(direct_exec_result(DirectExecEvidence::timed_out()));

    assert_eq!(
        outcome.natural_loop_disposition(EvalFamily::Exec),
        EvalDisposition::Infrastructure
    );
    assert!(
        reject_natural_executor_failure(&outcome, EvalFamily::Exec, EvalDisposition::Pass).is_err()
    );
}

#[test]
fn unforced_exec_tier_reports_a_nonzero_exit_as_infrastructure() {
    let outcome = natural_exec_outcome(direct_exec_result(DirectExecEvidence::nonzero_exit()));

    assert_eq!(
        outcome.natural_loop_disposition(EvalFamily::Exec),
        EvalDisposition::Infrastructure
    );
    assert!(
        reject_natural_executor_failure(&outcome, EvalFamily::Exec, EvalDisposition::Pass).is_err()
    );
}

#[test]
fn unforced_exec_tier_reports_a_supervision_failure_as_infrastructure() {
    let outcome =
        natural_exec_outcome(direct_exec_result(DirectExecEvidence::supervision_failure()));

    assert_eq!(
        outcome.natural_loop_disposition(EvalFamily::Exec),
        EvalDisposition::Infrastructure
    );
}

#[test]
fn unforced_exec_structured_infrastructure_fails_the_job() {
    let outcome =
        natural_exec_outcome(direct_exec_result(DirectExecEvidence::supervision_failure()));

    assert!(
        reject_natural_executor_failure(&outcome, EvalFamily::Exec, EvalDisposition::Pass).is_err()
    );
}

#[test]
fn unforced_exec_tier_reports_sandbox_refusal_as_infrastructure() {
    let outcome = natural_exec_outcome(direct_exec_result(DirectExecEvidence::sandbox_refusal()));

    assert_eq!(
        outcome.natural_loop_disposition(EvalFamily::Exec),
        EvalDisposition::Infrastructure
    );
}

#[test]
fn unforced_exec_tier_scores_the_explicit_approval_cap_as_a_miss() {
    let outcome = CaseOutcome {
        target: None,
        expected_arguments: None,
        execution_completed: true,
        forced_verification_failed: false,
        tool_results: Vec::new(),
        snapshot: CaseSnapshot {
            turn_disposition: SnapshotTurnDisposition::ApprovalCapReached,
            requests: vec![
                denied_unsandboxed_request(Uuid::from_u128(ARBITRARY_EVAL_REQUEST_ID)),
                denied_unsandboxed_request(Uuid::from_u128(ARBITRARY_SECOND_EVAL_REQUEST_ID)),
                denied_unsandboxed_request(Uuid::from_u128(ARBITRARY_THIRD_EVAL_REQUEST_ID)),
            ],
            model_calls: MAX_NATURAL_MODEL_CALLS,
        },
    };

    assert_eq!(
        outcome.natural_loop_disposition(EvalFamily::Exec),
        EvalDisposition::Miss
    );
}

#[test]
fn unforced_exec_tier_keeps_setup_failure_above_the_approval_cap() {
    let mut outcome = natural_exec_outcome(direct_exec_result(
        DirectExecEvidence::sandbox_setup_failure(),
    ));
    outcome.snapshot.requests.push(successful_request(
        Uuid::from_u128(ARBITRARY_SECOND_EVAL_REQUEST_ID),
        UNSANDBOXED_EXEC_NAME,
        serde_json::json!({}),
    ));
    outcome.snapshot.requests.push(successful_request(
        Uuid::from_u128(ARBITRARY_THIRD_EVAL_REQUEST_ID),
        UNSANDBOXED_EXEC_NAME,
        serde_json::json!({}),
    ));
    outcome.snapshot.requests.push(successful_request(
        Uuid::from_u128(ARBITRARY_FOURTH_EVAL_REQUEST_ID),
        UNSANDBOXED_EXEC_NAME,
        serde_json::json!({}),
    ));

    assert_eq!(
        outcome.natural_loop_disposition(EvalFamily::Exec),
        EvalDisposition::Infrastructure
    );
}

#[test]
fn forced_sandboxed_exec_tier_reports_setup_failure_as_infrastructure() {
    let outcome = forced_exec_outcome(
        SANDBOXED_EXEC_NAME,
        direct_exec_result(DirectExecEvidence::sandbox_setup_failure()),
    );

    assert_eq!(
        outcome.forced_disposition(),
        EvalDisposition::Infrastructure
    );
    assert_eq!(outcome.infrastructure_label(), "sandbox setup failed");
}

#[test]
fn forced_sandboxed_exec_setup_failure_fails_the_job() {
    let outcome = forced_exec_outcome(
        SANDBOXED_EXEC_NAME,
        direct_exec_result(DirectExecEvidence::sandbox_setup_failure()),
    );

    assert!(reject_forced_executor_failures(&[outcome]).is_err());
}

#[test]
fn forced_exec_denied_sole_exact_request_is_infrastructure() {
    let mut outcome = forced_exec_outcome(
        UNSANDBOXED_EXEC_NAME,
        zero_exit_with_confinement(ZeroExitEvidence {
            confinement: ExecutionConfinement::Unsandboxed,
            stdout: EXEC_FORCED_READ_ONLY_OUTPUT,
        }),
    );
    outcome.snapshot.requests[0].completed_result_entry_index = None;
    outcome.snapshot.requests[0].attempt_succeeded = false;
    outcome.snapshot.requests[0].attempt_denied = true;
    outcome.tool_results.clear();

    assert_eq!(
        outcome.forced_disposition(),
        EvalDisposition::Infrastructure
    );
    assert!(reject_forced_executor_failures(&[outcome]).is_err());
}

#[test]
fn forced_exec_denied_exact_retry_remains_a_report_only_miss() {
    let mut outcome = forced_exec_outcome(
        UNSANDBOXED_EXEC_NAME,
        zero_exit_with_confinement(ZeroExitEvidence {
            confinement: ExecutionConfinement::Unsandboxed,
            stdout: EXEC_FORCED_READ_ONLY_OUTPUT,
        }),
    );
    let fixture = forced_exec_fixture(UNSANDBOXED_EXEC_NAME);
    outcome.snapshot.requests.push(RequestSnapshot {
        request_id: Uuid::from_u128(ARBITRARY_SECOND_EVAL_REQUEST_ID),
        producing_model_call_id: Uuid::from_u128(ARBITRARY_SECOND_EVAL_MODEL_CALL_ID),
        name: String::from(UNSANDBOXED_EXEC_NAME),
        arguments_text: String::from(fixture.expected_arguments),
        entry_index: ARBITRARY_LATE_RESULT_ENTRY_INDEX,
        completed_result_entry_index: None,
        attempt_succeeded: false,
        attempt_denied: true,
    });

    assert_eq!(outcome.forced_disposition(), EvalDisposition::Miss);
    assert!(reject_forced_executor_failures(&[outcome]).is_ok());
}

#[test]
fn unforced_exec_tier_rejects_an_unconfined_execution() {
    let outcome = natural_exec_outcome(zero_exit_with_confinement(ZeroExitEvidence {
        confinement: ExecutionConfinement::Unsandboxed,
        stdout: "",
    }));

    assert_eq!(
        outcome.natural_loop_disposition(EvalFamily::Exec),
        EvalDisposition::Infrastructure
    );
    assert!(
        reject_natural_executor_failure(&outcome, EvalFamily::Exec, EvalDisposition::Pass).is_err()
    );
}

#[test]
fn every_forced_exec_fixture_is_admitted_by_its_own_dispatch_case() -> EvalResult {
    let [sandboxed, unsandboxed, diagnostics] = EXEC_CASES;

    assert_forced_exec_fixture_is_admitted(sandboxed)?;
    assert_forced_exec_fixture_is_admitted(unsandboxed)?;
    assert_forced_exec_fixture_is_admitted(diagnostics)?;
    Ok(())
}

#[test]
fn workspace_natural_state_rejects_pre_execution_parent_mtime() -> EvalResult {
    let suite = FamilySuite::workspace()?;
    let answer = suite.workspace.path().join(WORKSPACE_ANSWER_PATH);
    let started = current_filesystem_recorded_time()?;
    fs::write(&answer, WORKSPACE_ANSWER)?;
    fs::File::open(suite.workspace.path())?
        .set_times(fs::FileTimes::new().set_modified(UNIX_EPOCH))?;
    suite.executor.record_filesystem_execution_window(
        WRITE_FILE_NAME,
        FilesystemExecutionTimeWindow {
            started,
            finished: current_filesystem_recorded_time()?,
        },
    );
    let snapshot = successful_workspace_natural_snapshot();

    assert!(!suite.natural_state_passed(&snapshot)?);
    Ok(())
}

#[test]
fn workspace_natural_execution_requires_the_full_read_result() {
    let snapshot = successful_workspace_natural_snapshot();
    let tracker = OperationTracker::default();
    record_workspace_read_result(
        &tracker,
        &WORKSPACE_SEED[..WORKSPACE_FORCED_READ_MAX_BYTES],
        true,
    );

    assert!(!workspace_natural_read_result_passed(&snapshot, &tracker));
}

#[test]
fn workspace_natural_execution_accepts_the_exact_full_read_result() {
    let snapshot = successful_workspace_natural_snapshot();
    let tracker = OperationTracker::default();
    record_workspace_read_result(&tracker, WORKSPACE_SEED, false);

    assert!(workspace_natural_read_result_passed(&snapshot, &tracker));
}

#[test]
fn workspace_natural_execution_rejects_an_unknown_read_result_field() {
    let snapshot = successful_workspace_natural_snapshot();
    let tracker = OperationTracker::default();
    tracker.observe_result(
        Uuid::from_u128(ARBITRARY_EVAL_REQUEST_ID),
        &serde_json::json!({
            "path": WORKSPACE_SEED_PATH,
            "content": WORKSPACE_SEED,
            "offset": 0,
            "bytes_read": WORKSPACE_SEED.len(),
            "next_offset": WORKSPACE_SEED.len(),
            "total_bytes": WORKSPACE_SEED.len(),
            "truncated": false,
            "error": "synthetic contradictory field",
            EVAL_RECEIPT_FIELD: SYNTHETIC_EVAL_RECEIPT,
        })
        .to_string(),
    );

    assert!(!workspace_natural_read_result_passed(&snapshot, &tracker));
}

#[test]
fn workspace_natural_execution_accepts_exact_read_and_write_results() {
    let snapshot = successful_workspace_natural_snapshot();
    let tracker = OperationTracker::default();
    record_workspace_read_result(&tracker, WORKSPACE_SEED, false);
    record_workspace_write_result(
        &tracker,
        WORKSPACE_ANSWER_PATH,
        WORKSPACE_ANSWER.len(),
        true,
    );

    assert!(workspace_natural_result_payloads_passed(
        &snapshot, &tracker
    ));
}

#[test]
fn workspace_natural_execution_rejects_inaccurate_write_evidence() {
    let snapshot = successful_workspace_natural_snapshot();
    let tracker = OperationTracker::default();
    record_workspace_read_result(&tracker, WORKSPACE_SEED, false);
    record_workspace_write_result(&tracker, WORKSPACE_SEED_PATH, 0, false);

    assert!(!workspace_natural_result_payloads_passed(
        &snapshot, &tracker
    ));
}

#[test]
fn workspace_natural_execution_rejects_an_unknown_write_result_field() {
    let snapshot = successful_workspace_natural_snapshot();
    let tracker = OperationTracker::default();
    tracker.observe_result(
        Uuid::from_u128(ARBITRARY_SECOND_EVAL_REQUEST_ID),
        &serde_json::json!({
            "path": WORKSPACE_ANSWER_PATH,
            "bytes_written": WORKSPACE_ANSWER.len(),
            "created": true,
            "error": "synthetic contradictory field",
            EVAL_RECEIPT_FIELD: SYNTHETIC_EVAL_RECEIPT,
        })
        .to_string(),
    );

    assert!(!workspace_natural_write_result_passed(&snapshot, &tracker));
}

#[cfg(unix)]
#[test]
fn exec_natural_created_file_gate_rejects_changed_ownership() -> EvalResult {
    let (
        workspace,
        _entries,
        _times,
        expected_identities,
        _attributes,
        _inode_flags,
        _execution_window,
    ) = prepared_exec_natural_workspace()?;
    let target = Path::new(EXEC_RESULT_PATH);
    let mut actual_identities = workspace_entry_identities(workspace.path())?;
    let changed_group_id = actual_identities[target].group_id.wrapping_add(1);
    actual_identities
        .get_mut(target)
        .expect("the Exec result has a filesystem identity")
        .group_id = changed_group_id;

    assert!(!created_entry_identity_matches_workspace(
        &actual_identities,
        &expected_identities,
        target,
    ));
    Ok(())
}

#[cfg(unix)]
#[test]
fn exec_natural_created_file_gate_rejects_a_different_device() -> EvalResult {
    let (
        workspace,
        _entries,
        _times,
        expected_identities,
        _attributes,
        _inode_flags,
        _execution_window,
    ) = prepared_exec_natural_workspace()?;
    let target = Path::new(EXEC_RESULT_PATH);
    let mut actual_identities = workspace_entry_identities(workspace.path())?;
    actual_identities
        .get_mut(target)
        .expect("the Exec result has a filesystem identity")
        .device = expected_identities[Path::new("")].device.wrapping_add(1);

    assert!(!created_entry_identity_matches_workspace(
        &actual_identities,
        &expected_identities,
        target,
    ));
    Ok(())
}

#[cfg(unix)]
#[test]
fn exec_natural_created_file_gate_rejects_collateral_write_permissions() -> EvalResult {
    let (workspace, entries, times, identities, attributes, inode_flags, execution_window) =
        prepared_exec_natural_workspace()?;
    fs::set_permissions(
        workspace.path().join(EXEC_RESULT_PATH),
        fs::Permissions::from_mode(EXEC_PERMISSIVE_CREATION_MODE),
    )?;

    assert!(!exec_natural_entries_match(
        workspace.path(),
        &entries,
        &times,
        &identities,
        &attributes,
        &inode_flags,
        Some(execution_window),
    )?);
    Ok(())
}

#[test]
fn forced_direct_exec_workspace_accepts_the_unchanged_seed() -> EvalResult {
    let (workspace, seed_entries, seed_modified_times, seed_entry_identities) =
        prepared_exec_seed_workspace()?;
    let seed_extended_attributes = workspace_extended_attributes(workspace.path())?;

    assert!(exec_workspace_matches_seed(
        workspace.path(),
        &seed_entries,
        &seed_modified_times,
        &seed_entry_identities,
        &seed_extended_attributes,
        &workspace_inode_flags(workspace.path())?,
    )?);
    Ok(())
}

#[cfg(target_os = "linux")]
#[test]
fn forced_direct_exec_workspace_rejects_inode_flag_drift() -> EvalResult {
    let (workspace, seed_entries, seed_modified_times, seed_entry_identities) =
        prepared_exec_seed_workspace()?;
    let seed_extended_attributes = workspace_extended_attributes(workspace.path())?;
    let seed_inode_flags = workspace_inode_flags(workspace.path())?;
    let seed = fs::File::open(workspace.path().join("Cargo.toml"))?;
    let flags = rustix::fs::ioctl_getflags(&seed)?;
    rustix::fs::ioctl_setflags(&seed, flags | rustix::fs::IFlags::NOATIME)?;

    assert!(!exec_workspace_matches_seed(
        workspace.path(),
        &seed_entries,
        &seed_modified_times,
        &seed_entry_identities,
        &seed_extended_attributes,
        &seed_inode_flags,
    )?);
    Ok(())
}

#[test]
fn forced_direct_exec_workspace_rejects_a_mutated_seed_file() -> EvalResult {
    let (workspace, seed_entries, seed_modified_times, seed_entry_identities) =
        prepared_exec_seed_workspace()?;
    let seed_extended_attributes = workspace_extended_attributes(workspace.path())?;
    fs::write(workspace.path().join("src/lib.rs"), "pub fn drifted() {}\n")?;

    assert!(!exec_workspace_matches_seed(
        workspace.path(),
        &seed_entries,
        &seed_modified_times,
        &seed_entry_identities,
        &seed_extended_attributes,
        &workspace_inode_flags(workspace.path())?,
    )?);
    Ok(())
}

#[test]
fn forced_direct_exec_workspace_rejects_a_collateral_path() -> EvalResult {
    let (workspace, seed_entries, seed_modified_times, seed_entry_identities) =
        prepared_exec_seed_workspace()?;
    let seed_extended_attributes = workspace_extended_attributes(workspace.path())?;
    fs::write(
        workspace.path().join("collateral.txt"),
        "collateral fixture\n",
    )?;

    assert!(!exec_workspace_matches_seed(
        workspace.path(),
        &seed_entries,
        &seed_modified_times,
        &seed_entry_identities,
        &seed_extended_attributes,
        &workspace_inode_flags(workspace.path())?,
    )?);
    Ok(())
}

#[cfg(unix)]
#[test]
fn forced_direct_exec_workspace_rejects_byte_identical_seed_replacement() -> EvalResult {
    let (workspace, seed_entries, seed_modified_times, seed_entry_identities) =
        prepared_exec_seed_workspace()?;
    let seed_extended_attributes = workspace_extended_attributes(workspace.path())?;
    replace_exec_seed_file_byte_identically(workspace.path(), &seed_modified_times)?;

    assert_eq!(workspace_entries(workspace.path())?, seed_entries);
    assert_eq!(
        workspace_modified_times(workspace.path())?,
        seed_modified_times
    );
    assert_ne!(
        workspace_entry_identities(workspace.path())?,
        seed_entry_identities
    );
    assert!(!exec_workspace_matches_seed(
        workspace.path(),
        &seed_entries,
        &seed_modified_times,
        &seed_entry_identities,
        &seed_extended_attributes,
        &workspace_inode_flags(workspace.path())?,
    )?);
    Ok(())
}

#[cfg(target_os = "linux")]
#[test]
fn forced_direct_exec_workspace_rejects_extended_attribute_drift() -> EvalResult {
    let (workspace, seed_entries, seed_modified_times, seed_entry_identities) =
        prepared_exec_seed_workspace()?;
    let seed_extended_attributes = workspace_extended_attributes(workspace.path())?;
    rustix::fs::setxattr(
        workspace.path().join("Cargo.toml"),
        SYNTHETIC_UNEXPECTED_XATTR_NAME,
        SYNTHETIC_UNEXPECTED_XATTR_VALUE,
        rustix::fs::XattrFlags::CREATE,
    )?;

    assert!(!exec_workspace_matches_seed(
        workspace.path(),
        &seed_entries,
        &seed_modified_times,
        &seed_entry_identities,
        &seed_extended_attributes,
        &workspace_inode_flags(workspace.path())?,
    )?);
    Ok(())
}

#[test]
fn exec_natural_state_accepts_only_the_requested_output_addition() -> EvalResult {
    let (
        workspace,
        seed_entries,
        seed_modified_times,
        seed_entry_identities,
        seed_extended_attributes,
        seed_inode_flags,
        execution_window,
    ) = prepared_exec_natural_workspace()?;

    assert!(exec_natural_entries_match(
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

#[cfg(target_os = "linux")]
#[test]
fn exec_natural_state_rejects_output_inode_flag_drift() -> EvalResult {
    let (
        workspace,
        seed_entries,
        seed_modified_times,
        seed_entry_identities,
        seed_extended_attributes,
        seed_inode_flags,
        execution_window,
    ) = prepared_exec_natural_workspace()?;
    let output = fs::File::open(workspace.path().join(EXEC_RESULT_PATH))?;
    let flags = rustix::fs::ioctl_getflags(&output)?;
    rustix::fs::ioctl_setflags(&output, flags | rustix::fs::IFlags::NOATIME)?;

    assert!(!exec_natural_entries_match(
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

#[cfg(target_os = "linux")]
#[test]
fn exec_natural_state_rejects_seed_inode_flag_drift() -> EvalResult {
    let (
        workspace,
        seed_entries,
        seed_modified_times,
        seed_entry_identities,
        seed_extended_attributes,
        seed_inode_flags,
        execution_window,
    ) = prepared_exec_natural_workspace()?;
    let seed = fs::File::open(workspace.path().join("Cargo.toml"))?;
    let flags = rustix::fs::ioctl_getflags(&seed)?;
    rustix::fs::ioctl_setflags(&seed, flags | rustix::fs::IFlags::NOATIME)?;

    assert!(!exec_natural_entries_match(
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
fn exec_natural_state_rejects_out_of_window_output_times() -> EvalResult {
    let (
        workspace,
        seed_entries,
        seed_modified_times,
        seed_entry_identities,
        seed_extended_attributes,
        seed_inode_flags,
        execution_window,
    ) = prepared_exec_natural_workspace()?;
    fs::File::open(workspace.path().join(EXEC_RESULT_PATH))?
        .set_times(fs::FileTimes::new().set_modified(UNIX_EPOCH))?;

    assert!(!exec_natural_entries_match(
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
fn exec_natural_state_rejects_out_of_window_parent_times() -> EvalResult {
    let (
        workspace,
        seed_entries,
        seed_modified_times,
        seed_entry_identities,
        seed_extended_attributes,
        seed_inode_flags,
        execution_window,
    ) = prepared_exec_natural_workspace()?;
    fs::File::open(workspace.path())?.set_times(fs::FileTimes::new().set_modified(UNIX_EPOCH))?;

    assert!(!exec_natural_entries_match(
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
fn exec_natural_state_rejects_a_mutated_seed_file() -> EvalResult {
    let (
        workspace,
        seed_entries,
        seed_modified_times,
        seed_entry_identities,
        seed_extended_attributes,
        seed_inode_flags,
        execution_window,
    ) = prepared_exec_natural_workspace()?;
    fs::write(
        workspace.path().join("Cargo.toml"),
        "[package]\nname = \"collateral-mutation\"\n",
    )?;

    assert!(!exec_natural_entries_match(
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
fn exec_natural_state_rejects_a_collateral_addition() -> EvalResult {
    let (
        workspace,
        seed_entries,
        seed_modified_times,
        seed_entry_identities,
        seed_extended_attributes,
        seed_inode_flags,
        execution_window,
    ) = prepared_exec_natural_workspace()?;
    fs::write(
        workspace.path().join("collateral.txt"),
        "collateral fixture\n",
    )?;

    assert!(!exec_natural_entries_match(
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
fn exec_natural_state_rejects_an_oversized_sparse_collateral_file() -> EvalResult {
    let (
        workspace,
        seed_entries,
        seed_modified_times,
        seed_entry_identities,
        seed_extended_attributes,
        seed_inode_flags,
        execution_window,
    ) = prepared_exec_natural_workspace()?;
    fs::File::create(workspace.path().join("oversized-collateral.txt"))?
        .set_len((MAX_WORKSPACE_READ_BYTES + 1) as u64)?;

    assert!(!exec_natural_entries_match(
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

#[cfg(target_os = "linux")]
#[test]
fn exec_natural_state_rejects_root_extended_attribute_drift() -> EvalResult {
    let (
        workspace,
        seed_entries,
        seed_modified_times,
        seed_entry_identities,
        seed_extended_attributes,
        seed_inode_flags,
        execution_window,
    ) = prepared_exec_natural_workspace()?;
    let root = Path::new("");
    rustix::fs::setxattr(
        workspace.path(),
        SYNTHETIC_UNEXPECTED_XATTR_NAME,
        SYNTHETIC_UNEXPECTED_XATTR_VALUE,
        rustix::fs::XattrFlags::CREATE,
    )?;

    assert_ne!(
        workspace_extended_attributes(workspace.path())?[root],
        seed_extended_attributes[root]
    );
    assert!(!exec_natural_entries_match(
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

#[cfg(unix)]
#[test]
fn exec_natural_state_rejects_byte_identical_seed_replacement() -> EvalResult {
    let (
        workspace,
        seed_entries,
        seed_modified_times,
        seed_entry_identities,
        seed_extended_attributes,
        seed_inode_flags,
        execution_window,
    ) = prepared_exec_natural_workspace()?;
    replace_exec_seed_file_byte_identically(workspace.path(), &seed_modified_times)?;

    assert!(!exec_natural_entries_match(
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
fn exec_result_inspection_rejects_a_directory() -> EvalResult {
    let (
        workspace,
        seed_entries,
        seed_modified_times,
        seed_entry_identities,
        seed_extended_attributes,
        seed_inode_flags,
        execution_window,
    ) = prepared_exec_natural_workspace()?;
    fs::remove_file(workspace.path().join(EXEC_RESULT_PATH))?;
    fs::create_dir(workspace.path().join(EXEC_RESULT_PATH))?;

    assert!(!exec_natural_entries_match(
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

#[cfg(target_os = "linux")]
#[test]
fn exec_result_inspection_rejects_a_fifo_without_opening_it() -> EvalResult {
    let (
        workspace,
        seed_entries,
        seed_modified_times,
        seed_entry_identities,
        seed_extended_attributes,
        seed_inode_flags,
        execution_window,
    ) = prepared_exec_natural_workspace()?;
    let result = workspace.path().join(EXEC_RESULT_PATH);
    fs::remove_file(&result)?;
    rustix::fs::mkfifoat(
        rustix::fs::CWD,
        &result,
        rustix::fs::Mode::RUSR | rustix::fs::Mode::WUSR,
    )?;

    assert!(!exec_natural_entries_match(
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
fn web_natural_execution_accepts_exact_search_and_fetch_results() -> EvalResult {
    let snapshot = successful_web_natural_snapshot()?;
    let tracker = OperationTracker::default();
    record_web_natural_results(
        &tracker,
        exact_web_search_result(),
        exact_web_fetch_result(),
    );

    assert!(web_natural_result_payloads_passed(&snapshot, &tracker));
    Ok(())
}

#[test]
fn web_natural_execution_rejects_an_empty_search_result() -> EvalResult {
    let snapshot = successful_web_natural_snapshot()?;
    let tracker = OperationTracker::default();
    let empty_search = serde_json::json!({
        "results": [],
        "truncated": false,
        EVAL_RECEIPT_FIELD: SYNTHETIC_EVAL_RECEIPT,
    });
    record_web_natural_results(&tracker, empty_search, exact_web_fetch_result());

    assert!(!web_natural_result_payloads_passed(&snapshot, &tracker));
    Ok(())
}

#[test]
fn web_natural_execution_rejects_corrupted_fetch_metadata() -> EvalResult {
    let snapshot = successful_web_natural_snapshot()?;
    let tracker = OperationTracker::default();
    let corrupted_fetch = serde_json::json!({
        "url": WEB_ORIGIN,
        "status": 201,
        "content_type": "application/octet-stream",
        "body": WEB_FETCH_BODY,
        "truncated": false,
        EVAL_RECEIPT_FIELD: SYNTHETIC_EVAL_RECEIPT,
    });
    record_web_natural_results(&tracker, exact_web_search_result(), corrupted_fetch);

    assert!(!web_natural_result_payloads_passed(&snapshot, &tracker));
    Ok(())
}
