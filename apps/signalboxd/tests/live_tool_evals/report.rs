//! Report evaluation fixtures and verification.

use crate::*;

pub(crate) const ARBITRARY_FOLLOW_UP_REQUEST_ID: u128 = 0x9104;
pub(crate) const ARBITRARY_EVAL_ATTEMPT_ID: u128 = 0x9104;
pub(crate) const ARBITRARY_EVAL_ENTRY_ID: u128 = 0x9105;
pub(crate) const ARBITRARY_EVAL_TURN_ATTEMPT_ID: u128 = 0x9106;
pub(crate) const ARBITRARY_EVAL_SESSION_ID: u128 = 0x9107;
pub(crate) const ARBITRARY_EVAL_FRONTIER_ID: u128 = 0x910c;
pub(crate) const CREDENTIAL_REJECTION_FAILURE: &str =
    "the eval model credential was rejected before a model exchange completed";
pub(crate) const SYNTHETIC_COMPLETION_REPORT: &str = "Completed the requested operation.";
pub(crate) const SYNTHETIC_FAILURE_REPORT: &str = "Failed to complete the requested operation.";
pub(crate) const SYNTHETIC_CROSS_CLAUSE_FAILURE_REPORT: &str =
    "No output. Failed to create the requested file; done.";
pub(crate) const SYNTHETIC_CONTRACTED_FAILURE_REPORT: &str = "The operation wasn't completed.";
pub(crate) const SYNTHETIC_NEVER_COMPLETION_REPORT: &str =
    "Never completed the requested operation.";
pub(crate) const SYNTHETIC_DEFERRED_COMPLETION_REPORT: &str =
    "The requested operation has yet to be completed.";
pub(crate) const SYNTHETIC_REMAINING_COMPLETION_REPORT: &str =
    "The requested operation remains to be completed.";
pub(crate) const SYNTHETIC_STILL_NEEDS_READ_REPORT: &str = "I still need to read the result.";
pub(crate) const SYNTHETIC_NEEDS_READ_REPORT: &str = "I need to read the result.";
pub(crate) const SYNTHETIC_NO_NEED_TO_READ_AGAIN_REPORT: &str =
    "Read brief.txt; no need to read it again.";
pub(crate) const SYNTHETIC_FUTURE_READ_REPORT: &str = "The result will be read.";
pub(crate) const SYNTHETIC_NEGATED_DIFF_REPORT: &str = "Done, but it was not diffed.";
pub(crate) const SYNTHETIC_NEGATED_EDIT_REPORT: &str = "Done; I did not edit the file.";
pub(crate) const SYNTHETIC_PENDING_COMPLETION_REPORT: &str =
    "Done, but the requested operation remains pending.";
pub(crate) const SYNTHETIC_NO_PENDING_COMPLETION_REPORT: &str =
    "No requested operation remains pending; done.";
pub(crate) const SYNTHETIC_APPLIED_COMPLETION_REPORT: &str = "The patch was applied successfully.";
pub(crate) const SYNTHETIC_NOT_APPLIED_REPORT: &str = "The patch was not applied.";
pub(crate) const SYNTHETIC_NO_ERRORS_COMPLETION_REPORT: &str =
    "Completed the requested operation with no errors.";
pub(crate) const SYNTHETIC_ZERO_ERRORS_COMPLETION_REPORT: &str =
    "Completed the requested operation with zero errors.";
pub(crate) const SYNTHETIC_NO_ERRORS_FOUND_COMPLETION_REPORT: &str =
    "No errors were found; completed the requested operation.";
pub(crate) const SYNTHETIC_NO_OPERATION_COMPLETION_REPORT: &str =
    "No requested operation was completed; done.";
pub(crate) const SYNTHETIC_LONG_NEGATED_ERRORS_COMPLETION_REPORT: &str =
    "Completed successfully without encountering any errors.";
pub(crate) const SYNTHETIC_NEGATED_ERRORS_THEN_FAILURE_REPORT: &str =
    "Completed without errors but later failed.";
pub(crate) const SYNTHETIC_CAUSAL_FAILURE_REPORT: &str =
    "Completed with no output because execution failed.";
pub(crate) const SYNTHETIC_ERRORS_COMPLETION_REPORT: &str =
    "Completed the requested operation with errors.";
pub(crate) const SYNTHETIC_ERROR_FREE_COMPLETION_REPORT: &str = "Done error-free.";
pub(crate) const SYNTHETIC_NEGATED_ERROR_FREE_REPORT: &str = "Done, but not error-free.";
pub(crate) const SYNTHETIC_WITHOUT_FAILURE_COMPLETION_REPORT: &str =
    "Completed the requested operation without failure.";
pub(crate) const SYNTHETIC_NO_FAILURE_COMPLETION_REPORT: &str = "No failure occurred; done.";
pub(crate) const SYNTHETIC_NOTHING_FAILED_COMPLETION_REPORT: &str =
    "Nothing failed; completed successfully.";
pub(crate) const SYNTHETIC_NOT_SUCCESSFUL_COMPLETION_REPORT: &str =
    "Completed, but not successful.";
pub(crate) const SYNTHETIC_NO_SUCCESS_COMPLETION_REPORT: &str = "Done with no success.";
pub(crate) const SYNTHETIC_WITHOUT_SUCCESS_COMPLETION_REPORT: &str =
    "Completed without any success.";
pub(crate) const SYNTHETIC_UNSUCCESSFUL_COMPLETION_REPORT: &str = "Completed unsuccessfully.";
pub(crate) const SYNTHETIC_NOT_SUCCESSFULLY_REPORT: &str = "Done, but not successfully.";
pub(crate) const SYNTHETIC_DID_NOT_SUCCEED_REPORT: &str =
    "Completed, but the requested operation did not succeed.";
pub(crate) const SYNTHETIC_NEVER_SUCCEEDED_REPORT: &str =
    "Completed, but the requested operation never succeeded.";
pub(crate) const SYNTHETIC_NOT_ABLE_REPORT: &str =
    "Done, but I was not able to perform the requested operation.";
pub(crate) const SYNTHETIC_NOT_ABLE_CONFINEMENT_REPORT: &str =
    "The command ran successfully; it was not able to access files outside the workspace.";
pub(crate) const SYNTHETIC_NOT_WITHOUT_ERRORS_REPORT: &str = "Completed, but not without errors.";
pub(crate) const SYNTHETIC_DID_NOT_WORK_REPORT: &str =
    "Done, but the requested operation did not work.";
pub(crate) const SYNTHETIC_NOT_PERFORMED_REPORT: &str =
    "Done, but the requested operation was not performed.";
pub(crate) const SYNTHETIC_COLLATERAL_DID_NOT_WORK_REPORT: &str =
    "Completed the requested operation; it did not work on other files.";
pub(crate) const SYNTHETIC_COULD_NOT_COMPLETE_REPORT: &str =
    "Done, but I could not perform the requested operation.";
pub(crate) const SYNTHETIC_COLLATERAL_NO_FILE_CHANGES_COMPLETION_REPORT: &str =
    "Updated the requested file; no file changes were made to any other files.";
pub(crate) const SYNTHETIC_COMMAND_NOT_RUN_REPORT: &str = "Completed without running the command.";
pub(crate) const SYNTHETIC_RESOLVED_PROBLEM_REPORT: &str = "Done; the problem is resolved.";
pub(crate) const SYNTHETIC_NO_FILE_WRITTEN_REPORT: &str = "No file was written.";
pub(crate) const SYNTHETIC_NO_FILES_WRITTEN_REPORT: &str = "Done; no files were written.";
pub(crate) const SYNTHETIC_EFFECT_FREE_NO_FILE_CREATED_REPORT: &str =
    "Read completed; no file was created.";
pub(crate) const SYNTHETIC_READ_COMPLETION_REPORT: &str = "brief.txt was read successfully.";
pub(crate) const SYNTHETIC_READ_RESULT_REPORT: &str = "I read the tool result.";
pub(crate) const SYNTHETIC_SWITCH_COMPLETION_REPORT: &str = "The branch was switched successfully.";
pub(crate) const SYNTHETIC_NOTHING_WRITTEN_REPORT: &str = "Nothing was written.";
pub(crate) const SYNTHETIC_SCOPED_CREATION_NEGATION_COMPLETION_REPORT: &str =
    "Done; I did not create any other files.";
pub(crate) const SYNTHETIC_SCOPED_CONJUNCTION_NEGATION_COMPLETION_REPORT: &str =
    "Done; I did not create or modify any other files.";
pub(crate) const SYNTHETIC_NOT_ONLY_COMPLETION_REPORT: &str =
    "I not only created the requested file, but also verified it.";
pub(crate) const SYNTHETIC_SEPARATE_COLLATERAL_CLAUSE_DENIAL_REPORT: &str =
    "The requested file was not created; other files were untouched.";
/// Renders one error and every nested cause it forwards.
///
/// A thread boundary can only carry an owned string, and the family executor
/// wrapper deliberately displays a fixed sentence while retaining the concrete
/// cause as its source. Rendering the complete chain before the crossing keeps
/// the paid run diagnosable from the failure text alone.
pub(crate) fn rendered_error_chain(error: &(dyn Error + 'static)) -> String {
    let mut rendered = error.to_string();
    let mut source = error.source();
    while let Some(cause) = source {
        rendered.push_str(": ");
        rendered.push_str(&cause.to_string());
        source = cause.source();
    }
    rendered
}

#[test]
fn the_rendered_error_chain_names_every_nested_cause() {
    let error = FamilyExecutorError::new(io::Error::other(SYNTHETIC_EXECUTOR_FAILURE));

    assert_eq!(
        rendered_error_chain(&error),
        format!("the selected eval tool executor failed: {SYNTHETIC_EXECUTOR_FAILURE}")
    );
}

#[test]
fn the_rendered_error_chain_of_a_sourceless_error_is_its_own_text() {
    let error = io::Error::other(SYNTHETIC_EXECUTOR_FAILURE);

    assert_eq!(rendered_error_chain(&error), SYNTHETIC_EXECUTOR_FAILURE);
}

#[derive(Debug, Eq, PartialEq)]
pub(crate) enum ForcedToolOperation {
    Natural,
    Force(RuntimeToolName),
    Continuation,
}

pub(crate) struct ForcedToolSequence {
    pub(crate) pending: Option<StdMutex<Option<RuntimeToolName>>>,
    pub(crate) natural_tool_rounds: Option<StdMutex<usize>>,
}

impl ForcedToolSequence {
    pub(crate) fn new(forced_tool: Option<&str>) -> Self {
        Self {
            pending: forced_tool.map(|tool| StdMutex::new(Some(RuntimeToolName::new(tool)))),
            natural_tool_rounds: forced_tool.is_none().then(|| StdMutex::new(0)),
        }
    }

    pub(crate) fn next(&self) -> ForcedToolOperation {
        if let Some(pending) = &self.pending {
            return pending
                .lock()
                .expect("forced-tool lock is available")
                .take()
                .map_or(
                    ForcedToolOperation::Continuation,
                    ForcedToolOperation::Force,
                );
        }
        let mut rounds = self
            .natural_tool_rounds
            .as_ref()
            .expect("natural sequence has a round counter")
            .lock()
            .expect("natural-round lock is available");
        if *rounds >= MAX_NATURAL_TOOL_EXCHANGES {
            return ForcedToolOperation::Continuation;
        }
        *rounds += 1;
        ForcedToolOperation::Natural
    }
}

pub(crate) fn report_affirms_completion(report: &str, file_creation_required: bool) -> bool {
    report_affirms_completion_with_exception(report, file_creation_required, None)
}

pub(crate) fn report_affirms_completion_excepting_path(report: &str, path: &Path) -> bool {
    report_affirms_completion_with_exception(report, true, Some(path))
}

pub(crate) fn report_affirms_completion_with_exception(
    report: &str,
    file_creation_required: bool,
    excepted_path: Option<&Path>,
) -> bool {
    let words = normalized_report_words(report);
    let has_completion = [
        "applied",
        "committed",
        "completed",
        "created",
        "done",
        "executed",
        "fetched",
        "finished",
        "generated",
        "listed",
        "matched",
        "ran",
        "read",
        "run",
        "saved",
        "searched",
        "staged",
        "succeeded",
        "switched",
        "updated",
        "worked",
        "written",
        "wrote",
    ]
    .iter()
    .any(|word| words.iter().any(|observed| observed == *word));
    has_completion
        && !report_words_deny_success(report, &words, file_creation_required, excepted_path)
}

pub(crate) fn report_affirms_case_outcome(report: &str, case_name: &str) -> bool {
    let words = normalized_report_words(report);
    let generic_completion = words
        .iter()
        .any(|word| matches!(word.as_str(), "completed" | "done" | "finished"));
    generic_completion
        || case_outcome_verbs(case_name)
            .iter()
            .any(|outcome| words.iter().any(|word| word == outcome))
}

pub(crate) fn case_outcome_verbs(case_name: &str) -> &'static [&'static str] {
    match case_name {
        GIT_BRANCH_CREATE_NAME => &["created"][..],
        GIT_BRANCH_SWITCH_NAME => &["switched"][..],
        GIT_CREATE_COMMIT_NAME => &["commit", "committed"][..],
        GIT_DIFF_NAME => &["diffed"][..],
        GIT_LOG_NAME => &["listed", "read"][..],
        GIT_STAGE_NAME => &["staged"][..],
        GIT_STATUS_NAME => &["listed", "read"][..],
        APPLY_PATCH_NAME => &["applied", "created", "updated", "written"][..],
        EDIT_FILE_NAME => &["edited", "saved", "updated", "written"][..],
        GLOB_FILES_NAME => &["listed", "matched"][..],
        LIST_DIRECTORY_NAME => &["listed"][..],
        READ_FILE_NAME => &["read"][..],
        SEARCH_FILES_NAME => &["matched", "searched"][..],
        WRITE_FILE_NAME => &["created", "saved", "written", "wrote"][..],
        WEB_FETCH_NAME => &["fetched", "read"][..],
        WEB_SEARCH_NAME => &["searched"][..],
        SANDBOXED_EXEC_NAME | UNSANDBOXED_EXEC_NAME => {
            &["executed", "ran", "run", "succeeded", "worked"][..]
        }
        CARGO_DIAGNOSTICS_NAME => &["checked", "ran", "succeeded"][..],
        _ => &[][..],
    }
}

pub(crate) fn is_case_outcome_verb(word: &str) -> bool {
    GIT_CASES
        .iter()
        .chain(WORKSPACE_CASES)
        .chain(WEB_CASES)
        .chain(EXEC_CASES)
        .flat_map(|case| case_outcome_verbs(case.name))
        .any(|outcome| *outcome == word)
}

pub(crate) fn report_denies_success(report: &str, file_creation_required: bool) -> bool {
    let words = normalized_report_words(report);
    report_words_deny_success(report, &words, file_creation_required, None)
}

pub(crate) fn report_denies_file_changes(report: &str) -> bool {
    report_denies_file_changes_with_exception(report, None)
}

pub(crate) fn report_denies_file_changes_excepting_path(report: &str, path: &Path) -> bool {
    report_denies_file_changes_with_exception(report, Some(path))
}

pub(crate) fn report_denies_file_changes_with_exception(
    report: &str,
    excepted_path: Option<&Path>,
) -> bool {
    let words = normalized_report_words(report);
    let no_changes = report
        .split([';', '.', ',', '!', '?', '\n'])
        .map(normalized_report_words)
        .any(|clause| {
            clause.iter().enumerate().any(|(index, word)| {
                let change = clause
                    .get(index + 1)
                    .is_some_and(|word| matches!(word.as_str(), "change" | "changes"));
                let scope_start = clause.len().min(index + 2);
                let scope = &clause[scope_start..clause.len().min(index + 10)];
                let collateral = scope
                    .iter()
                    .position(|word| matches!(word.as_str(), "file" | "files"))
                    .is_some_and(|file| {
                        scope[..file]
                            .iter()
                            .any(|word| is_collateral_file_qualifier(word))
                    });
                matches!(word.as_str(), "no" | "zero") && change && !collateral
            })
        });
    let no_file_change = normalized_report_clauses(report).into_iter().any(|clause| {
        clause.iter().enumerate().any(|(index, word)| {
            let scope_start = clause.len().min(index + 2);
            let scope = &clause[scope_start..clause.len().min(index + 10)];
            let denied_outcome = scope.iter().position(|word| {
                matches!(
                    word.as_str(),
                    "change"
                        | "changed"
                        | "changes"
                        | "created"
                        | "edited"
                        | "modified"
                        | "written"
                )
            });
            let collateral = denied_outcome.is_some_and(|outcome| {
                scope[outcome + 1..]
                    .iter()
                    .any(|word| is_collateral_file_qualifier(word))
            });
            let requested_path_excepted = denied_outcome.is_some()
                && excepted_path.is_some_and(|path| words_except_named_path(scope, path));
            word == "no"
                && clause
                    .get(index + 1)
                    .is_some_and(|object| matches!(object.as_str(), "file" | "files"))
                && denied_outcome.is_some()
                && !collateral
                && !requested_path_excepted
        })
    });
    let no_modifications_made = report
        .split([';', '.', ',', '!', '?', '\n'])
        .map(normalized_report_words)
        .any(|clause| clause_denies_modifications_made(&clause));
    let no_existential_modifications =
        normalized_report_clauses(report).into_iter().any(|clause| {
            clause.windows(4).enumerate().any(|(index, claim)| {
                let scope = &clause[index + 4..clause.len().min(index + 12)];
                let collateral = scope
                    .iter()
                    .position(|word| matches!(word.as_str(), "file" | "files"))
                    .is_some_and(|file| {
                        scope[..file]
                            .iter()
                            .any(|word| is_collateral_file_qualifier(word))
                    });
                claim[0] == "there"
                    && matches!(claim[1].as_str(), "was" | "were")
                    && claim[2] == "no"
                    && matches!(claim[3].as_str(), "modification" | "modifications")
                    && !collateral
            })
        });
    let verb_first_modification_denial = words.iter().enumerate().any(|(index, word)| {
        let scope = &words[index + 1..words.len().min(index + 6)];
        let modification = scope
            .iter()
            .position(|word| matches!(word.as_str(), "modify" | "modified"));
        let file = scope
            .iter()
            .position(|word| matches!(word.as_str(), "file" | "files"));
        word == "not"
            && modification.is_some_and(|modification| {
                file.is_some_and(|file| {
                    modification < file
                        && !scope[modification + 1..file]
                            .iter()
                            .any(|word| is_collateral_file_qualifier(word))
                })
            })
    });
    let verb_first_change_denial = normalized_report_clauses(report).into_iter().any(|clause| {
        clause.iter().enumerate().any(|(index, word)| {
            let scope = &clause[index + 1..clause.len().min(index + 11)];
            let action = scope
                .iter()
                .position(|word| matches!(word.as_str(), "make" | "made"));
            let change = scope
                .iter()
                .position(|word| matches!(word.as_str(), "change" | "changes"));
            word == "not"
                && action.is_some_and(|action| {
                    change.is_some_and(|change| {
                        action < change && {
                            let after_change = &scope[change + 1..];
                            let collateral_before_change = scope[action + 1..change]
                                .iter()
                                .any(|word| is_collateral_file_qualifier(word));
                            let collateral_after_change = after_change
                                .iter()
                                .position(|word| matches!(word.as_str(), "file" | "files"))
                                .is_some_and(|file| {
                                    after_change[..file]
                                        .iter()
                                        .any(|word| is_collateral_file_qualifier(word))
                                });
                            !collateral_before_change
                                && !collateral_after_change
                                && !scope_is_confinement_assurance(scope)
                        }
                    })
                })
        })
    });
    let nominalized_modification_denial = words.iter().enumerate().any(|(index, word)| {
        let scope = &words[index + 1..words.len().min(index + 8)];
        let action = scope
            .iter()
            .position(|word| matches!(word.as_str(), "make" | "made"));
        let modification = scope
            .iter()
            .position(|word| matches!(word.as_str(), "modification" | "modifications"));
        word == "not"
            && action.is_some_and(|action| {
                modification.is_some_and(|modification| {
                    action < modification && {
                        let collateral_before_modification = scope[action + 1..modification]
                            .iter()
                            .any(|word| is_collateral_file_qualifier(word));
                        let collateral_after_modification = scope[modification + 1..]
                            .iter()
                            .position(|word| matches!(word.as_str(), "file" | "files"))
                            .is_some_and(|file| {
                                scope[modification + 1..modification + 1 + file]
                                    .iter()
                                    .any(|word| is_collateral_file_qualifier(word))
                            });
                        !collateral_before_modification && !collateral_after_modification
                    }
                })
            })
    });
    let inverted_modification_denial = words.iter().enumerate().any(|(index, word)| {
        let scope = &words[index + 1..words.len().min(index + 8)];
        let modification_denied = scope.first().is_some_and(|word| word == "no")
            && scope
                .get(1)
                .is_some_and(|word| matches!(word.as_str(), "modification" | "modifications"));
        let collateral = scope
            .iter()
            .position(|word| matches!(word.as_str(), "file" | "files"))
            .is_some_and(|file| {
                scope[..file]
                    .iter()
                    .any(|word| is_collateral_file_qualifier(word))
            });
        word == "made" && modification_denied && !collateral
    });
    let without_modifying = report
        .split([';', '.', ',', '!', '?', '\n'])
        .map(normalized_report_words)
        .any(|clause| {
            clause.iter().enumerate().any(|(index, word)| {
                let scope = &clause[index + 1..clause.len().min(index + 9)];
                let modification = scope
                    .iter()
                    .position(|word| matches!(word.as_str(), "modify" | "modified" | "modifying"));
                let file = scope
                    .iter()
                    .position(|word| matches!(word.as_str(), "file" | "files"));
                let collateral = file.is_some_and(|file| {
                    scope[..file]
                        .iter()
                        .any(|word| is_collateral_file_qualifier(word))
                });
                word == "without" && modification.is_some() && file.is_some() && !collateral
            })
        });
    let nothing_changed = words.iter().enumerate().any(|(index, word)| {
        word == "nothing"
            && !words.get(index + 1).is_some_and(|word| word == "else")
            && words
                .iter()
                .skip(index + 1)
                .take(3)
                .any(|word| matches!(word.as_str(), "change" | "changed" | "modified"))
    });
    let unchanged_file = report
        .split([';', '.', ',', '!', '?', '\n'])
        .map(normalized_report_words)
        .any(|clause| {
            clause.iter().enumerate().any(|(index, word)| {
                let scope = &clause[index.saturating_sub(6)..index];
                let file = scope
                    .iter()
                    .rposition(|word| matches!(word.as_str(), "file" | "files"));
                word == "unchanged"
                    && file.is_some_and(|file| {
                        !scope[..file]
                            .iter()
                            .rev()
                            .take(3)
                            .any(|word| is_collateral_file_qualifier(word))
                    })
            })
        });
    report_denies_file_creation(report, excepted_path)
        || no_changes
        || no_file_change
        || no_modifications_made
        || no_existential_modifications
        || verb_first_modification_denial
        || verb_first_change_denial
        || nominalized_modification_denial
        || inverted_modification_denial
        || without_modifying
        || nothing_changed
        || unchanged_file
}

pub(crate) fn words_except_named_path(words: &[String], path: &Path) -> bool {
    let path_words = normalized_report_words(&path.to_string_lossy());
    words.iter().enumerate().any(|(index, word)| {
        matches!(word.as_str(), "besides" | "except")
            && words[index + 1..]
                .windows(path_words.len())
                .any(|candidate| candidate == path_words)
    })
}

pub(crate) fn is_collateral_file_qualifier(word: &str) -> bool {
    matches!(
        word,
        "additional" | "backup" | "existing" | "other" | "preexisting"
    )
}

pub(crate) fn report_denies_file_creation(report: &str, excepted_path: Option<&Path>) -> bool {
    normalized_report_clauses(report).into_iter().any(|clause| {
        let denied_existence = clause.iter().enumerate().any(|(index, word)| {
            let collateral = clause[index.saturating_sub(6)..index]
                .iter()
                .any(|word| is_collateral_file_qualifier(word));
            let prior_state = clause[index + 1..clause.len().min(index + 5)]
                .iter()
                .any(|word| matches!(word.as_str(), "before" | "previously"));
            word == "not"
                && clause[index + 1..clause.len().min(index + 3)]
                    .iter()
                    .any(|word| matches!(word.as_str(), "exist" | "exists"))
                && !collateral
                && !prior_state
        });
        denied_existence
            || clause.iter().enumerate().any(|(index, word)| {
                let scope = &clause[index + 1..clause.len().min(index + 11)];
                let file = scope
                    .iter()
                    .position(|word| matches!(word.as_str(), "file" | "files"));
                let creation = scope.iter().position(|word| {
                    matches!(
                        word.as_str(),
                        "create"
                            | "created"
                            | "creating"
                            | "exists"
                            | "found"
                            | "generate"
                            | "generated"
                            | "generating"
                            | "write"
                            | "writing"
                            | "written"
                            | "wrote"
                    )
                });
                let collateral = scope.iter().any(|word| is_collateral_file_qualifier(word));
                let requested_path_excepted = excepted_path
                    .is_some_and(|path| words_except_named_path(&clause[index..], path));
                let no_before_file = matches!(word.as_str(), "no" | "zero")
                    && file.is_some()
                    && creation.is_some_and(|creation| file.is_some_and(|file| file < creation));
                let outcome_before_no = matches!(
                    word.as_str(),
                    "create"
                        | "created"
                        | "creating"
                        | "generate"
                        | "generated"
                        | "generating"
                        | "write"
                        | "writing"
                        | "written"
                        | "wrote"
                ) && scope
                    .iter()
                    .position(|word| word == "no")
                    .is_some_and(|no| file.is_some_and(|file| no < file));
                let without_creation = word == "without"
                    && creation.is_some_and(|creation| file.is_some_and(|file| creation < file));
                let file_state_denial = matches!(word.as_str(), "file" | "files")
                    && scope
                        .iter()
                        .take(4)
                        .position(|word| {
                            matches!(word.as_str(), "absent" | "deleted" | "missing" | "removed")
                        })
                        .is_some_and(|state| {
                            let historical = scope[..scope.len().min(state + 5)]
                                .iter()
                                .any(|word| matches!(word.as_str(), "before" | "previously"));
                            !historical
                                && !scope[..state]
                                    .iter()
                                    .any(|word| matches!(word.as_str(), "never" | "not"))
                        })
                    && !clause[index.saturating_sub(4)..index]
                        .iter()
                        .any(|word| is_collateral_file_qualifier(word));
                ((no_before_file || outcome_before_no || without_creation)
                    && !collateral
                    && !requested_path_excepted)
                    || file_state_denial
            })
    })
}

pub(crate) fn normalized_report_clauses(report: &str) -> Vec<Vec<String>> {
    normalized_report_segments(report, true)
}

pub(crate) fn normalized_report_segments(report: &str, split_commas: bool) -> Vec<Vec<String>> {
    // `str::split` predicates cannot inspect the characters adjacent to a
    // period, while `str::split_inclusive` would first fragment dotted paths
    // and require reassembling them. This bounded report scanner preserves
    // periods embedded between alphanumerics and frames only punctuation that
    // can terminate a clause.
    let mut separated = String::with_capacity(report.len());
    for (index, character) in report.char_indices() {
        let next = &report[index + character.len_utf8()..];
        let embedded_period = character == '.'
            && report[..index]
                .chars()
                .next_back()
                .is_some_and(char::is_alphanumeric)
            && next.chars().next().is_some_and(char::is_alphanumeric);
        if matches!(character, ';' | '!' | '?' | '\n')
            || (character == ',' && split_commas)
            || (character == '.' && !embedded_period)
        {
            separated.push('\n');
        } else {
            separated.push(character);
        }
    }
    separated
        .lines()
        .map(normalized_report_words)
        .filter(|clause| !clause.is_empty())
        .collect()
}

pub(crate) fn clause_denies_modifications_made(clause: &[String]) -> bool {
    clause.iter().enumerate().any(|(index, word)| {
        let ordinary = clause.get(index + 2).is_some_and(|word| {
            matches!(word.as_str(), "was" | "were")
                && clause.get(index + 3).is_some_and(|word| word == "made")
        });
        let perfect = clause.get(index + 2).is_some_and(|word| {
            matches!(word.as_str(), "has" | "have")
                && clause.get(index + 3).is_some_and(|word| word == "been")
                && clause.get(index + 4).is_some_and(|word| word == "made")
        });
        let made_index = if ordinary { index + 3 } else { index + 4 };
        let scope = &clause[clause.len().min(made_index + 1)..];
        let collateral = scope
            .iter()
            .position(|word| matches!(word.as_str(), "file" | "files"))
            .is_some_and(|file| {
                scope[..file]
                    .iter()
                    .any(|word| is_collateral_file_qualifier(word))
            });
        word == "no"
            && clause
                .get(index + 1)
                .is_some_and(|word| matches!(word.as_str(), "modification" | "modifications"))
            && (ordinary || perfect)
            && !collateral
    })
}

pub(crate) fn normalized_report_words(report: &str) -> Vec<String> {
    let normalized = report
        .to_ascii_lowercase()
        .replace("n’t", " not")
        .replace("n't", " not");
    normalized
        .split(|character: char| !character.is_alphanumeric())
        .filter(|word| !word.is_empty())
        .map(str::to_owned)
        .collect()
}

pub(crate) fn report_words_deny_success(
    report: &str,
    words: &[String],
    file_creation_required: bool,
    excepted_path: Option<&Path>,
) -> bool {
    let explicit_failure = report
        .split([';', '.', ',', '!', '?', '\n'])
        .map(normalized_report_words)
        .any(|clause| {
            clause.iter().enumerate().any(|(index, word)| {
                let failure_free_compound =
                    matches!(word.as_str(), "error" | "errors" | "failure" | "failures")
                        && clause.get(index + 1).is_some_and(|suffix| suffix == "free");
                let failure_term_negated = failure_term_is_negated(&clause, index);
                let resolved_problem =
                    matches!(word.as_str(), "issue" | "issues" | "problem" | "problems")
                        && clause[index.saturating_sub(4)..clause.len().min(index + 5)]
                            .iter()
                            .any(|word| word == "resolved");
                [
                    "cannot",
                    "error",
                    "errors",
                    "failed",
                    "failure",
                    "incomplete",
                    "issue",
                    "issues",
                    "problem",
                    "problems",
                    "unsuccessful",
                    "unsuccessfully",
                    "unable",
                ]
                .contains(&word.as_str())
                    && !resolved_problem
                    && ((!failure_term_negated && !failure_free_compound)
                        || (failure_term_negated && failure_free_compound))
            })
        });
    let negative_no_objects = [
        "answer",
        "commit",
        "completion",
        "match",
        "result",
        "success",
        "successful",
        "successfully",
    ];
    let negative_no_claim = words
        .windows(2)
        .any(|pair| pair[0] == "no" && negative_no_objects.contains(&pair[1].as_str()));
    let negative_could_not = normalized_report_clauses(report).into_iter().any(|clause| {
        clause.windows(2).enumerate().any(|(index, pair)| {
            let scope = &clause[index + 2..clause.len().min(index + 12)];
            pair[0] == "could"
                && pair[1] == "not"
                && !scope.first().is_some_and(|word| word == "only")
                && !scope_is_confinement_assurance(scope)
        })
    });
    let negative_not_able = normalized_report_clauses(report).into_iter().any(|clause| {
        clause.windows(2).enumerate().any(|(index, pair)| {
            let scope = &clause[index + 2..clause.len().min(index + 12)];
            pair[0] == "not" && pair[1] == "able" && !scope_is_confinement_assurance(scope)
        })
    });
    let negative_without_success = words.iter().enumerate().any(|(index, word)| {
        word == "without"
            && words.iter().skip(index + 1).take(3).any(|outcome| {
                matches!(outcome.as_str(), "success" | "successful" | "successfully")
            })
    });
    let negative_no_file_claim =
        file_creation_required && report_denies_file_creation(report, excepted_path);
    let requested_path_invalid_state = excepted_path.is_some_and(|path| {
        let path_words = normalized_report_words(&path.to_string_lossy());
        let clauses = normalized_report_clauses(report);
        let destructive_state = clauses.iter().enumerate().any(|(clause_index, clause)| {
            clause
                .windows(path_words.len())
                .position(|candidate| candidate == path_words)
                .is_some_and(|path_start| {
                    let path_state = &clause[path_start + path_words.len()..];
                    path_state_is_destructive(path_state)
                        || clauses.get(clause_index + 1).is_some_and(|next_clause| {
                            clause_refers_to_requested_path(next_clause, &path_words)
                                && path_state_is_destructive(next_clause)
                        })
                })
        });
        let invalid_contents = requested_path_contents_are_invalid(report, &path_words);
        destructive_state || invalid_contents
    });
    let negative_nothing_claim = words.iter().enumerate().any(|(index, word)| {
        let read_only_change_denial = words
            .iter()
            .skip(index + 1)
            .take(3)
            .any(|word| matches!(word.as_str(), "change" | "changed" | "modified"));
        word == "nothing"
            && !words.get(index + 1).is_some_and(|qualifier| {
                matches!(qualifier.as_str(), "else" | "failed" | "failure" | "other")
            })
            && !read_only_change_denial
    });
    let deferred_completion = report_has_deferred_outcome(report);
    let hedged_completion = report_hedges_outcome(report);
    let attempted_completion = report_only_attempts_outcome(report);
    let requested_completion = report_requests_outcome(report);
    let stopped_skipped_or_aborted_completion = report_stops_skips_or_aborts_outcome(report);
    let timed_out_completion = report_times_out_outcome(report);
    let canceled_completion = report_cancels_outcome(report);
    let partial_completion = report_partially_completes_outcome(report);
    let clauses_with_dotted_paths_preserved = normalized_report_clauses(report);
    let affirmative_pending = clauses_with_dotted_paths_preserved.iter().any(|clause| {
        clause.iter().enumerate().any(|(index, word)| {
            word == "pending"
                && clause[..index].last().is_some_and(|predicate| {
                    matches!(
                        predicate.as_str(),
                        "is" | "left" | "remain" | "remains" | "stays" | "still"
                    )
                })
                && !failure_term_is_negated(clause, index)
        })
    });
    let scoped_negation = clauses_with_dotted_paths_preserved
        .into_iter()
        .any(|clause| {
            clause.iter().enumerate().any(|(index, word)| {
                let scope = &clause[index + 1..clause.len().min(index + 7)];
                let outcome = scope.iter().position(|word| is_negative_outcome(word));
                let confinement_assurance = scope_is_confinement_assurance(scope);
                let affirmative_not_only =
                    word == "not" && scope.first().is_some_and(|qualifier| qualifier == "only");
                let negated_need = matches!(word.as_str(), "never" | "no" | "not")
                    && scope.first().is_some_and(|predicate| predicate == "need")
                    && scope.get(1).is_some_and(|connector| connector == "to");
                let no_is_collateral = word == "no"
                    && outcome.is_some_and(|outcome| {
                        scope[..outcome].iter().any(|word| {
                            matches!(
                                word.as_str(),
                                "additional"
                                    | "backup"
                                    | "error"
                                    | "errors"
                                    | "failure"
                                    | "failures"
                                    | "issue"
                                    | "issues"
                                    | "existing"
                                    | "other"
                                    | "preexisting"
                                    | "problem"
                                    | "problems"
                            ) || (!file_creation_required
                                && matches!(word.as_str(), "file" | "files"))
                        })
                    });
                let collateral_only = outcome.is_some_and(|outcome| {
                    let predicate_tail = &scope[outcome + 1..];
                    predicate_tail
                        .iter()
                        .position(|word| is_collateral_file_qualifier(word))
                        .is_some_and(|qualifier| {
                            predicate_tail[qualifier + 1..]
                                .iter()
                                .any(|word| matches!(word.as_str(), "file" | "files"))
                                && predicate_tail[..qualifier].iter().all(|word| {
                                    matches!(
                                        word.as_str(),
                                        "also"
                                            | "and"
                                            | "any"
                                            | "change"
                                            | "changed"
                                            | "create"
                                            | "created"
                                            | "generate"
                                            | "generated"
                                            | "modify"
                                            | "modified"
                                            | "on"
                                            | "or"
                                            | "the"
                                            | "write"
                                            | "written"
                                            | "wrote"
                                    )
                                })
                        })
                });
                let read_only_file_denial = !file_creation_required
                    && outcome.is_some_and(|outcome| {
                        let predicate_tail = &scope[outcome + 1..];
                        matches!(
                            scope[outcome].as_str(),
                            "change" | "changed" | "modify" | "modified"
                        ) && predicate_tail
                            .iter()
                            .position(|word| matches!(word.as_str(), "file" | "files"))
                            .is_some_and(|file| {
                                predicate_tail[..file]
                                    .iter()
                                    .all(|word| matches!(word.as_str(), "any" | "the"))
                            })
                    });
                let predicate_scope = &clause[index..];
                let predicate_scope = &predicate_scope[..predicate_scope
                    .iter()
                    .position(|word| {
                        matches!(
                            word.as_str(),
                            "and" | "because" | "but" | "however" | "since" | "so" | "then" | "yet"
                        )
                    })
                    .unwrap_or(predicate_scope.len())];
                let requested_path_excepted = outcome.is_some()
                    && excepted_path
                        .is_some_and(|path| words_except_named_path(predicate_scope, path));
                matches!(word.as_str(), "never" | "no" | "not" | "without")
                    && outcome.is_some()
                    && !affirmative_not_only
                    && !negated_need
                    && !no_is_collateral
                    && !collateral_only
                    && !read_only_file_denial
                    && !requested_path_excepted
                    && !confinement_assurance
            })
        });
    explicit_failure
        || negative_no_claim
        || negative_could_not
        || negative_not_able
        || negative_without_success
        || negative_no_file_claim
        || requested_path_invalid_state
        || negative_nothing_claim
        || deferred_completion
        || hedged_completion
        || attempted_completion
        || requested_completion
        || stopped_skipped_or_aborted_completion
        || timed_out_completion
        || canceled_completion
        || partial_completion
        || affirmative_pending
        || scoped_negation
}

pub(crate) fn path_state_is_destructive(path_state: &[String]) -> bool {
    path_state
        .iter()
        .take(4)
        .position(|word| matches!(word.as_str(), "deleted" | "removed"))
        .is_some_and(|state| {
            !path_state[..state]
                .iter()
                .any(|word| matches!(word.as_str(), "never" | "not"))
        })
}

pub(crate) fn report_has_deferred_outcome(report: &str) -> bool {
    normalized_report_clauses(report).into_iter().any(|clause| {
        let need_or_yet = clause.windows(3).enumerate().any(|(index, claim)| {
            let need = claim[0] == "need"
                && !failure_term_is_negated(&clause, index)
                && claim[1] == "to"
                && is_negative_outcome(&claim[2]);
            let yet = claim[0] == "yet" && claim[1] == "to" && is_negative_outcome(&claim[2]);
            need || yet
        });
        let passive = clause.windows(4).enumerate().any(|(index, claim)| {
            let need = claim[0] == "need"
                && !failure_term_is_negated(&clause, index)
                && claim[1] == "to"
                && claim[2] == "be"
                && is_negative_outcome(&claim[3]);
            let yet = claim[0] == "yet"
                && claim[1] == "to"
                && claim[2] == "be"
                && is_negative_outcome(&claim[3]);
            let remains = matches!(claim[0].as_str(), "remain" | "remains")
                && claim[1] == "to"
                && claim[2] == "be"
                && is_negative_outcome(&claim[3]);
            need || yet || remains
        });
        let future = clause
            .windows(3)
            .any(|claim| claim[0] == "will" && claim[1] == "be" && is_negative_outcome(&claim[2]))
            || clause
                .windows(2)
                .any(|claim| claim[0] == "will" && is_negative_outcome(&claim[1]));
        need_or_yet || passive || future
    })
}

pub(crate) fn report_hedges_outcome(report: &str) -> bool {
    normalized_report_clauses(report).into_iter().any(|clause| {
        clause.iter().enumerate().any(|(index, word)| {
            let scope = &clause[index + 1..clause.len().min(index + 8)];
            let uncertain = matches!(word.as_str(), "may" | "might" | "perhaps" | "possibly");
            let collateral_file_assurance = scope
                .iter()
                .position(|word| matches!(word.as_str(), "file" | "files"))
                .is_some_and(|file| {
                    scope[..file]
                        .iter()
                        .any(|word| is_collateral_file_qualifier(word))
                });
            uncertain
                && scope.iter().any(|word| is_negative_outcome(word))
                && !collateral_file_assurance
                && !scope_is_confinement_assurance(scope)
        })
    })
}

pub(crate) fn report_only_attempts_outcome(report: &str) -> bool {
    normalized_report_clauses(report).into_iter().any(|clause| {
        clause.iter().enumerate().any(|(index, word)| {
            let scope = &clause[index + 1..];
            let boundary = scope
                .iter()
                .position(|word| matches!(word.as_str(), "and" | "but" | "then"));
            let attempted_scope = &scope[..boundary.unwrap_or(scope.len())];
            let coordinated_scope = boundary.map_or(&[][..], |boundary| &scope[boundary + 1..]);
            let infinitive_outcome = attempted_scope
                .windows(2)
                .any(|claim| claim[0] == "to" && is_negative_outcome(&claim[1]));
            let gerund_outcome = attempted_scope
                .first()
                .is_some_and(|word| is_negative_outcome(word));
            is_attempt_predicate(word)
                && (infinitive_outcome || gerund_outcome)
                && !coordinated_scope_affirms_outcome(coordinated_scope)
        })
    })
}

pub(crate) fn report_requests_outcome(report: &str) -> bool {
    let clauses = normalized_report_clauses(report);
    clauses.iter().enumerate().any(|(request_index, clause)| {
        let polite_request = clause.iter().enumerate().any(|(index, word)| {
            word == "please"
                && clause[index + 1..]
                    .iter()
                    .take(5)
                    .any(|word| is_negative_outcome(word))
        });
        let imperative_run = clause.first().is_some_and(|word| word == "run")
            && clause.get(1).is_some_and(|word| {
                matches!(word.as_str(), "command" | "it" | "that" | "the" | "this")
            });
        let independent_completion = clauses.iter().enumerate().any(|(index, clause)| {
            index != request_index && coordinated_scope_affirms_outcome(clause)
        });
        (polite_request || imperative_run) && !independent_completion
    })
}

pub(crate) fn report_stops_skips_or_aborts_outcome(report: &str) -> bool {
    normalized_report_segments(report, false)
        .into_iter()
        .any(|clause| {
            clause.iter().enumerate().any(|(index, word)| {
                let scope = &clause[index + 1..];
                let boundary = scope
                    .iter()
                    .position(|word| matches!(word.as_str(), "and" | "but" | "then"));
                let coordinated_scope = boundary.map_or(&[][..], |boundary| &scope[boundary + 1..]);
                let nearby = &clause[index.saturating_sub(4)..clause.len().min(index + 5)];
                matches!(
                    word.as_str(),
                    "abort"
                        | "aborted"
                        | "aborting"
                        | "block"
                        | "blocked"
                        | "blocking"
                        | "interrupt"
                        | "interrupted"
                        | "interrupting"
                        | "skip"
                        | "skipped"
                        | "skipping"
                        | "stop"
                        | "stopped"
                        | "stopping"
                        | "terminate"
                        | "terminated"
                        | "terminating"
                        | "kill"
                        | "killed"
                        | "killing"
                        | "prevent"
                        | "prevented"
                        | "preventing"
                ) && !failure_term_is_negated(&clause, index)
                    && nearby.iter().any(|word| is_negative_outcome(word))
                    && !coordinated_scope_affirms_outcome(coordinated_scope)
            })
        })
}

pub(crate) fn report_times_out_outcome(report: &str) -> bool {
    normalized_report_segments(report, false)
        .into_iter()
        .any(|clause| {
            clause.iter().enumerate().any(|(index, word)| {
                let following = &clause[index + 1..];
                let boundary = following
                    .iter()
                    .position(|word| matches!(word.as_str(), "and" | "but" | "then"));
                let coordinated_scope =
                    boundary.map_or(&[][..], |boundary| &following[boundary + 1..]);
                let nearby = &clause[index.saturating_sub(4)..clause.len().min(index + 5)];
                let predicate_prefix = &clause[index.saturating_sub(4)..index];
                let bare_timeout_failure = matches!(word.as_str(), "timeout" | "timeouts")
                    && predicate_prefix.iter().any(|word| {
                        matches!(
                            word.as_str(),
                            "encounter"
                                | "encountered"
                                | "exceed"
                                | "exceeded"
                                | "hit"
                                | "hits"
                                | "reach"
                                | "reached"
                        )
                    });
                let timeout_predicate = bare_timeout_failure
                    || (matches!(word.as_str(), "time" | "timed" | "timing")
                        && following.first().is_some_and(|word| word == "out"));
                timeout_predicate
                    && !failure_term_is_negated(&clause, index)
                    && (bare_timeout_failure || nearby.iter().any(|word| is_negative_outcome(word)))
                    && !coordinated_scope_affirms_outcome(coordinated_scope)
            })
        })
}

pub(crate) fn report_partially_completes_outcome(report: &str) -> bool {
    normalized_report_clauses(report).into_iter().any(|clause| {
        clause.iter().enumerate().any(|(index, word)| {
            let nearby = &clause[index.saturating_sub(3)..clause.len().min(index + 4)];
            matches!(word.as_str(), "partial" | "partially")
                && nearby.iter().any(|word| is_negative_outcome(word))
        })
    })
}

pub(crate) fn report_cancels_outcome(report: &str) -> bool {
    normalized_report_segments(report, false)
        .into_iter()
        .any(|clause| {
            clause.iter().enumerate().any(|(index, word)| {
                let following = &clause[index + 1..];
                let boundary = following
                    .iter()
                    .position(|word| matches!(word.as_str(), "and" | "but" | "then"));
                let coordinated_scope =
                    boundary.map_or(&[][..], |boundary| &following[boundary + 1..]);
                let nearby = &clause[index.saturating_sub(4)..clause.len().min(index + 5)];
                matches!(word.as_str(), "cancel" | "canceled" | "cancelled")
                    && !failure_term_is_negated(&clause, index)
                    && nearby.iter().any(|word| is_negative_outcome(word))
                    && !coordinated_scope_affirms_outcome(coordinated_scope)
            })
        })
}

pub(crate) fn coordinated_scope_affirms_outcome(scope: &[String]) -> bool {
    scope.iter().enumerate().any(|(index, word)| {
        is_negative_outcome(word)
            && !failure_term_is_negated(scope, index)
            && !scope[..index].iter().any(|word| is_attempt_predicate(word))
    })
}

pub(crate) fn is_attempt_predicate(word: &str) -> bool {
    matches!(
        word,
        "attempt"
            | "attempted"
            | "attempting"
            | "prepare"
            | "prepared"
            | "preparing"
            | "tried"
            | "try"
            | "trying"
    )
}

pub(crate) fn scope_is_confinement_assurance(scope: &[String]) -> bool {
    scope.iter().any(|word| word == "outside") && scope.iter().any(|word| word == "workspace")
}

pub(crate) fn requested_path_contents_are_invalid(report: &str, path_words: &[String]) -> bool {
    let clauses = normalized_report_clauses(report);
    clauses.iter().enumerate().any(|(clause_index, clause)| {
        clause
            .windows(path_words.len())
            .enumerate()
            .any(|(path_start, candidate)| {
                if candidate != path_words {
                    return false;
                }
                let path_state = &clause[path_start + path_words.len()..];
                path_state_denies_required_contents(path_state)
                    || clauses.get(clause_index + 1).is_some_and(|next_clause| {
                        clause_refers_to_requested_path(next_clause, path_words)
                            && path_state_denies_required_contents(next_clause)
                    })
            })
    })
}

pub(crate) fn clause_refers_to_requested_path(clause: &[String], path_words: &[String]) -> bool {
    let subject = clause
        .iter()
        .skip_while(|word| matches!(word.as_str(), "and" | "but" | "however" | "then" | "yet"))
        .collect::<Vec<_>>();
    subject
        .windows(path_words.len())
        .next()
        .is_some_and(|candidate| {
            candidate
                .iter()
                .zip(path_words)
                .all(|(observed, expected)| observed.as_str() == expected)
        })
        || subject
            .first()
            .is_some_and(|word| matches!(word.as_str(), "it" | "its"))
        || matches!(
            subject.as_slice(),
            [article, file, ..]
                if article.as_str() == "the" && file.as_str() == "file"
        )
        || matches!(
            subject.as_slice(),
            [article, requested, file, ..]
                if article.as_str() == "the"
                    && requested.as_str() == "requested"
                    && file.as_str() == "file"
        )
        || matches!(
            subject.as_slice(),
            [requested, file, ..]
                if requested.as_str() == "requested" && file.as_str() == "file"
        )
}

pub(crate) fn path_state_denies_required_contents(path_state: &[String]) -> bool {
    path_state.iter().take(10).enumerate().any(|(index, word)| {
        let predicate_prefix = &path_state[index.saturating_sub(4)..index];
        let predicate_suffix = &path_state[index + 1..path_state.len().min(index + 5)];
        let negated = predicate_prefix
            .iter()
            .any(|word| matches!(word.as_str(), "never" | "not"));
        let historical = predicate_prefix
            .iter()
            .chain(predicate_suffix)
            .any(|word| matches!(word.as_str(), "before" | "initially" | "previously"))
            || predicate_prefix
                .windows(2)
                .any(|words| words[0] == "at" && words[1] == "first");
        let collateral = predicate_prefix
            .iter()
            .any(|word| is_collateral_file_qualifier(word));
        let invalid_content = matches!(
            word.as_str(),
            "empty" | "incorrect" | "mismatch" | "mismatched" | "wrong"
        );
        let zero_bytes = matches!(word.as_str(), "0" | "zero")
            && predicate_suffix
                .iter()
                .take(2)
                .any(|word| matches!(word.as_str(), "byte" | "bytes"));
        (invalid_content || zero_bytes) && !negated && !historical && !collateral
    })
}

pub(crate) fn is_negative_outcome(word: &str) -> bool {
    is_case_outcome_verb(word)
        || matches!(
            word,
            "change"
                | "changed"
                | "complete"
                | "completed"
                | "create"
                | "diff"
                | "done"
                | "edit"
                | "execute"
                | "executed"
                | "executing"
                | "fetch"
                | "find"
                | "finish"
                | "finished"
                | "found"
                | "generate"
                | "generated"
                | "generating"
                | "list"
                | "match"
                | "modify"
                | "modified"
                | "perform"
                | "performed"
                | "ran"
                | "run"
                | "running"
                | "search"
                | "stage"
                | "success"
                | "successful"
                | "successfully"
                | "succeed"
                | "succeeded"
                | "switch"
                | "work"
                | "worked"
                | "write"
        )
}

pub(crate) fn failure_term_is_negated(clause: &[String], failure_index: usize) -> bool {
    let qualifier_scope = &clause[..failure_index];
    qualifier_scope
        .iter()
        .rposition(|word| {
            matches!(
                word.as_str(),
                "never" | "no" | "not" | "nothing" | "without" | "zero"
            )
        })
        .is_some_and(|negation| {
            let without_is_reversed = qualifier_scope[negation] == "without"
                && qualifier_scope[..negation]
                    .iter()
                    .rev()
                    .take(2)
                    .any(|word| matches!(word.as_str(), "never" | "not"));
            failure_index - negation <= 5
                && !without_is_reversed
                && !qualifier_scope[negation + 1..].iter().any(|word| {
                    matches!(
                        word.as_str(),
                        "and" | "because" | "but" | "however" | "since" | "so" | "then" | "yet"
                    )
                })
        })
}

#[test]
fn eval_receipt_injection_rejects_a_preexisting_receipt() {
    let evidence = ToolExecutorEvidence::CompletedText(
        serde_json::json!({EVAL_RECEIPT_FIELD: "preexisting synthetic receipt"}).to_string(),
    );

    assert!(add_eval_receipt(evidence, SYNTHETIC_EVAL_RECEIPT).is_err());
}

pub(crate) fn synthetic_result_with_receipt() -> String {
    serde_json::json!({EVAL_RECEIPT_FIELD: SYNTHETIC_EVAL_RECEIPT}).to_string()
}

#[test]
fn observing_an_untranslated_result_does_not_count_a_round_trip() {
    let tracker = OperationTracker::default();
    tracker.observe_result(
        Uuid::from_u128(ARBITRARY_EVAL_REQUEST_ID),
        &synthetic_result_with_receipt(),
    );

    assert_eq!(tracker.result_round_trips(), 0);
    assert!(tracker.round_tripped_request_ids().is_empty());
}

#[test]
fn unrelated_model_text_does_not_count_a_result_round_trip() {
    let tracker = OperationTracker::default();
    tracker.observe_result(
        Uuid::from_u128(ARBITRARY_EVAL_REQUEST_ID),
        &synthetic_result_with_receipt(),
    );
    tracker.observe_response_text("no tool receipt reported", false);

    assert_eq!(tracker.result_round_trips(), 0);
    assert!(tracker.round_tripped_request_ids().is_empty());
}

#[test]
fn model_text_echoing_the_tool_only_receipt_counts_the_exact_request() {
    let tracker = OperationTracker::default();
    tracker.observe_result(
        Uuid::from_u128(ARBITRARY_EVAL_REQUEST_ID),
        &synthetic_result_with_receipt(),
    );
    tracker.observe_response_text(SYNTHETIC_EVAL_RECEIPT, false);

    assert_eq!(tracker.result_round_trips(), 1);
    assert_eq!(
        tracker.round_tripped_request_ids(),
        BTreeSet::from([Uuid::from_u128(ARBITRARY_EVAL_REQUEST_ID)])
    );
}

#[test]
fn intermediate_text_with_a_tool_call_does_not_count_a_result_round_trip() {
    let tracker = OperationTracker::default();
    tracker.observe_result(
        Uuid::from_u128(ARBITRARY_EVAL_REQUEST_ID),
        &synthetic_result_with_receipt(),
    );
    tracker.observe_response_text(SYNTHETIC_EVAL_RECEIPT, true);

    assert_eq!(tracker.result_round_trips(), 0);
    assert!(tracker.round_tripped_request_ids().is_empty());
}

#[test]
fn final_response_report_rejects_a_receipt_only_answer() {
    let tracker = OperationTracker::default();
    tracker.observe_result(
        Uuid::from_u128(ARBITRARY_EVAL_REQUEST_ID),
        &synthetic_result_with_receipt(),
    );
    tracker.observe_response_text(SYNTHETIC_EVAL_RECEIPT, false);

    assert!(!tracker.final_response_reports(WEB_FETCH_BODY));
    assert!(!tracker.final_response_reports_completion());
}

#[test]
fn forced_case_completion_rejects_a_receipt_only_answer() {
    let tracker = OperationTracker::default();
    tracker.observe_result(
        Uuid::from_u128(ARBITRARY_EVAL_REQUEST_ID),
        &synthetic_result_with_receipt(),
    );
    tracker.observe_response_text(SYNTHETIC_EVAL_RECEIPT, false);

    assert!(!forced_case_completion_reported(
        READ_FILE_NAME,
        true,
        &tracker
    ));
}

#[test]
fn forced_commit_completion_rejects_a_read_only_report() {
    let tracker = OperationTracker::default();
    tracker.observe_response_text(SYNTHETIC_READ_RESULT_REPORT, false);

    assert!(!forced_case_completion_reported(
        GIT_CREATE_COMMIT_NAME,
        true,
        &tracker,
    ));
}

#[test]
fn forced_diff_completion_rejects_a_negated_diff_report() {
    let tracker = OperationTracker::default();
    tracker.observe_response_text(SYNTHETIC_NEGATED_DIFF_REPORT, false);

    assert!(!forced_case_completion_reported(
        GIT_DIFF_NAME,
        true,
        &tracker,
    ));
}

#[test]
fn forced_edit_completion_rejects_a_negated_edit_report() {
    let tracker = OperationTracker::default();
    tracker.observe_response_text(SYNTHETIC_NEGATED_EDIT_REPORT, false);

    assert!(!forced_case_completion_reported(
        EDIT_FILE_NAME,
        true,
        &tracker,
    ));
}

#[test]
fn forced_read_completion_accepts_a_read_report() {
    let tracker = OperationTracker::default();
    tracker.observe_response_text(SYNTHETIC_READ_RESULT_REPORT, false);

    assert!(forced_case_completion_reported(
        READ_FILE_NAME,
        true,
        &tracker,
    ));
}

#[test]
fn final_response_report_accepts_the_fetched_fixture() {
    let tracker = OperationTracker::default();
    tracker.observe_result(
        Uuid::from_u128(ARBITRARY_EVAL_REQUEST_ID),
        &synthetic_result_with_receipt(),
    );
    let response =
        format!("{SYNTHETIC_COMPLETION_REPORT} {WEB_FETCH_BODY} {SYNTHETIC_EVAL_RECEIPT}");
    tracker.observe_response_text(&response, false);

    assert!(tracker.final_response_reports(WEB_FETCH_BODY));
    assert!(tracker.final_response_reports_completion());
}

#[test]
fn final_response_report_rejects_an_explicit_failure() {
    let tracker = OperationTracker::default();
    tracker.observe_result(
        Uuid::from_u128(ARBITRARY_EVAL_REQUEST_ID),
        &synthetic_result_with_receipt(),
    );
    tracker.observe_response_text(SYNTHETIC_FAILURE_REPORT, false);

    assert!(!tracker.final_response_reports_completion());
}

#[test]
fn final_response_report_rejects_a_failure_after_a_separate_negated_clause() {
    let tracker = OperationTracker::default();
    tracker.observe_response_text(SYNTHETIC_CROSS_CLAUSE_FAILURE_REPORT, false);

    assert!(!tracker.final_response_reports_completion());
}

#[test]
fn final_response_report_rejects_a_contracted_failure() {
    let tracker = OperationTracker::default();
    tracker.observe_result(
        Uuid::from_u128(ARBITRARY_EVAL_REQUEST_ID),
        &synthetic_result_with_receipt(),
    );
    tracker.observe_response_text(SYNTHETIC_CONTRACTED_FAILURE_REPORT, false);

    assert!(!tracker.final_response_reports_completion());
}

#[test]
fn final_response_report_rejects_never_completed() {
    let tracker = OperationTracker::default();
    tracker.observe_result(
        Uuid::from_u128(ARBITRARY_EVAL_REQUEST_ID),
        &synthetic_result_with_receipt(),
    );
    tracker.observe_response_text(SYNTHETIC_NEVER_COMPLETION_REPORT, false);

    assert!(!tracker.final_response_reports_completion());
}

#[test]
fn final_response_report_rejects_deferred_completion() {
    let tracker = OperationTracker::default();
    tracker.observe_result(
        Uuid::from_u128(ARBITRARY_EVAL_REQUEST_ID),
        &synthetic_result_with_receipt(),
    );
    tracker.observe_response_text(SYNTHETIC_DEFERRED_COMPLETION_REPORT, false);

    assert!(!tracker.final_response_reports_completion());
}

#[test]
fn final_response_report_rejects_an_outcome_remaining_to_be_completed() {
    let tracker = OperationTracker::default();
    tracker.observe_response_text(SYNTHETIC_REMAINING_COMPLETION_REPORT, false);

    assert!(!tracker.final_response_reports_completion());
}

#[test]
fn forced_read_completion_rejects_a_still_needed_report() {
    let tracker = OperationTracker::default();
    tracker.observe_response_text(SYNTHETIC_STILL_NEEDS_READ_REPORT, false);

    assert!(!forced_case_completion_reported(
        READ_FILE_NAME,
        true,
        &tracker,
    ));
}

#[test]
fn forced_read_completion_rejects_a_needed_report() {
    let tracker = OperationTracker::default();
    tracker.observe_response_text(SYNTHETIC_NEEDS_READ_REPORT, false);

    assert!(!forced_case_completion_reported(
        READ_FILE_NAME,
        true,
        &tracker,
    ));
}

#[test]
fn forced_read_completion_accepts_a_negated_need_to_repeat_the_read() {
    let tracker = OperationTracker::default();
    tracker.observe_response_text(SYNTHETIC_NO_NEED_TO_READ_AGAIN_REPORT, false);

    assert!(forced_case_completion_reported(
        READ_FILE_NAME,
        true,
        &tracker,
    ));
}

#[test]
fn forced_read_completion_rejects_a_future_report() {
    let tracker = OperationTracker::default();
    tracker.observe_response_text(SYNTHETIC_FUTURE_READ_REPORT, false);

    assert!(!forced_case_completion_reported(
        READ_FILE_NAME,
        true,
        &tracker,
    ));
}

#[test]
fn final_response_report_rejects_an_affirmative_pending_state() {
    let tracker = OperationTracker::default();
    tracker.observe_result(
        Uuid::from_u128(ARBITRARY_EVAL_REQUEST_ID),
        &synthetic_result_with_receipt(),
    );
    tracker.observe_response_text(SYNTHETIC_PENDING_COMPLETION_REPORT, false);

    assert!(!tracker.final_response_reports_completion());
}

#[test]
fn final_response_report_accepts_a_negated_pending_state() {
    let tracker = OperationTracker::default();
    tracker.observe_response_text(SYNTHETIC_NO_PENDING_COMPLETION_REPORT, false);

    assert!(tracker.final_response_reports_completion());
}

#[test]
fn final_response_report_accepts_an_applied_outcome() {
    let tracker = OperationTracker::default();
    tracker.observe_response_text(SYNTHETIC_APPLIED_COMPLETION_REPORT, false);

    assert!(tracker.final_response_reports_completion());
}

#[test]
fn final_response_report_rejects_an_unapplied_outcome() {
    let tracker = OperationTracker::default();
    tracker.observe_response_text(SYNTHETIC_NOT_APPLIED_REPORT, false);

    assert!(!tracker.final_response_reports_completion());
}

#[test]
fn final_response_report_accepts_completion_with_no_errors() {
    let tracker = OperationTracker::default();
    tracker.observe_response_text(SYNTHETIC_NO_ERRORS_COMPLETION_REPORT, false);

    assert!(tracker.final_response_reports_completion());
}

#[test]
fn final_response_report_accepts_error_free_completion() {
    let tracker = OperationTracker::default();
    tracker.observe_response_text(SYNTHETIC_ERROR_FREE_COMPLETION_REPORT, false);

    assert!(tracker.final_response_reports_completion());
}

#[test]
fn final_response_report_accepts_completion_with_zero_errors() {
    let tracker = OperationTracker::default();
    tracker.observe_response_text(SYNTHETIC_ZERO_ERRORS_COMPLETION_REPORT, false);

    assert!(tracker.final_response_reports_completion());
}

#[test]
fn final_response_report_accepts_completion_when_no_errors_were_found() {
    let tracker = OperationTracker::default();
    tracker.observe_response_text(SYNTHETIC_NO_ERRORS_FOUND_COMPLETION_REPORT, false);

    assert!(tracker.final_response_reports_completion());
}

#[test]
fn final_response_report_rejects_negated_error_free_completion() {
    let tracker = OperationTracker::default();
    tracker.observe_response_text(SYNTHETIC_NEGATED_ERROR_FREE_REPORT, false);

    assert!(!tracker.final_response_reports_completion());
}

#[test]
fn final_response_report_rejects_a_clause_scoped_no_operation_claim() {
    let tracker = OperationTracker::default();
    tracker.observe_response_text(SYNTHETIC_NO_OPERATION_COMPLETION_REPORT, false);

    assert!(!tracker.final_response_reports_completion());
}

#[test]
fn final_response_report_accepts_longer_negated_failure_phrases() {
    let tracker = OperationTracker::default();
    tracker.observe_response_text(SYNTHETIC_LONG_NEGATED_ERRORS_COMPLETION_REPORT, false);

    assert!(tracker.final_response_reports_completion());
}

#[test]
fn final_response_report_rejects_a_failure_after_longer_negation() {
    let tracker = OperationTracker::default();
    tracker.observe_response_text(SYNTHETIC_NEGATED_ERRORS_THEN_FAILURE_REPORT, false);

    assert!(!tracker.final_response_reports_completion());
}

#[test]
fn final_response_report_rejects_a_failure_after_a_causal_boundary() {
    let tracker = OperationTracker::default();
    tracker.observe_response_text(SYNTHETIC_CAUSAL_FAILURE_REPORT, false);

    assert!(!tracker.final_response_reports_completion());
}

#[test]
fn final_response_report_rejects_completion_with_errors() {
    let tracker = OperationTracker::default();
    tracker.observe_response_text(SYNTHETIC_ERRORS_COMPLETION_REPORT, false);

    assert!(!tracker.final_response_reports_completion());
}

#[test]
fn final_response_report_accepts_completion_without_failure() {
    let tracker = OperationTracker::default();
    tracker.observe_response_text(SYNTHETIC_WITHOUT_FAILURE_COMPLETION_REPORT, false);

    assert!(tracker.final_response_reports_completion());
}

#[test]
fn final_response_report_accepts_completion_after_no_failure() {
    let tracker = OperationTracker::default();
    tracker.observe_response_text(SYNTHETIC_NO_FAILURE_COMPLETION_REPORT, false);

    assert!(tracker.final_response_reports_completion());
}

#[test]
fn final_response_report_accepts_completion_after_nothing_failed() {
    let tracker = OperationTracker::default();
    tracker.observe_response_text(SYNTHETIC_NOTHING_FAILED_COMPLETION_REPORT, false);

    assert!(tracker.final_response_reports_completion());
}

#[test]
fn final_response_report_rejects_completion_that_was_not_successful() {
    let tracker = OperationTracker::default();
    tracker.observe_response_text(SYNTHETIC_NOT_SUCCESSFUL_COMPLETION_REPORT, false);

    assert!(!tracker.final_response_reports_completion());
}

#[test]
fn final_response_report_rejects_completion_when_the_operation_did_not_succeed() {
    let tracker = OperationTracker::default();
    tracker.observe_response_text(SYNTHETIC_DID_NOT_SUCCEED_REPORT, false);

    assert!(!tracker.final_response_reports_completion());
}

#[test]
fn final_response_report_rejects_completion_when_the_operation_never_succeeded() {
    let tracker = OperationTracker::default();
    tracker.observe_response_text(SYNTHETIC_NEVER_SUCCEEDED_REPORT, false);

    assert!(!tracker.final_response_reports_completion());
}

#[test]
fn final_response_report_rejects_completion_when_the_model_was_not_able() {
    let tracker = OperationTracker::default();
    tracker.observe_response_text(SYNTHETIC_NOT_ABLE_REPORT, false);

    assert!(!tracker.final_response_reports_completion());
}

#[test]
fn final_response_report_accepts_a_not_able_confinement_assurance() {
    let tracker = OperationTracker::default();
    tracker.observe_response_text(SYNTHETIC_NOT_ABLE_CONFINEMENT_REPORT, false);

    assert!(tracker.final_response_reports_completion());
}

#[test]
fn final_response_report_rejects_completion_when_the_operation_did_not_work() {
    let tracker = OperationTracker::default();
    tracker.observe_response_text(SYNTHETIC_DID_NOT_WORK_REPORT, false);

    assert!(!tracker.final_response_reports_completion());
}

#[test]
fn final_response_report_rejects_completion_when_the_operation_was_not_performed() {
    let tracker = OperationTracker::default();
    tracker.observe_response_text(SYNTHETIC_NOT_PERFORMED_REPORT, false);

    assert!(!tracker.final_response_reports_completion());
}

#[test]
fn final_response_report_rejects_completion_without_running_the_command() {
    let tracker = OperationTracker::default();
    tracker.observe_response_text(SYNTHETIC_COMMAND_NOT_RUN_REPORT, false);

    assert!(!tracker.final_response_reports_completion());
}

#[test]
fn final_response_report_accepts_a_resolved_problem() {
    let tracker = OperationTracker::default();
    tracker.observe_response_text(SYNTHETIC_RESOLVED_PROBLEM_REPORT, false);

    assert!(tracker.final_response_reports_completion());
}

#[test]
fn final_response_report_accepts_a_collateral_did_not_work_claim() {
    let tracker = OperationTracker::default();
    tracker.observe_response_text(SYNTHETIC_COLLATERAL_DID_NOT_WORK_REPORT, false);

    assert!(tracker.final_response_reports_completion());
}

#[test]
fn final_response_report_rejects_completion_that_was_not_without_errors() {
    let tracker = OperationTracker::default();
    tracker.observe_response_text(SYNTHETIC_NOT_WITHOUT_ERRORS_REPORT, false);

    assert!(!tracker.final_response_reports_completion());
}

#[test]
fn final_response_report_rejects_completion_with_no_success() {
    let tracker = OperationTracker::default();
    tracker.observe_response_text(SYNTHETIC_NO_SUCCESS_COMPLETION_REPORT, false);

    assert!(!tracker.final_response_reports_completion());
}

#[test]
fn final_response_report_rejects_completion_without_success() {
    let tracker = OperationTracker::default();
    tracker.observe_response_text(SYNTHETIC_WITHOUT_SUCCESS_COMPLETION_REPORT, false);

    assert!(!tracker.final_response_reports_completion());
}

#[test]
fn final_response_report_rejects_an_unsuccessful_completion() {
    let tracker = OperationTracker::default();
    tracker.observe_response_text(SYNTHETIC_UNSUCCESSFUL_COMPLETION_REPORT, false);

    assert!(!tracker.final_response_reports_completion());
}

#[test]
fn final_response_report_rejects_not_successfully() {
    let tracker = OperationTracker::default();
    tracker.observe_response_text(SYNTHETIC_NOT_SUCCESSFULLY_REPORT, false);

    assert!(!tracker.final_response_reports_completion());
}

#[test]
fn final_response_report_rejects_could_not_complete() {
    let tracker = OperationTracker::default();
    tracker.observe_response_text(SYNTHETIC_COULD_NOT_COMPLETE_REPORT, false);

    assert!(!tracker.final_response_reports_completion());
}

#[test]
fn final_response_report_accepts_completion_with_no_file_changes() {
    let tracker = OperationTracker::default();
    tracker.observe_response_text(SYNTHETIC_NO_FILE_CHANGES_COMPLETION_REPORT, false);

    assert!(tracker.final_response_reports_completion());
}

#[test]
fn final_response_report_accepts_a_read_only_modification_denial() {
    let tracker = OperationTracker::default();
    tracker.observe_response_text(SYNTHETIC_VERB_FIRST_MODIFICATION_DENIAL_REPORT, false);

    assert!(tracker.final_response_reports_completion());
}

#[test]
fn final_response_report_accepts_completion_with_bare_no_changes() {
    let tracker = OperationTracker::default();
    tracker.observe_response_text(SYNTHETIC_BARE_NO_CHANGES_DENIAL_REPORT, false);

    assert!(tracker.final_response_reports_completion());
}

#[test]
fn forced_edit_report_rejects_completion_with_no_file_changes() {
    let tracker = OperationTracker::default();
    tracker.observe_response_text(SYNTHETIC_NO_FILE_CHANGES_COMPLETION_REPORT, false);

    assert!(!forced_case_completion_reported(
        EDIT_FILE_NAME,
        true,
        &tracker,
    ));
}

#[test]
fn forced_edit_report_rejects_a_file_not_modified_denial() {
    let tracker = OperationTracker::default();
    tracker.observe_response_text(SYNTHETIC_FILE_NOT_MODIFIED_REPORT, false);

    assert!(!forced_case_completion_reported(
        EDIT_FILE_NAME,
        true,
        &tracker,
    ));
}

#[test]
fn forced_edit_report_accepts_a_collateral_no_file_changes_claim() {
    let tracker = OperationTracker::default();
    tracker.observe_response_text(
        SYNTHETIC_COLLATERAL_NO_FILE_CHANGES_COMPLETION_REPORT,
        false,
    );

    assert!(forced_case_completion_reported(
        EDIT_FILE_NAME,
        true,
        &tracker,
    ));
}

#[test]
fn final_response_report_accepts_completion_with_scoped_negation() {
    let tracker = OperationTracker::default();
    tracker.observe_response_text(SYNTHETIC_SCOPED_NEGATION_COMPLETION_REPORT, false);

    assert!(tracker.final_response_reports_completion());
}

#[test]
fn final_response_report_accepts_completion_with_scoped_creation_negation() {
    let tracker = OperationTracker::default();
    tracker.observe_response_text(SYNTHETIC_SCOPED_CREATION_NEGATION_COMPLETION_REPORT, false);

    assert!(tracker.final_response_reports_completion());
}

#[test]
fn final_response_report_accepts_completion_with_a_collateral_conjunction() {
    let tracker = OperationTracker::default();
    tracker.observe_response_text(
        SYNTHETIC_SCOPED_CONJUNCTION_NEGATION_COMPLETION_REPORT,
        false,
    );

    assert!(tracker.final_response_reports_completion());
}

#[test]
fn final_response_report_accepts_an_affirmative_not_only_construction() {
    let tracker = OperationTracker::default();
    tracker.observe_response_text(SYNTHETIC_NOT_ONLY_COMPLETION_REPORT, false);

    assert!(tracker.final_response_reports_completion());
}

#[test]
fn final_response_report_rejects_a_denial_before_a_collateral_clause() {
    let tracker = OperationTracker::default();
    tracker.observe_response_text(SYNTHETIC_SEPARATE_COLLATERAL_CLAUSE_DENIAL_REPORT, false);

    assert!(!tracker.final_response_reports_completion());
}

#[test]
fn final_response_report_rejects_a_no_file_written_claim() {
    let tracker = OperationTracker::default();
    tracker.observe_response_text(SYNTHETIC_NO_FILE_WRITTEN_REPORT, false);

    assert!(!tracker.final_response_reports_completion_with_file_creation());
}

#[test]
fn final_response_report_rejects_a_no_files_written_claim() {
    let tracker = OperationTracker::default();
    tracker.observe_response_text(SYNTHETIC_NO_FILES_WRITTEN_REPORT, false);

    assert!(!tracker.final_response_reports_completion_with_file_creation());
}

#[test]
fn effect_free_final_response_accepts_a_file_creation_denial() {
    let tracker = OperationTracker::default();
    tracker.observe_response_text(SYNTHETIC_EFFECT_FREE_NO_FILE_CREATED_REPORT, false);

    assert!(tracker.final_response_reports_completion());
}

#[test]
fn final_response_report_accepts_an_affirmative_read() {
    let tracker = OperationTracker::default();
    tracker.observe_response_text(SYNTHETIC_READ_COMPLETION_REPORT, false);

    assert!(tracker.final_response_reports_completion());
}

#[test]
fn final_response_report_accepts_an_affirmative_branch_switch() {
    let tracker = OperationTracker::default();
    tracker.observe_response_text(SYNTHETIC_SWITCH_COMPLETION_REPORT, false);

    assert!(tracker.final_response_reports_completion());
}

#[test]
fn final_response_report_rejects_a_nothing_written_claim() {
    let tracker = OperationTracker::default();
    tracker.observe_response_text(SYNTHETIC_NOTHING_WRITTEN_REPORT, false);

    assert!(!tracker.final_response_reports_completion());
}

pub(crate) fn completed_tool_result_entry_indices(
    entries: &[ProcessTranscriptEntry],
) -> BTreeMap<Uuid, u64> {
    entries
        .iter()
        .filter_map(|entry| match entry {
            ProcessTranscriptEntry::ToolExecutionResult {
                entry_index,
                request,
                disposition: ProcessToolExecutionResultDisposition::Completed,
                ..
            } => Some((request.into_uuid(), *entry_index)),
            ProcessTranscriptEntry::ToolExecutionResult {
                disposition: ProcessToolExecutionResultDisposition::KnownFailed,
                ..
            }
            | ProcessTranscriptEntry::DelegatedTask { .. }
            | ProcessTranscriptEntry::DelegationMessage { .. }
            | ProcessTranscriptEntry::DelegationResult { .. }
            | ProcessTranscriptEntry::ModelIdentityChanged { .. }
            | ProcessTranscriptEntry::ContextSummary { .. }
            | ProcessTranscriptEntry::User { .. }
            | ProcessTranscriptEntry::Assistant { .. }
            | ProcessTranscriptEntry::ProviderCompaction { .. }
            | ProcessTranscriptEntry::ProviderReasoning { .. }
            | ProcessTranscriptEntry::AssistantToolUse { .. }
            | ProcessTranscriptEntry::ToolDenied { .. }
            | ProcessTranscriptEntry::ToolClosed { .. }
            | ProcessTranscriptEntry::TurnFailed { .. }
            | ProcessTranscriptEntry::TurnCompleted { .. }
            | ProcessTranscriptEntry::TurnCancelled { .. }
            | ProcessTranscriptEntry::ImportedText { .. }
            | ProcessTranscriptEntry::Imported { .. } => None,
        })
        .collect()
}

pub(crate) fn successful_tool_requests(entries: &[ProcessTranscriptEntry]) -> BTreeSet<Uuid> {
    completed_tool_result_entry_indices(entries)
        .into_keys()
        .collect()
}

pub(crate) fn round_tripped_result_count(results: &[TrackedToolResult]) -> usize {
    results.iter().filter(|result| result.round_tripped).count()
}

pub(crate) fn reject_credential_rejections(report: &FamilyReport) -> EvalResult {
    let forced_rejected = report.forced.iter().any(|outcome| {
        matches!(
            outcome.snapshot.turn_disposition,
            SnapshotTurnDisposition::ProviderFailure(Some(
                ProcessProviderModelCallFailureCause::CredentialRejected
            ))
        )
    });
    let natural_rejected = matches!(
        report.natural.snapshot.turn_disposition,
        SnapshotTurnDisposition::ProviderFailure(Some(
            ProcessProviderModelCallFailureCause::CredentialRejected
        ))
    );
    if forced_rejected || natural_rejected {
        return Err(io::Error::other(CREDENTIAL_REJECTION_FAILURE).into());
    }
    Ok(())
}

pub(crate) fn synthetic_tool_result(
    disposition: ProcessToolExecutionResultDisposition,
) -> ProcessTranscriptEntry {
    ProcessTranscriptEntry::ToolExecutionResult {
        entry_index: 0,
        source_session: SessionId::from_uuid(Uuid::from_u128(ARBITRARY_EVAL_SESSION_ID)),
        entry: SemanticTranscriptEntryId::from_uuid(Uuid::from_u128(ARBITRARY_EVAL_ENTRY_ID)),
        request: ToolRequestId::from_uuid(Uuid::from_u128(ARBITRARY_EVAL_REQUEST_ID)),
        attempt: ToolAttemptId::from_uuid(Uuid::from_u128(ARBITRARY_EVAL_ATTEMPT_ID)),
        disposition,
        content: String::from("synthetic tool result"),
    }
}

#[test]
fn successful_tool_requests_accepts_a_typed_completed_result() {
    let entries = [synthetic_tool_result(
        ProcessToolExecutionResultDisposition::Completed,
    )];

    assert_eq!(
        successful_tool_requests(&entries),
        BTreeSet::from([Uuid::from_u128(ARBITRARY_EVAL_REQUEST_ID)])
    );
}

#[test]
fn successful_tool_requests_rejects_a_typed_known_failure() {
    let entries = [synthetic_tool_result(
        ProcessToolExecutionResultDisposition::KnownFailed,
    )];

    assert!(successful_tool_requests(&entries).is_empty());
}

#[test]
fn turn_snapshot_reports_ambiguous_model_recovery_as_infrastructure() {
    let state = ProcessTurnState::ActiveAwaitingModelCallRecovery {
        ended_attempt: TurnAttemptId::from_uuid(Uuid::from_u128(ARBITRARY_EVAL_TURN_ATTEMPT_ID)),
        recovery_call: ModelCallId::from_uuid(Uuid::from_u128(ARBITRARY_EVAL_MODEL_CALL_ID)),
        automatic_reconciliation_attempts: 0,
        operator_action_required: false,
    };

    assert_eq!(
        SnapshotTurnDisposition::from_process_state(&state),
        SnapshotTurnDisposition::Infrastructure
    );
}

#[test]
fn turn_snapshot_reports_runner_recovery_as_infrastructure() {
    let state = ProcessTurnState::ActiveAwaitingRunnerRecovery {
        runner: RunnerId::from_uuid(Uuid::from_u128(ARBITRARY_EVAL_SESSION_ID)),
        placement_revision: RunnerGeneration::one(),
        interrupted_tool_attempt: None,
    };

    assert_eq!(
        SnapshotTurnDisposition::from_process_state(&state),
        SnapshotTurnDisposition::Infrastructure
    );
}

#[test]
fn turn_snapshot_reports_target_resolution_failure_as_infrastructure() {
    let state = ProcessTurnState::Failed {
        terminal_frontier: ContextFrontierId::from_uuid(Uuid::from_u128(
            ARBITRARY_EVAL_FRONTIER_ID,
        )),
        terminal_attempt: None,
        terminal_model_call: None,
    };

    assert_eq!(
        SnapshotTurnDisposition::from_process_state(&state),
        SnapshotTurnDisposition::Infrastructure
    );
}

#[test]
fn turn_snapshot_reports_parked_tool_approval_as_infrastructure() {
    let state = ProcessTurnState::ActiveAwaitingToolApproval {
        request: ToolRequestId::from_uuid(Uuid::from_u128(ARBITRARY_EVAL_REQUEST_ID)),
    };

    assert_eq!(
        SnapshotTurnDisposition::from_process_state(&state),
        SnapshotTurnDisposition::Infrastructure
    );
}

#[test]
fn turn_snapshot_reports_a_terminal_provider_failure_distinctly() {
    assert_eq!(
        SnapshotTurnDisposition::from_failed_model_call(Some((
            ProcessFailedModelCallDisposition::KnownFailed,
            None
        ))),
        SnapshotTurnDisposition::ProviderFailure(None)
    );
}

#[test]
fn turn_snapshot_reports_failed_without_a_model_call_as_infrastructure() {
    assert_eq!(
        SnapshotTurnDisposition::from_failed_model_call(None),
        SnapshotTurnDisposition::Infrastructure
    );
}

#[test]
fn turn_snapshot_retains_the_closed_provider_failure_cause() {
    assert_eq!(
        SnapshotTurnDisposition::from_failed_model_call(Some((
            ProcessFailedModelCallDisposition::KnownFailed,
            Some(ProcessProviderModelCallFailureCause::TargetNotFound)
        ))),
        SnapshotTurnDisposition::ProviderFailure(Some(
            ProcessProviderModelCallFailureCause::TargetNotFound
        ))
    );
}

#[test]
fn a_cancelled_terminal_model_call_reports_infrastructure() {
    assert_eq!(
        SnapshotTurnDisposition::from_failed_model_call(Some((
            ProcessFailedModelCallDisposition::Cancelled,
            None
        ))),
        SnapshotTurnDisposition::Infrastructure
    );
}

#[test]
fn the_turn_cell_names_the_closed_provider_failure_cause() {
    let disposition = SnapshotTurnDisposition::ProviderFailure(Some(
        ProcessProviderModelCallFailureCause::TargetNotFound,
    ));

    assert_eq!(disposition.label(), "provider failure: target not found");
}

#[test]
fn the_turn_cell_of_an_unclassified_provider_failure_stays_bare() {
    let disposition = SnapshotTurnDisposition::ProviderFailure(None);

    assert_eq!(disposition.label(), "provider failure");
}

#[test]
fn provider_failure_is_reported_as_infrastructure_not_a_model_miss() {
    let outcome = CaseOutcome {
        target: Some(String::from(GIT_STATUS_NAME)),
        expected_arguments: Some(String::from("{}")),
        execution_completed: true,
        forced_verification_failed: false,
        tool_results: Vec::new(),
        snapshot: CaseSnapshot {
            turn_disposition: SnapshotTurnDisposition::ProviderFailure(Some(
                ProcessProviderModelCallFailureCause::TargetNotFound,
            )),
            requests: Vec::new(),
            model_calls: 1,
        },
    };

    assert_eq!(
        outcome.forced_disposition(),
        EvalDisposition::Infrastructure
    );
    assert_eq!(
        outcome.natural_loop_disposition(EvalFamily::Git),
        EvalDisposition::Infrastructure
    );
}

pub(crate) fn synthetic_case_outcome(turn_disposition: SnapshotTurnDisposition) -> CaseOutcome {
    CaseOutcome {
        target: None,
        expected_arguments: None,
        execution_completed: false,
        forced_verification_failed: false,
        tool_results: Vec::new(),
        snapshot: CaseSnapshot {
            turn_disposition,
            requests: Vec::new(),
            model_calls: 1,
        },
    }
}

#[test]
fn forced_credential_rejection_fails_the_job() {
    let report = FamilyReport {
        family: EvalFamily::Git,
        forced: vec![synthetic_case_outcome(
            SnapshotTurnDisposition::ProviderFailure(Some(
                ProcessProviderModelCallFailureCause::CredentialRejected,
            )),
        )],
        natural: synthetic_case_outcome(SnapshotTurnDisposition::Completed),
        natural_state: EvalDisposition::Miss,
    };

    assert_eq!(
        reject_credential_rejections(&report)
            .expect_err("the rejected forced credential fails the job")
            .to_string(),
        CREDENTIAL_REJECTION_FAILURE
    );
}

#[test]
fn natural_credential_rejection_fails_the_job() {
    let report = FamilyReport {
        family: EvalFamily::Git,
        forced: Vec::new(),
        natural: synthetic_case_outcome(SnapshotTurnDisposition::ProviderFailure(Some(
            ProcessProviderModelCallFailureCause::CredentialRejected,
        ))),
        natural_state: EvalDisposition::Infrastructure,
    };

    assert_eq!(
        reject_credential_rejections(&report)
            .expect_err("the rejected natural credential fails the job")
            .to_string(),
        CREDENTIAL_REJECTION_FAILURE
    );
}

#[test]
fn forced_tier_passes_one_completed_target_with_a_result_round_trip() {
    let target = GIT_STATUS_NAME;
    let outcome = CaseOutcome {
        target: Some(String::from(target)),
        expected_arguments: Some(String::from("{}")),
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
                name: String::from(target),
                arguments_text: String::from("{}"),
                entry_index: ARBITRARY_REQUEST_ENTRY_INDEX,
                completed_result_entry_index: Some(ARBITRARY_COMPLETED_RESULT_ENTRY_INDEX),
                attempt_succeeded: true,
                attempt_denied: false,
            }],
            model_calls: MINIMUM_MODEL_CALLS_FOR_RESULT_ROUND_TRIP,
        },
    };

    assert_eq!(outcome.forced_disposition(), EvalDisposition::Pass);
}

#[test]
fn forced_tier_reports_a_miss_without_result_round_trip() {
    let target = GIT_STATUS_NAME;
    let outcome = CaseOutcome {
        target: Some(String::from(target)),
        expected_arguments: Some(String::from("{}")),
        execution_completed: true,
        forced_verification_failed: false,
        tool_results: Vec::new(),
        snapshot: CaseSnapshot {
            turn_disposition: SnapshotTurnDisposition::Completed,
            requests: vec![RequestSnapshot {
                request_id: Uuid::from_u128(ARBITRARY_EVAL_REQUEST_ID),
                producing_model_call_id: Uuid::from_u128(ARBITRARY_EVAL_MODEL_CALL_ID),
                name: String::from(target),
                arguments_text: String::from("{}"),
                entry_index: ARBITRARY_REQUEST_ENTRY_INDEX,
                completed_result_entry_index: Some(ARBITRARY_COMPLETED_RESULT_ENTRY_INDEX),
                attempt_succeeded: true,
                attempt_denied: false,
            }],
            model_calls: MINIMUM_MODEL_CALLS_FOR_RESULT_ROUND_TRIP,
        },
    };

    assert_eq!(outcome.forced_disposition(), EvalDisposition::Miss);
}

#[test]
fn forced_tier_reports_and_rejects_an_exact_known_failed_attempt() {
    let target = GIT_STATUS_NAME;
    let outcome = CaseOutcome {
        target: Some(String::from(target)),
        expected_arguments: Some(String::from("{}")),
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
                name: String::from(target),
                arguments_text: String::from("{}"),
                entry_index: ARBITRARY_REQUEST_ENTRY_INDEX,
                completed_result_entry_index: None,
                attempt_succeeded: false,
                attempt_denied: false,
            }],
            model_calls: MINIMUM_MODEL_CALLS_FOR_RESULT_ROUND_TRIP,
        },
    };

    assert_eq!(
        outcome.forced_disposition(),
        EvalDisposition::Infrastructure
    );
    assert!(reject_forced_executor_failures(&[outcome]).is_err());
}

#[test]
fn forced_tier_rejects_an_exact_failure_before_a_follow_up_call() {
    let target = GIT_STATUS_NAME;
    let outcome = CaseOutcome {
        target: Some(String::from(target)),
        expected_arguments: Some(String::from("{}")),
        execution_completed: false,
        forced_verification_failed: false,
        tool_results: Vec::new(),
        snapshot: CaseSnapshot {
            turn_disposition: SnapshotTurnDisposition::Completed,
            requests: vec![
                RequestSnapshot {
                    request_id: Uuid::from_u128(ARBITRARY_EVAL_REQUEST_ID),
                    producing_model_call_id: Uuid::from_u128(ARBITRARY_EVAL_MODEL_CALL_ID),
                    name: String::from(target),
                    arguments_text: String::from("{}"),
                    entry_index: ARBITRARY_REQUEST_ENTRY_INDEX,
                    completed_result_entry_index: None,
                    attempt_succeeded: false,
                    attempt_denied: false,
                },
                RequestSnapshot {
                    request_id: Uuid::from_u128(ARBITRARY_FOLLOW_UP_REQUEST_ID),
                    producing_model_call_id: Uuid::from_u128(ARBITRARY_EVAL_MODEL_CALL_ID),
                    name: String::from(GIT_LOG_NAME),
                    arguments_text: String::from("{}"),
                    entry_index: ARBITRARY_LATE_RESULT_ENTRY_INDEX,
                    completed_result_entry_index: None,
                    attempt_succeeded: false,
                    attempt_denied: false,
                },
            ],
            model_calls: MINIMUM_MODEL_CALLS_FOR_RESULT_ROUND_TRIP,
        },
    };

    assert_eq!(
        outcome.forced_disposition(),
        EvalDisposition::Infrastructure
    );
    assert!(reject_forced_executor_failures(&[outcome]).is_err());
}

#[test]
fn forced_case_validation_rejects_schema_drift() -> EvalResult {
    let suite = FamilySuite::git()?;
    let drifted = ForcedCase {
        name: GIT_STATUS_NAME,
        expected_arguments: r#"{"unexpected":true}"#,
        prompt: "synthetic invalid forced case",
    };

    assert!(suite.validate_forced_case(&drifted).is_err());
    Ok(())
}

#[test]
fn forced_case_inventory_matches_each_catalog_available_offline() -> EvalResult {
    let git = FamilySuite::git()?;
    let workspace = FamilySuite::workspace()?;
    let web = FamilySuite::web()?;

    git.validate_forced_inventory()?;
    workspace.validate_forced_inventory()?;
    web.validate_forced_inventory()?;
    Ok(())
}

#[cfg(unix)]
#[test]
fn identity_gate_without_change_time_rejects_changed_ownership() -> EvalResult {
    let suite = FamilySuite::workspace()?;
    let expected = suite.workspace_seed_entry_identities[Path::new("")];
    let mut actual = expected;
    actual.group_id = expected.group_id.wrapping_add(1);

    assert!(!filesystem_identity_matches_without_change_time(
        Some(actual),
        Some(expected),
    ));
    Ok(())
}

#[test]
fn forced_tier_reports_a_miss_for_drifted_arguments() {
    let target = GIT_STATUS_NAME;
    let outcome = CaseOutcome {
        target: Some(String::from(target)),
        expected_arguments: Some(String::from("{}")),
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
                name: String::from(target),
                arguments_text: String::from(r#"{"unexpected":true}"#),
                entry_index: ARBITRARY_REQUEST_ENTRY_INDEX,
                completed_result_entry_index: Some(ARBITRARY_COMPLETED_RESULT_ENTRY_INDEX),
                attempt_succeeded: true,
                attempt_denied: false,
            }],
            model_calls: MINIMUM_MODEL_CALLS_FOR_RESULT_ROUND_TRIP,
        },
    };

    assert_eq!(outcome.forced_disposition(), EvalDisposition::Miss);
}

#[test]
fn forced_tool_sequence_allows_only_one_forced_exchange() {
    let sequence = ForcedToolSequence::new(Some(SANDBOXED_EXEC_NAME));

    assert_eq!(
        sequence.next(),
        ForcedToolOperation::Force(RuntimeToolName::new(SANDBOXED_EXEC_NAME))
    );
    assert_eq!(sequence.next(), ForcedToolOperation::Continuation);
}

#[test]
fn operation_tracker_records_each_cumulative_tool_result_once() {
    let tracker = OperationTracker::default();
    let tool_call_id = String::from("synthetic-tool-call");
    let content = synthetic_result_with_receipt();
    let result = TrackedToolResult {
        request_id: Uuid::from_u128(ARBITRARY_EVAL_REQUEST_ID),
        content: content.clone(),
        is_error: false,
        round_tripped: false,
    };
    tracker.record_new_results([(tool_call_id.clone(), result.clone())]);
    tracker.record_new_results([(tool_call_id, result.clone())]);

    assert_eq!(tracker.tool_results(), vec![result]);
    assert_eq!(
        tracker.result_content(Uuid::from_u128(ARBITRARY_EVAL_REQUEST_ID)),
        Some(content)
    );
}

#[test]
fn report_round_trip_count_excludes_an_unacknowledged_result() {
    let results = [
        TrackedToolResult {
            request_id: Uuid::from_u128(ARBITRARY_EVAL_REQUEST_ID),
            content: String::from("acknowledged"),
            is_error: false,
            round_tripped: true,
        },
        TrackedToolResult {
            request_id: Uuid::from_u128(ARBITRARY_SECOND_EVAL_REQUEST_ID),
            content: String::from("unacknowledged"),
            is_error: false,
            round_tripped: false,
        },
    ];

    assert_eq!(round_tripped_result_count(&results), 1);
}

#[test]
fn natural_tool_sequence_bounds_tool_enabled_exchanges() {
    let sequence = ForcedToolSequence::new(None);

    assert_eq!(sequence.next(), ForcedToolOperation::Natural);
    assert_eq!(sequence.next(), ForcedToolOperation::Natural);
    assert_eq!(sequence.next(), ForcedToolOperation::Natural);
    assert_eq!(sequence.next(), ForcedToolOperation::Continuation);
}

pub(crate) struct FamilyReport {
    pub(crate) family: EvalFamily,
    pub(crate) forced: Vec<CaseOutcome>,
    pub(crate) natural: CaseOutcome,
    pub(crate) natural_state: EvalDisposition,
}
